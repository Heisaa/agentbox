use std::{
    collections::BTreeMap,
    env,
    ffi::{OsStr, OsString},
    fs,
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result};
use tempfile::NamedTempFile;

use crate::{
    compose::ComposeProject,
    config::{Config, HomeMode, NetworkMode},
};

#[derive(Debug)]
pub struct RunSpec {
    pub args: Vec<OsString>,
    pub network: String,
    pub environment: BTreeMap<String, String>,
    pub workspace_write: bool,
    pub uid_gid: String,
    pub imported_credentials: Option<String>,
    runtime_image: String,
    cargo_cache: bool,
    credential_file: Option<NamedTempFile>,
    claude_state_file: Option<NamedTempFile>,
}

#[derive(Debug, Clone)]
pub struct ImageBuildSpec {
    pub args: Vec<OsString>,
    pub image: String,
}

pub struct BuildInput<'a> {
    pub config: &'a Config,
    pub repo_root: &'a Path,
    pub workspace: &'a Path,
    pub host_home: Option<&'a Path>,
    pub compose: Option<&'a ComposeProject>,
    pub environment: BTreeMap<String, String>,
    pub command: Vec<String>,
    pub interactive: bool,
    pub session_id: Option<&'a str>,
}

pub fn build_run_spec(input: BuildInput<'_>) -> Result<RunSpec> {
    let config = input.config;
    let slug = crate::project::project_slug(input.repo_root);
    let uid_gid = host_uid_gid()?;
    let mut command = input.command.clone();
    let codex_account = command
        .first()
        .map(String::as_str)
        .map(codex_account_from_command_name)
        .transpose()?
        .flatten()
        .map(str::to_owned);
    if codex_account.is_some() {
        command[0] = "codex".into();
    }
    let mut args = vec![OsString::from("run"), OsString::from("--rm")];

    if input.interactive {
        args.push(OsString::from("-i"));
        if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            args.push(OsString::from("-t"));
        }
    }

    let container_name = container_name(input.repo_root, input.session_id);
    args.extend([
        OsString::from("--name"),
        OsString::from(container_name),
        OsString::from("--user"),
        OsString::from(&uid_gid),
        OsString::from("--workdir"),
        OsString::from(&config.workspace.container_path),
        OsString::from("--memory"),
        OsString::from(&config.limits.memory),
        OsString::from("--cpus"),
        OsString::from(config.limits.cpus.to_string()),
        OsString::from("--pids-limit"),
        OsString::from(config.limits.pids.to_string()),
        OsString::from("--security-opt"),
        OsString::from("no-new-privileges"),
        OsString::from("--cap-drop"),
        OsString::from("ALL"),
    ]);
    if let Some(session_id) = input.session_id {
        args.extend([
            OsString::from("--label"),
            OsString::from("agentbox.managed=true"),
            OsString::from("--label"),
            OsString::from(format!("agentbox.repo={}", input.repo_root.display())),
            OsString::from("--label"),
            OsString::from(format!("agentbox.session={session_id}")),
        ]);
        if let Some(timezone) = host_timezone_name() {
            args.extend([
                OsString::from("--label"),
                OsString::from(format!("agentbox.timezone={timezone}")),
            ]);
        }
        if let Ok(owner) = env::var("AGENTBOX_OWNER_PID")
            && !owner.is_empty()
        {
            args.extend([
                OsString::from("--label"),
                OsString::from(format!("agentbox.owner={owner}")),
            ]);
        }
    }

    let network = match config.network.mode {
        NetworkMode::Compose => input
            .compose
            .map(|project| project.network.clone())
            .unwrap_or_else(|| "bridge".into()),
        NetworkMode::Bridge => "bridge".into(),
        NetworkMode::None => "none".into(),
    };
    args.push(OsString::from("--network"));
    args.push(OsString::from(&network));

    let readonly = if config.workspace.write {
        ""
    } else {
        ",readonly"
    };
    args.push(OsString::from("--mount"));
    args.push(OsString::from(format!(
        "type=bind,src={},dst={}{}",
        input.workspace.display(),
        config.workspace.container_path,
        readonly
    )));

    args.push(OsString::from("--tmpfs"));
    args.push(OsString::from("/tmp:rw,nosuid,nodev,exec,size=2g"));
    if Path::new("/etc/localtime").is_file() {
        add_bind_file(&mut args, Path::new("/etc/localtime"), "/etc/localtime");
    }

    if config.agent.home == HomeMode::Persistent {
        add_volume(&mut args, &format!("agentbox-{slug}-home"), "/home/agent");
    } else {
        args.push(OsString::from("--tmpfs"));
        args.push(OsString::from("/home/agent:rw,nosuid,nodev,exec,size=4g"));
    }
    if config.caches.npm {
        add_volume(&mut args, "agentbox-npm-cache", "/home/agent/.npm");
    }
    if config.caches.pnpm {
        add_volume(
            &mut args,
            "agentbox-pnpm-cache",
            "/home/agent/.local/share/pnpm/store",
        );
    }
    if config.caches.cargo {
        add_volume(
            &mut args,
            "agentbox-cargo-cache",
            "/home/agent/.cargo/registry",
        );
    }
    if config.caches.pip {
        add_volume(&mut args, "agentbox-pip-cache", "/home/agent/.cache/pip");
    }

    let (imported_credentials, credential_file) = add_agent_credentials(
        &mut args,
        input.host_home,
        &command,
        codex_account.as_deref(),
    )?;
    let claude_state_file = if imported_credentials.as_deref() == Some("claude") {
        add_claude_state(&mut args, input.host_home)?
    } else {
        None
    };

    for (name, value) in &input.environment {
        args.push(OsString::from("--env"));
        args.push(OsString::from(format!("{name}={value}")));
    }
    args.push(OsString::from("--env"));
    args.push(OsString::from("HOME=/home/agent"));
    args.push(OsString::from("--env"));
    args.push(OsString::from("AGENTBOX=1"));
    if let Some(timezone) = host_timezone_name() {
        args.push(OsString::from("--env"));
        args.push(OsString::from(format!("TZ={timezone}")));
    }
    args.push(OsString::from("--env"));
    args.push(OsString::from(format!(
        "AGENTBOX_AUTO_UPDATE={}",
        u8::from(config.runtime.auto_update)
    )));
    if let Some(agent) = update_agent(&command) {
        args.push(OsString::from("--env"));
        args.push(OsString::from(format!("AGENTBOX_UPDATE_AGENT={agent}")));
    }
    args.push(OsString::from("--env"));
    args.push(OsString::from(format!(
        "AGENTBOX_CAVEMAN={}",
        u8::from(config.caveman.enabled)
    )));

    let runtime_image = runtime_image(config, input.repo_root);
    // Terminate option parsing so a repo-supplied image reference cannot be
    // interpreted by `docker run` as an additional flag (e.g. `--privileged`).
    args.push(OsString::from("--"));
    args.push(OsString::from(&runtime_image));
    args.extend(command.iter().map(OsString::from));

    Ok(RunSpec {
        args,
        network,
        environment: input.environment,
        workspace_write: config.workspace.write,
        uid_gid,
        imported_credentials,
        runtime_image,
        cargo_cache: config.caches.cargo,
        credential_file,
        claude_state_file,
    })
}

pub fn build_image_spec(config: &Config, repo_root: &Path) -> Result<Option<ImageBuildSpec>> {
    if config.runtime.dockerfile.as_os_str().is_empty() {
        return Ok(None);
    }

    let dockerfile = crate::security::resolve_repo_path(
        repo_root,
        &config.runtime.dockerfile,
        "runtime Dockerfile",
    )?;
    if !dockerfile.is_file() {
        anyhow::bail!("runtime Dockerfile is not a file: {}", dockerfile.display());
    }
    let context = crate::security::resolve_repo_path(
        repo_root,
        &config.runtime.build_context,
        "runtime build context",
    )?;
    if !context.is_dir() {
        anyhow::bail!(
            "runtime build context is not a directory: {}",
            context.display()
        );
    }

    let image = runtime_image(config, repo_root);
    Ok(Some(ImageBuildSpec {
        args: vec![
            "build".into(),
            "--tag".into(),
            image.clone().into(),
            "--file".into(),
            dockerfile.into_os_string(),
            "--build-arg".into(),
            format!("AGENTBOX_BASE_IMAGE={}", config.runtime.image).into(),
            context.into_os_string(),
        ],
        image,
    }))
}

pub fn runtime_image(config: &Config, repo_root: &Path) -> String {
    if config.runtime.dockerfile.as_os_str().is_empty() {
        return config.runtime.image.clone();
    }
    let slug = crate::project::project_slug(repo_root);
    let hash = repo_path_hash(repo_root);
    format!("agentbox/{slug}-{hash:08x}:local")
}

fn repo_path_hash(path: &Path) -> u32 {
    path.to_string_lossy()
        .bytes()
        .fold(2_166_136_261_u32, |hash, byte| {
            (hash ^ u32::from(byte)).wrapping_mul(16_777_619)
        })
}

pub fn execute_image_build(spec: &ImageBuildSpec) -> Result<u8> {
    let status = Command::new("docker")
        .args(&spec.args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to execute docker build")?;
    Ok(status.code().unwrap_or(1).clamp(0, 255) as u8)
}

pub fn format_image_build_command(spec: &ImageBuildSpec) -> String {
    std::iter::once(OsString::from("docker"))
        .chain(spec.args.iter().cloned())
        .map(|arg| shell_words::quote(&arg.to_string_lossy()).into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn host_timezone_name() -> Option<String> {
    let environment = env::var("TZ").ok();
    detect_timezone_name(
        environment.as_deref(),
        Path::new("/etc/timezone"),
        Path::new("/etc/localtime"),
    )
}

fn detect_timezone_name(
    environment: Option<&str>,
    timezone_file: &Path,
    localtime: &Path,
) -> Option<String> {
    if let Some(timezone) = environment.map(str::trim).filter(|value| !value.is_empty()) {
        return Some(timezone.to_owned());
    }
    if let Ok(timezone) = fs::read_to_string(timezone_file) {
        let timezone = timezone.trim();
        if !timezone.is_empty() {
            return Some(timezone.to_owned());
        }
    }
    let target = fs::read_link(localtime).ok()?;
    timezone_from_zoneinfo_path(&target)
}

fn timezone_from_zoneinfo_path(path: &Path) -> Option<String> {
    let marker = Path::new("zoneinfo");
    let components = path.components().collect::<Vec<_>>();
    let index = components
        .iter()
        .position(|component| component.as_os_str() == marker.as_os_str())?;
    let timezone = components[index + 1..]
        .iter()
        .collect::<PathBuf>()
        .to_string_lossy()
        .into_owned();
    (!timezone.is_empty()).then_some(timezone)
}

fn container_suffix(value: &str) -> String {
    let suffix = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let suffix = suffix
        .trim_matches('-')
        .chars()
        .take(40)
        .collect::<String>();
    if suffix.is_empty() {
        "session".into()
    } else {
        suffix
    }
}

pub(crate) fn container_name(repo_root: &Path, session_id: Option<&str>) -> String {
    let slug = crate::project::project_slug(repo_root);
    session_id
        .map(|id| format!("agentbox-{slug}-{}", container_suffix(id)))
        .unwrap_or_else(|| format!("agentbox-{slug}"))
}

fn update_agent(command: &[String]) -> Option<&'static str> {
    match command.first().map(String::as_str) {
        Some("claude") => Some("claude"),
        Some("codex") => Some("codex"),
        _ => None,
    }
}

pub(crate) fn codex_account_from_command_name(command_name: &str) -> Result<Option<&str>> {
    let Some(account) = command_name.strip_prefix("codex@") else {
        return Ok(None);
    };
    validate_codex_account_name(account)?;
    Ok(Some(account))
}

fn validate_codex_account_name(account: &str) -> Result<()> {
    if account.is_empty() {
        anyhow::bail!("Codex account name cannot be empty");
    }
    if !account
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        anyhow::bail!(
            "Codex account name `{account}` is invalid; use only letters, numbers, '.', '_', and '-'"
        );
    }
    Ok(())
}

fn add_agent_credentials(
    args: &mut Vec<OsString>,
    host_home: Option<&Path>,
    command: &[String],
    codex_account: Option<&str>,
) -> Result<(Option<String>, Option<NamedTempFile>)> {
    let Some(home) = host_home else {
        return Ok((None, None));
    };
    let (agent, file_source, target) = match command.first().map(String::as_str) {
        Some("claude") => (
            "claude",
            home.join(".claude/.credentials.json"),
            "/tmp/agentbox-claude-credentials.json",
        ),
        Some("codex") => {
            let source = codex_account
                .map(|account| home.join(".codex/accounts").join(account).join("auth.json"))
                .unwrap_or_else(|| home.join(".codex/auth.json"));
            ("codex", source, "/tmp/agentbox-codex-auth.json")
        }
        _ => return Ok((None, None)),
    };

    let mut temporary = None;
    let source = if file_source.is_file() {
        file_source
    } else if agent == "claude" {
        let Some(exported) = export_macos_claude_credentials()? else {
            return Ok((None, None));
        };
        let path = exported.path().to_path_buf();
        temporary = Some(exported);
        path
    } else if let ("codex", Some(account)) = (agent, codex_account) {
        anyhow::bail!(
            "Codex account `{}` was not found at {}; create it by saving an auth.json there",
            account,
            file_source.display()
        );
    } else {
        return Ok((None, None));
    };

    add_bind_file(args, &source, target);
    args.push(OsString::from("--env"));
    args.push(OsString::from(format!(
        "AGENTBOX_IMPORT_CREDENTIALS={agent}"
    )));
    if let Some(account) = codex_account {
        args.push(OsString::from("--env"));
        args.push(OsString::from(format!("AGENTBOX_CODEX_ACCOUNT={account}")));
    }
    let imported = codex_account
        .map(|account| format!("codex@{account}"))
        .unwrap_or_else(|| agent.to_owned());
    Ok((Some(imported), temporary))
}

fn add_claude_state(
    args: &mut Vec<OsString>,
    host_home: Option<&Path>,
) -> Result<Option<NamedTempFile>> {
    let mut state = serde_json::Map::new();
    state.insert(
        "hasCompletedOnboarding".into(),
        serde_json::Value::Bool(true),
    );

    if let Some(home) = host_home {
        let path = home.join(".claude.json");
        if path.is_file() {
            let contents = fs::read(&path)
                .with_context(|| format!("failed to read Claude state from {}", path.display()))?;
            let host_state: serde_json::Value = serde_json::from_slice(&contents)
                .with_context(|| format!("Claude state in {} is not valid JSON", path.display()))?;
            if let Some(object) = host_state.as_object()
                && let Some(theme) = object.get("theme")
            {
                state.insert("theme".into(), theme.clone());
            }
        }
    }

    let mut file = NamedTempFile::new().context("failed to create temporary Claude state file")?;
    serde_json::to_writer(&mut file, &state).context("failed to stage sanitized Claude state")?;
    file.flush()
        .context("failed to flush temporary Claude state file")?;
    add_bind_file(args, file.path(), "/tmp/agentbox-claude-state.json");
    args.push(OsString::from("--env"));
    args.push(OsString::from("AGENTBOX_IMPORT_CLAUDE_STATE=1"));
    Ok(Some(file))
}

#[cfg(target_os = "macos")]
fn export_macos_claude_credentials() -> Result<Option<NamedTempFile>> {
    let output = Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .output()
        .context("failed to read Claude Code credentials from macOS Keychain")?;
    if !output.status.success() {
        return Ok(None);
    }

    serde_json::from_slice::<serde_json::Value>(&output.stdout)
        .context("Claude Code credentials in macOS Keychain are not valid JSON")?;
    let mut file =
        NamedTempFile::new().context("failed to create temporary Claude credential file")?;
    file.write_all(&output.stdout)
        .context("failed to stage Claude credentials from macOS Keychain")?;
    file.flush()
        .context("failed to flush temporary Claude credential file")?;
    Ok(Some(file))
}

#[cfg(not(target_os = "macos"))]
fn export_macos_claude_credentials() -> Result<Option<NamedTempFile>> {
    Ok(None)
}

fn add_volume(args: &mut Vec<OsString>, source: &str, target: &str) {
    args.push(OsString::from("--mount"));
    args.push(OsString::from(format!(
        "type=volume,src={source},dst={target}"
    )));
}

fn add_bind_file(args: &mut Vec<OsString>, source: &Path, target: &str) {
    args.push(OsString::from("--mount"));
    args.push(OsString::from(format!(
        "type=bind,src={},dst={target},readonly",
        source.display()
    )));
}

#[cfg(unix)]
fn host_uid_gid() -> Result<String> {
    fn id(flag: &str) -> Result<String> {
        let output = Command::new("id")
            .arg(flag)
            .output()
            .context("failed to execute id")?;
        if !output.status.success() {
            anyhow::bail!("id {flag} failed");
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }
    Ok(format!("{}:{}", id("-u")?, id("-g")?))
}

#[cfg(not(unix))]
fn host_uid_gid() -> Result<String> {
    Ok("1000:1000".into())
}

pub fn execute(spec: &RunSpec) -> Result<u8> {
    let _credential_file = &spec.credential_file;
    let _claude_state_file = &spec.claude_state_file;
    initialize_cargo_cache(spec)?;
    let status = Command::new("docker")
        .args(&spec.args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to execute docker; is Docker installed and running?")?;
    Ok(status.code().unwrap_or(1).clamp(0, 255) as u8)
}

fn initialize_cargo_cache(spec: &RunSpec) -> Result<()> {
    if !spec.cargo_cache {
        return Ok(());
    }

    let output = Command::new("docker")
        .args(cargo_cache_init_args(spec))
        .output()
        .context("failed to initialize the Cargo cache volume")?;
    if !output.status.success() {
        anyhow::bail!(
            "failed to make the Cargo cache writable by container user {}: {}",
            spec.uid_gid,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn cargo_cache_init_args(spec: &RunSpec) -> Vec<OsString> {
    vec![
        "run".into(),
        "--rm".into(),
        "--network".into(),
        "none".into(),
        "--read-only".into(),
        "--security-opt".into(),
        "no-new-privileges".into(),
        "--cap-drop".into(),
        "ALL".into(),
        "--cap-add".into(),
        "CHOWN".into(),
        "--user".into(),
        "0:0".into(),
        "--mount".into(),
        "type=volume,src=agentbox-cargo-cache,dst=/cache".into(),
        "--entrypoint".into(),
        "/bin/sh".into(),
        "--".into(),
        spec.runtime_image.clone().into(),
        "-c".into(),
        r#"owner="$(stat -c '%u:%g' /cache)"; if [ "$owner" != "$1" ]; then chown -R "$1" /cache; fi"#.into(),
        "agentbox-cargo-cache-init".into(),
        spec.uid_gid.clone().into(),
    ]
}

pub fn format_command(spec: &RunSpec) -> String {
    let mut redact_next_env = false;
    std::iter::once(OsString::from("docker"))
        .chain(spec.args.iter().cloned())
        .map(|arg| {
            if redact_next_env {
                redact_next_env = false;
                return redact_environment(&arg);
            }
            if arg == OsStr::new("--env") {
                redact_next_env = true;
            }
            arg
        })
        .map(|arg| shell_words::quote(&arg.to_string_lossy()).into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

fn redact_environment(argument: &OsStr) -> OsString {
    let rendered = argument.to_string_lossy();
    let Some((name, _)) = rendered.split_once('=') else {
        return argument.to_owned();
    };
    if crate::security::is_sensitive(name) {
        OsString::from(format!("{name}=<redacted>"))
    } else {
        argument.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn safe_spec_has_no_host_home_or_socket() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config::default();
        let spec = build_run_spec(BuildInput {
            config: &config,
            repo_root: temp.path(),
            workspace: temp.path(),
            host_home: None,
            compose: None,
            environment: BTreeMap::new(),
            command: vec!["sh".into()],
            interactive: false,
            session_id: None,
        })
        .unwrap();
        let rendered = format_command(&spec);
        assert!(!rendered.contains("/var/run/docker.sock"));
        assert!(rendered.contains("HOME=/home/agent"));
        assert!(rendered.contains("AGENTBOX_AUTO_UPDATE=1"));
        assert!(!rendered.contains("AGENTBOX_UPDATE_AGENT="));
        assert!(rendered.contains("AGENTBOX_CAVEMAN=0"));
        assert!(rendered.contains("--cap-drop ALL"));
        assert!(rendered.contains("--pids-limit 2048"));
        assert!(rendered.contains("--network bridge"));
        // The image must be guarded by a `--` option terminator.
        assert!(rendered.contains(&format!("-- {}", spec.runtime_image)));
        assert!(rendered.contains("type=bind,src="));
        if Path::new("/etc/localtime").is_file() {
            assert!(rendered.contains("src=/etc/localtime,dst=/etc/localtime,readonly"));
        }
        if let Some(timezone) = host_timezone_name() {
            assert!(rendered.contains(&format!("TZ={timezone}")));
        }
        assert!(!rendered.contains(",rw"));
    }

    #[test]
    fn timezone_detection_prefers_environment_then_file() {
        let temp = tempfile::tempdir().unwrap();
        let timezone_file = temp.path().join("timezone");
        std::fs::write(&timezone_file, "Europe/Amsterdam\n").unwrap();

        assert_eq!(
            detect_timezone_name(
                Some("America/New_York"),
                &timezone_file,
                &temp.path().join("localtime")
            ),
            Some("America/New_York".into())
        );
        assert_eq!(
            detect_timezone_name(None, &timezone_file, &temp.path().join("localtime")),
            Some("Europe/Amsterdam".into())
        );
    }

    #[test]
    fn timezone_is_extracted_from_zoneinfo_path() {
        assert_eq!(
            timezone_from_zoneinfo_path(Path::new("/usr/share/zoneinfo/America/Los_Angeles")),
            Some("America/Los_Angeles".into())
        );
        assert_eq!(timezone_from_zoneinfo_path(Path::new("/etc/UTC")), None);
    }

    #[test]
    fn readonly_workspace_uses_mount_readonly_flag() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.workspace.write = false;
        let spec = build_run_spec(BuildInput {
            config: &config,
            repo_root: temp.path(),
            workspace: temp.path(),
            host_home: None,
            compose: None,
            environment: BTreeMap::new(),
            command: vec!["true".into()],
            interactive: false,
            session_id: None,
        })
        .unwrap();
        assert!(format_command(&spec).contains(",readonly"));
    }

    #[test]
    fn auto_updates_can_be_disabled() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.runtime.auto_update = false;
        let spec = build_run_spec(BuildInput {
            config: &config,
            repo_root: temp.path(),
            workspace: temp.path(),
            host_home: None,
            compose: None,
            environment: BTreeMap::new(),
            command: vec!["true".into()],
            interactive: false,
            session_id: None,
        })
        .unwrap();

        assert!(format_command(&spec).contains("AGENTBOX_AUTO_UPDATE=0"));
    }

    #[test]
    fn project_dockerfile_builds_and_selects_a_derived_image() {
        let temp = tempfile::tempdir().unwrap();
        let dockerfile = temp.path().join(".agentbox/Dockerfile");
        std::fs::create_dir_all(dockerfile.parent().unwrap()).unwrap();
        std::fs::write(
            &dockerfile,
            "ARG AGENTBOX_BASE_IMAGE\nFROM ${AGENTBOX_BASE_IMAGE}\n",
        )
        .unwrap();
        let mut config = Config::default();
        config.runtime.dockerfile = ".agentbox/Dockerfile".into();

        let build = build_image_spec(&config, temp.path()).unwrap().unwrap();
        let command = format_image_build_command(&build);
        assert!(build.image.starts_with("agentbox/"));
        assert!(build.image.ends_with(":local"));
        assert!(command.contains("AGENTBOX_BASE_IMAGE=agentbox/fullstack:latest"));
        assert!(command.contains(".agentbox/Dockerfile"));

        let run = build_run_spec(BuildInput {
            config: &config,
            repo_root: temp.path(),
            workspace: temp.path(),
            host_home: None,
            compose: None,
            environment: BTreeMap::new(),
            command: vec!["true".into()],
            interactive: false,
            session_id: None,
        })
        .unwrap();
        assert!(format_command(&run).contains(&build.image));
        assert!(!format_command(&run).contains("agentbox/fullstack:latest"));
    }

    #[test]
    fn runtime_without_dockerfile_uses_configured_image() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.runtime.image = "example/custom:1".into();

        assert!(build_image_spec(&config, temp.path()).unwrap().is_none());
        assert_eq!(runtime_image(&config, temp.path()), "example/custom:1");
    }

    #[test]
    fn only_the_started_agent_is_selected_for_updates() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config::default();

        for (command, expected, unexpected) in [
            (
                "claude",
                "AGENTBOX_UPDATE_AGENT=claude",
                "AGENTBOX_UPDATE_AGENT=codex",
            ),
            (
                "codex",
                "AGENTBOX_UPDATE_AGENT=codex",
                "AGENTBOX_UPDATE_AGENT=claude",
            ),
        ] {
            let spec = build_run_spec(BuildInput {
                config: &config,
                repo_root: temp.path(),
                workspace: temp.path(),
                host_home: None,
                compose: None,
                environment: BTreeMap::new(),
                command: vec![command.into()],
                interactive: false,
                session_id: None,
            })
            .unwrap();
            let rendered = format_command(&spec);

            assert!(rendered.contains(expected));
            assert!(!rendered.contains(unexpected));
        }
    }

    #[test]
    fn imports_only_the_started_agents_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(home.join(".claude/.credentials.json"), "{}").unwrap();
        std::fs::write(home.join(".codex/auth.json"), "{}").unwrap();
        let config = Config::default();

        for (command, expected, unexpected) in [
            ("claude", ".claude/.credentials.json", ".codex/auth.json"),
            ("codex", ".codex/auth.json", ".claude/.credentials.json"),
        ] {
            let spec = build_run_spec(BuildInput {
                config: &config,
                repo_root: temp.path(),
                workspace: temp.path(),
                host_home: Some(&home),
                compose: None,
                environment: BTreeMap::new(),
                command: vec![command.into()],
                interactive: false,
                session_id: None,
            })
            .unwrap();
            let rendered = format_command(&spec);

            assert!(rendered.contains(expected));
            assert!(!rendered.contains(unexpected));
            assert!(rendered.contains("readonly"));
            assert_eq!(spec.imported_credentials.as_deref(), Some(command));
            assert!(spec.credential_file.is_none());
            if command == "claude" {
                assert!(rendered.contains("/tmp/agentbox-claude-state.json"));
                assert!(rendered.contains("AGENTBOX_IMPORT_CLAUDE_STATE=1"));
                assert!(spec.claude_state_file.is_some());
            } else {
                assert!(!rendered.contains("/tmp/agentbox-claude-state.json"));
                assert!(spec.claude_state_file.is_none());
            }
        }
    }

    #[test]
    fn named_codex_account_imports_selected_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir_all(home.join(".codex/accounts/work")).unwrap();
        std::fs::write(home.join(".codex/accounts/work/auth.json"), "{}").unwrap();

        let spec = build_run_spec(BuildInput {
            config: &Config::default(),
            repo_root: temp.path(),
            workspace: temp.path(),
            host_home: Some(&home),
            compose: None,
            environment: BTreeMap::new(),
            command: vec!["codex@work".into(), "--no-alt-screen".into()],
            interactive: false,
            session_id: None,
        })
        .unwrap();
        let rendered = format_command(&spec);

        assert!(rendered.contains(".codex/accounts/work/auth.json"));
        assert!(rendered.contains("AGENTBOX_IMPORT_CREDENTIALS=codex"));
        assert!(rendered.contains("AGENTBOX_CODEX_ACCOUNT=work"));
        assert!(rendered.ends_with("codex --no-alt-screen"));
        assert!(!rendered.contains("codex@work"));
        assert_eq!(spec.imported_credentials.as_deref(), Some("codex@work"));
    }

    #[test]
    fn missing_named_codex_account_is_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir_all(home.join(".codex")).unwrap();

        let error = build_run_spec(BuildInput {
            config: &Config::default(),
            repo_root: temp.path(),
            workspace: temp.path(),
            host_home: Some(&home),
            compose: None,
            environment: BTreeMap::new(),
            command: vec!["codex@work".into()],
            interactive: false,
            session_id: None,
        })
        .unwrap_err()
        .to_string();

        assert!(error.contains("Codex account `work` was not found"));
        assert!(error.contains(".codex/accounts/work/auth.json"));
    }

    #[test]
    fn claude_state_import_is_sanitized_and_completes_onboarding() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(home.join(".claude/.credentials.json"), "{}").unwrap();
        std::fs::write(
            home.join(".claude.json"),
            r#"{
                "theme": "dark",
                "hasCompletedOnboarding": false,
                "oauthAccount": {"emailAddress": "user@example.com"},
                "mcpServers": {"secret": {"env": {"TOKEN": "do-not-import"}}},
                "projects": {"/private/project": {}}
            }"#,
        )
        .unwrap();

        let spec = build_run_spec(BuildInput {
            config: &Config::default(),
            repo_root: temp.path(),
            workspace: temp.path(),
            host_home: Some(&home),
            compose: None,
            environment: BTreeMap::new(),
            command: vec!["claude".into()],
            interactive: false,
            session_id: None,
        })
        .unwrap();
        let state: serde_json::Value =
            serde_json::from_reader(spec.claude_state_file.as_ref().unwrap().reopen().unwrap())
                .unwrap();

        assert_eq!(state["hasCompletedOnboarding"], true);
        assert_eq!(state["theme"], "dark");
        assert!(state.get("oauthAccount").is_none());
        assert!(state.get("mcpServers").is_none());
        assert!(state.get("projects").is_none());
        assert!(!state.to_string().contains("do-not-import"));
    }

    #[test]
    fn cargo_cache_initializer_repairs_volume_for_the_container_user() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config::default();
        let spec = build_run_spec(BuildInput {
            config: &config,
            repo_root: temp.path(),
            workspace: temp.path(),
            host_home: None,
            compose: None,
            environment: BTreeMap::new(),
            command: vec!["true".into()],
            interactive: false,
            session_id: None,
        })
        .unwrap();
        let rendered = cargo_cache_init_args(&spec)
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");

        assert!(rendered.contains("--network none"));
        assert!(rendered.contains("--cap-drop ALL --cap-add CHOWN"));
        assert!(rendered.contains("--user 0:0"));
        assert!(rendered.contains(&format!("-- {}", spec.runtime_image)));
        assert!(rendered.contains("agentbox-cargo-cache"));
        assert!(rendered.ends_with(&spec.uid_gid));
    }

    #[test]
    fn missing_or_custom_agent_credentials_are_not_imported() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config::default();

        for command in ["claude", "codex", "custom-agent"] {
            let spec = build_run_spec(BuildInput {
                config: &config,
                repo_root: temp.path(),
                workspace: temp.path(),
                host_home: Some(temp.path()),
                compose: None,
                environment: BTreeMap::new(),
                command: vec![command.into()],
                interactive: false,
                session_id: None,
            })
            .unwrap();

            assert_eq!(spec.imported_credentials, None);
            assert!(!format_command(&spec).contains("AGENTBOX_IMPORT_CREDENTIALS"));
        }
    }

    #[test]
    fn caveman_can_be_enabled() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.caveman.enabled = true;
        let spec = build_run_spec(BuildInput {
            config: &config,
            repo_root: temp.path(),
            workspace: temp.path(),
            host_home: None,
            compose: None,
            environment: BTreeMap::new(),
            command: vec!["true".into()],
            interactive: false,
            session_id: None,
        })
        .unwrap();

        assert!(format_command(&spec).contains("AGENTBOX_CAVEMAN=1"));
    }

    #[test]
    fn tui_session_ids_are_safe_container_suffixes() {
        assert_eq!(container_suffix("TUI 42/../../"), "tui-42");
        assert_eq!(container_suffix("***"), "session");
        assert_eq!(
            container_name(Path::new("/tmp/My Project"), Some("TUI 42/../../")),
            "agentbox-my-project-tui-42"
        );
    }

    #[test]
    fn tui_containers_are_ephemeral_and_labeled() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config::default();
        let spec = build_run_spec(BuildInput {
            config: &config,
            repo_root: temp.path(),
            workspace: temp.path(),
            host_home: None,
            compose: None,
            environment: BTreeMap::new(),
            command: vec!["codex".into()],
            interactive: false,
            session_id: Some("tui-123"),
        })
        .unwrap();
        let rendered = format_command(&spec);

        assert!(spec.args.iter().any(|argument| argument == "--rm"));
        assert!(rendered.contains("agentbox.managed=true"));
        assert!(rendered.contains("agentbox.session=tui-123"));
        assert!(rendered.contains("agentbox.repo="));
        if let Some(timezone) = host_timezone_name() {
            assert!(rendered.contains(&format!("agentbox.timezone={timezone}")));
        }
    }

    #[test]
    fn displayed_command_redacts_sensitive_environment_values() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config::default();
        let environment = BTreeMap::from([
            ("NODE_ENV".into(), "development".into()),
            ("OPENAI_API_KEY".into(), "super-secret-value".into()),
        ]);
        let spec = build_run_spec(BuildInput {
            config: &config,
            repo_root: temp.path(),
            workspace: temp.path(),
            host_home: None,
            compose: None,
            environment,
            command: vec!["true".into()],
            interactive: false,
            session_id: None,
        })
        .unwrap();

        let rendered = format_command(&spec);
        assert!(rendered.contains("NODE_ENV=development"));
        assert!(rendered.contains("OPENAI_API_KEY=<redacted>"));
        assert!(!rendered.contains("super-secret-value"));
        assert!(
            spec.args
                .iter()
                .any(|argument| argument == "OPENAI_API_KEY=super-secret-value")
        );
    }
}
