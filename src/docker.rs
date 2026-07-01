use std::{
    collections::BTreeMap,
    env,
    ffi::{OsStr, OsString},
    fs,
    io::{self, IsTerminal, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use tempfile::NamedTempFile;

use crate::{
    compose::ComposeProject,
    config::{Config, GuiConfig, HomeMode, NetworkMode},
};

#[derive(Debug)]
pub struct RunSpec {
    pub args: Vec<OsString>,
    pub network: String,
    pub environment: BTreeMap<String, String>,
    pub workspace_write: bool,
    pub uid_gid: String,
    pub imported_credentials: Option<String>,
    pub host_browser: bool,
    runtime_image: String,
    cargo_cache: bool,
    gui_cache: bool,
    codex_desktop_cache: bool,
    claude_desktop_cache: bool,
    credential_file: Option<NamedTempFile>,
    claude_state_file: Option<NamedTempFile>,
    host_browser_bridge: Option<HostBrowserBridge>,
}

#[derive(Debug, Clone)]
pub struct ImageBuildSpec {
    pub args: Vec<OsString>,
    pub image: String,
}

#[derive(Debug)]
struct HostBrowserBridge {
    listener: TcpListener,
    url: String,
    token: String,
    container_name: String,
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
        OsString::from(&container_name),
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

    let host_browser_bridge = if config.host_browser.enabled && network != "none" {
        Some(add_host_browser(&mut args, &container_name)?)
    } else {
        None
    };

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
    add_worktree_git_mounts(
        &mut args,
        input.workspace,
        Path::new(&config.workspace.container_path),
        config.workspace.write,
    )?;

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
    if config.gui.enabled {
        add_volume(&mut args, "agentbox-gui-local", "/home/agent/.local");
    } else if config.caches.pnpm {
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
    if config.gui.enabled {
        add_volume(&mut args, "agentbox-gui-cache", "/home/agent/.cache");
    } else if config.caches.pip {
        add_volume(&mut args, "agentbox-pip-cache", "/home/agent/.cache/pip");
    }
    if config.gui.enabled {
        add_volume(
            &mut args,
            "agentbox-codex-desktop",
            "/agentbox/codex-desktop",
        );
    }
    let claude_desktop_requested = command_requests_claude_desktop(&command);
    if config.gui.enabled && claude_desktop_requested {
        add_volume(
            &mut args,
            "agentbox-claude-desktop",
            "/agentbox/claude-desktop",
        );
    }

    let credential_command = credential_command(config, &command);
    let (imported_credentials, credential_file) = add_agent_credentials(
        &mut args,
        input.host_home,
        credential_command.as_slice(),
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
    add_git_workspace_config(&mut args, &config.workspace.container_path);
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
    add_gui(
        &mut args,
        &config.gui,
        input.host_home,
        claude_desktop_requested,
    )?;

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
        host_browser: host_browser_bridge.is_some(),
        runtime_image,
        cargo_cache: config.caches.cargo,
        gui_cache: config.gui.enabled,
        codex_desktop_cache: config.gui.enabled,
        claude_desktop_cache: config.gui.enabled && claude_desktop_requested,
        credential_file,
        claude_state_file,
        host_browser_bridge,
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
    match command.first() {
        Some(command) if command == "claude" => Some("claude"),
        Some(command)
            if command == "codex"
                || command == "codex-login"
                || command_requests_codex_desktop(command) =>
        {
            Some("codex")
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeDesktopBackend {
    Auto,
    X11,
    Wayland,
}

fn claude_desktop_backend() -> ClaudeDesktopBackend {
    if env::var("AGENTBOX_CLAUDE_DESKTOP_BACKEND").is_ok_and(|value| value == "x11")
        || env::var("CLAUDE_USE_WAYLAND").is_ok_and(|value| value == "0")
    {
        ClaudeDesktopBackend::X11
    } else if env::var("AGENTBOX_CLAUDE_DESKTOP_BACKEND").is_ok_and(|value| value == "wayland")
        || env::var("CLAUDE_USE_WAYLAND").is_ok_and(|value| value == "1")
    {
        ClaudeDesktopBackend::Wayland
    } else {
        ClaudeDesktopBackend::Auto
    }
}

fn command_requests_codex_desktop(command: &str) -> bool {
    command == "codex-desktop" || command.contains("/codex-desktop/")
}

fn command_requests_codex_login(command: &str) -> bool {
    command == "codex-login"
}

fn command_requests_claude_desktop(command: &[String]) -> bool {
    command
        .first()
        .is_some_and(|command| command_requests_claude_desktop_name(command))
}

fn command_requests_claude_desktop_name(command: &str) -> bool {
    command == "claude-desktop" || command.contains("claude-desktop")
}

fn credential_command(config: &Config, command: &[String]) -> Vec<String> {
    if !config.gui.enabled {
        return command.to_vec();
    }

    // Desktop GUI apps launch through a wrapper script rather than the bare
    // `claude`/`codex` executable, so map each desktop launcher to the
    // credential file its agent expects. Claude Desktop launchers carry
    // `claude-desktop` in their path; anything else (including Codex Desktop and
    // bare launcher paths) falls back to Codex, matching `agentbox run codex`.
    let first = command.first().map(String::as_str);
    if first.is_some_and(command_requests_claude_desktop_name) {
        if config.gui.import_claude_credentials {
            return vec!["claude".into()];
        }
    } else if first.is_some_and(command_requests_codex_login) {
        return command.to_vec();
    } else if config.gui.import_codex_credentials && !matches!(first, Some("claude" | "codex")) {
        return vec!["codex".into()];
    }
    command.to_vec()
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

fn add_gui(
    args: &mut Vec<OsString>,
    config: &GuiConfig,
    host_home: Option<&Path>,
    claude_desktop: bool,
) -> Result<()> {
    if !config.enabled {
        return Ok(());
    }

    if claude_desktop {
        return add_claude_desktop_gui(args, config, host_home);
    }

    let mut added = false;
    if config.wayland && add_wayland(args, config)? {
        added = true;
    }
    if config.x11 && add_x11(args, config, host_home)? {
        added = true;
    }
    if !added {
        anyhow::bail!("GUI passthrough is enabled, but no usable X11 or Wayland socket was found");
    }
    add_env(args, "AGENTBOX_GUI", "1");
    add_env(args, "NO_AT_BRIDGE", "1");
    Ok(())
}

fn add_claude_desktop_gui(
    args: &mut Vec<OsString>,
    config: &GuiConfig,
    host_home: Option<&Path>,
) -> Result<()> {
    let mut added = false;
    match claude_desktop_backend() {
        ClaudeDesktopBackend::X11 => {
            if config.x11 && add_x11(args, config, host_home)? {
                add_env(args, "CLAUDE_USE_WAYLAND", "0");
                added = true;
            }
        }
        ClaudeDesktopBackend::Wayland => {
            if config.wayland && add_wayland(args, config)? {
                add_env(args, "CLAUDE_USE_WAYLAND", "1");
                add_env(args, "XDG_SESSION_TYPE", "wayland");
                added = true;
            }
        }
        ClaudeDesktopBackend::Auto => {
            if config.x11 && add_x11(args, config, host_home)? {
                added = true;
            } else if config.wayland && add_wayland(args, config)? {
                add_env(args, "CLAUDE_USE_WAYLAND", "1");
                add_env(args, "XDG_SESSION_TYPE", "wayland");
                added = true;
            }
        }
    }
    if !added {
        anyhow::bail!("GUI passthrough is enabled, but no usable X11 or Wayland socket was found");
    }
    add_env(args, "AGENTBOX_GUI", "1");
    add_env(args, "NO_AT_BRIDGE", "1");
    Ok(())
}

fn add_wayland(args: &mut Vec<OsString>, config: &GuiConfig) -> Result<bool> {
    let Some((source, display)) = wayland_socket(config)? else {
        return Ok(false);
    };
    if !source.exists() {
        return Ok(false);
    }

    let target_display = if display.is_empty() {
        "agentbox-wayland.sock".to_owned()
    } else {
        format!("agentbox-{}", wayland_target_name(&display))
    };
    add_bind_file(args, &source, &format!("/tmp/{target_display}"));
    add_env(args, "XDG_RUNTIME_DIR", "/tmp");
    add_env(args, "WAYLAND_DISPLAY", &target_display);
    Ok(true)
}

fn wayland_socket(config: &GuiConfig) -> Result<Option<(PathBuf, String)>> {
    if !config.wayland_socket.as_os_str().is_empty() {
        let display = wayland_target_name(&config.wayland_display);
        return Ok(Some((config.wayland_socket.clone(), display)));
    }

    let display = configured_or_env(&config.wayland_display, "WAYLAND_DISPLAY");
    let Some(display) = display.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let source = if Path::new(&display).is_absolute() {
        PathBuf::from(&display)
    } else {
        let runtime = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("WAYLAND_DISPLAY is set but XDG_RUNTIME_DIR is not"))?;
        runtime.join(&display)
    };
    Ok(Some((source, display)))
}

fn wayland_target_name(display: &str) -> String {
    display.trim().trim_start_matches('/').replace('/', "-")
}

fn add_x11(args: &mut Vec<OsString>, config: &GuiConfig, host_home: Option<&Path>) -> Result<bool> {
    let display = configured_or_env(&config.display, "DISPLAY");
    let Some(display) = display.filter(|value| !value.is_empty()) else {
        return Ok(false);
    };
    if !config.x11_socket.exists() {
        return Ok(false);
    }

    add_bind_path(args, &config.x11_socket, "/tmp/.X11-unix", true);
    add_env(args, "DISPLAY", &display);
    if let Some(xauthority) = xauthority_path(config, host_home)
        && xauthority.is_file()
    {
        add_bind_file(args, &xauthority, "/tmp/agentbox-xauthority");
        add_env(args, "XAUTHORITY", "/tmp/agentbox-xauthority");
    }
    Ok(true)
}

fn xauthority_path(config: &GuiConfig, host_home: Option<&Path>) -> Option<PathBuf> {
    if !config.xauthority.as_os_str().is_empty() {
        return Some(config.xauthority.clone());
    }
    if let Some(path) = env::var_os("XAUTHORITY").map(PathBuf::from) {
        return Some(path);
    }
    host_home.map(|home| home.join(".Xauthority"))
}

fn configured_or_env(configured: &str, variable: &str) -> Option<String> {
    let configured = configured.trim();
    if configured.is_empty() {
        env::var(variable).ok()
    } else {
        Some(configured.to_owned())
    }
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
    add_bind_path(args, source, target, true);
}

fn add_bind_path(args: &mut Vec<OsString>, source: &Path, target: &str, readonly: bool) {
    args.push(OsString::from("--mount"));
    let readonly = if readonly { ",readonly" } else { "" };
    args.push(OsString::from(format!(
        "type=bind,src={},dst={target}{readonly}",
        source.display()
    )));
}

fn add_worktree_git_mounts(
    args: &mut Vec<OsString>,
    workspace: &Path,
    container_workspace: &Path,
    writable: bool,
) -> Result<()> {
    let Some(worktree) = linked_worktree_git_paths(workspace, container_workspace)? else {
        return Ok(());
    };

    let readonly = !writable;
    add_git_metadata_mount(
        args,
        workspace,
        &worktree.common_dir,
        &worktree.common_target,
        readonly,
    );
    if worktree.git_dir != worktree.common_dir
        && !worktree.git_dir.starts_with(&worktree.common_dir)
    {
        add_git_metadata_mount(
            args,
            workspace,
            &worktree.git_dir,
            &worktree.git_target,
            readonly,
        );
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct WorktreeGitPaths {
    git_dir: PathBuf,
    git_target: PathBuf,
    common_dir: PathBuf,
    common_target: PathBuf,
}

fn linked_worktree_git_paths(
    workspace: &Path,
    container_workspace: &Path,
) -> Result<Option<WorktreeGitPaths>> {
    let git_file = workspace.join(".git");
    if !git_file.is_file() {
        return Ok(None);
    }

    let contents = fs::read_to_string(&git_file)
        .with_context(|| format!("failed to read {}", git_file.display()))?;
    let Some(raw_git_dir) = parse_gitdir_file(&contents) else {
        return Ok(None);
    };

    let git_dir = resolve_host_git_path(workspace, raw_git_dir)
        .with_context(|| format!("failed to resolve gitdir from {}", git_file.display()))?;
    let git_target =
        resolve_container_git_path(container_workspace, raw_git_dir).with_context(|| {
            format!(
                "failed to resolve container gitdir for {}",
                git_file.display()
            )
        })?;
    let commondir_file = git_dir.join("commondir");
    let (common_dir, common_target) = if commondir_file.is_file() {
        let contents = fs::read_to_string(&commondir_file)
            .with_context(|| format!("failed to read {}", commondir_file.display()))?;
        let raw_common_dir = contents.trim();
        if raw_common_dir.is_empty() {
            (git_dir.clone(), git_target.clone())
        } else {
            (
                resolve_host_git_path(&git_dir, raw_common_dir).with_context(|| {
                    format!(
                        "failed to resolve commondir from {}",
                        commondir_file.display()
                    )
                })?,
                resolve_container_git_path(&git_target, raw_common_dir).with_context(|| {
                    format!(
                        "failed to resolve container commondir for {}",
                        commondir_file.display()
                    )
                })?,
            )
        }
    } else {
        (git_dir.clone(), git_target.clone())
    };

    Ok(Some(WorktreeGitPaths {
        git_dir,
        git_target,
        common_dir,
        common_target,
    }))
}

fn parse_gitdir_file(contents: &str) -> Option<&str> {
    contents
        .strip_prefix("gitdir:")
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn resolve_host_git_path(base: &Path, raw: &str) -> Result<PathBuf> {
    let path = Path::new(raw);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    fs::canonicalize(&path)
        .with_context(|| format!("git metadata path does not exist: {}", path.display()))
}

fn resolve_container_git_path(base: &Path, raw: &str) -> Result<PathBuf> {
    let path = Path::new(raw);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    normalize_absolute_path(&path)
}

fn normalize_absolute_path(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        anyhow::bail!(
            "container git metadata path must be absolute: {}",
            path.display()
        );
    }

    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
            Component::Prefix(_) => {
                anyhow::bail!(
                    "unsupported container git metadata path: {}",
                    path.display()
                );
            }
        }
    }
    Ok(normalized)
}

fn add_git_metadata_mount(
    args: &mut Vec<OsString>,
    workspace: &Path,
    source: &Path,
    target: &Path,
    readonly: bool,
) {
    if source.starts_with(workspace) {
        return;
    }
    add_bind_path(args, source, &target.to_string_lossy(), readonly);
}

fn add_env(args: &mut Vec<OsString>, name: &str, value: &str) {
    args.push(OsString::from("--env"));
    args.push(OsString::from(format!("{name}={value}")));
}

fn add_git_workspace_config(args: &mut Vec<OsString>, workspace: &str) {
    add_env(args, "GIT_CONFIG_COUNT", "1");
    add_env(args, "GIT_CONFIG_KEY_0", "safe.directory");
    add_env(args, "GIT_CONFIG_VALUE_0", workspace);
}

fn add_host_browser(args: &mut Vec<OsString>, container_name: &str) -> Result<HostBrowserBridge> {
    let listener = TcpListener::bind(("0.0.0.0", 0))
        .context("failed to start host browser bridge listener")?;
    let port = listener
        .local_addr()
        .context("failed to read host browser bridge address")?
        .port();
    let token = browser_bridge_token();
    args.push(OsString::from("--add-host"));
    args.push(OsString::from("host.docker.internal:host-gateway"));
    add_env(
        args,
        "AGENTBOX_HOST_BROWSER_URL",
        &format!("http://host.docker.internal:{port}/open"),
    );
    add_env(args, "AGENTBOX_HOST_BROWSER_TOKEN", &token);
    add_env(args, "BROWSER", "/usr/local/bin/agentbox-open");
    Ok(HostBrowserBridge {
        listener,
        url: format!("http://127.0.0.1:{port}/open"),
        token,
        container_name: container_name.to_owned(),
    })
}

fn browser_bridge_token() -> String {
    let mut bytes = [0_u8; 32];
    if let Ok(mut file) = fs::File::open("/dev/urandom")
        && file.read_exact(&mut bytes).is_ok()
    {
        return hex(&bytes);
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    hex(format!("{}:{now}", std::process::id()).as_bytes())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(DIGITS[(byte >> 4) as usize] as char);
        encoded.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    encoded
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
    initialize_gui_cache(spec)?;
    initialize_desktop_caches(spec)?;
    let _host_browser_worker = spec
        .host_browser_bridge
        .as_ref()
        .map(HostBrowserWorker::start)
        .transpose()?;
    let status = Command::new("docker")
        .args(&spec.args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to execute docker; is Docker installed and running?")?;
    Ok(status.code().unwrap_or(1).clamp(0, 255) as u8)
}

struct HostBrowserWorker {
    shutdown: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    wake_url: String,
    callback_forwarders: Arc<Mutex<BTreeMap<u16, CallbackForwarder>>>,
}

impl HostBrowserWorker {
    fn start(bridge: &HostBrowserBridge) -> Result<Self> {
        let listener = bridge
            .listener
            .try_clone()
            .context("failed to clone host browser bridge listener")?;
        listener
            .set_nonblocking(true)
            .context("failed to configure host browser bridge listener")?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = shutdown.clone();
        let token = bridge.token.clone();
        let container_name = bridge.container_name.clone();
        let callback_forwarders = Arc::new(Mutex::new(BTreeMap::new()));
        let thread_callback_forwarders = callback_forwarders.clone();
        let join = thread::spawn(move || {
            while !thread_shutdown.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => handle_browser_request(
                        stream,
                        &token,
                        &container_name,
                        &thread_callback_forwarders,
                    ),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(25));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            shutdown,
            join: Some(join),
            wake_url: bridge.url.clone(),
            callback_forwarders,
        })
    }
}

impl Drop for HostBrowserWorker {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(
            self.wake_url
                .trim_start_matches("http://")
                .trim_end_matches("/open"),
        );
        if let Ok(mut forwarders) = self.callback_forwarders.lock() {
            forwarders.clear();
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn handle_browser_request(
    mut stream: TcpStream,
    token: &str,
    container_name: &str,
    callback_forwarders: &Arc<Mutex<BTreeMap<u16, CallbackForwarder>>>,
) {
    let response = match read_browser_request(&mut stream) {
        Ok(request) if request.token == token => {
            let result = ensure_callback_forwarders(
                callback_forwarders,
                container_name,
                loopback_callback_ports(&request.url),
            )
            .and_then(|()| open_host_browser(&request.url));
            match result {
                Ok(()) => ("204 No Content", ""),
                Err(_) => ("502 Bad Gateway", "could not open host browser\n"),
            }
        }
        Ok(_) => ("403 Forbidden", "invalid token\n"),
        Err(_) => ("400 Bad Request", "invalid request\n"),
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response.0,
        response.1.len(),
        response.1
    );
}

struct BrowserOpenRequest {
    token: String,
    url: String,
}

fn read_browser_request(stream: &mut TcpStream) -> Result<BrowserOpenRequest> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let count = stream
            .read(&mut chunk)
            .context("failed to read browser request")?;
        if count == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..count]);
        if buffer.len() > 65_536 {
            anyhow::bail!("browser request is too large");
        }
        if let Some((header_end, content_length)) = browser_request_shape(&buffer)?
            && buffer.len() >= header_end + content_length
        {
            break;
        }
    }

    let Some((header_end, content_length)) = browser_request_shape(&buffer)? else {
        anyhow::bail!("browser request is incomplete");
    };
    if buffer.len() < header_end + content_length {
        anyhow::bail!("browser request body is incomplete");
    }
    let headers = String::from_utf8_lossy(&buffer[..header_end]);
    let mut lines = headers.lines();
    let request_line = lines.next().unwrap_or_default();
    if request_line != "POST /open HTTP/1.1" && request_line != "POST /open HTTP/1.0" {
        anyhow::bail!("unsupported browser request");
    }
    let token = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("x-agentbox-token"))
        .map(|(_, value)| value.trim().to_owned())
        .ok_or_else(|| anyhow::anyhow!("missing token"))?;
    let body = &buffer[header_end..header_end + content_length];
    let url = String::from_utf8(body.to_vec())
        .context("browser URL is not valid UTF-8")?
        .trim()
        .to_owned();
    validate_browser_url(&url)?;
    Ok(BrowserOpenRequest { token, url })
}

fn browser_request_shape(buffer: &[u8]) -> Result<Option<(usize, usize)>> {
    let Some(header_end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
        return Ok(None);
    };
    let header_end = header_end + 4;
    let headers = String::from_utf8_lossy(&buffer[..header_end]);
    let content_length = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.trim().parse::<usize>())
        .transpose()
        .context("invalid content length")?
        .unwrap_or(0);
    Ok(Some((header_end, content_length)))
}

fn validate_browser_url(url: &str) -> Result<()> {
    if url.len() > 8192 || url.chars().any(char::is_control) {
        anyhow::bail!("browser URL is invalid");
    }
    if !url.starts_with("https://") && !url.starts_with("http://") {
        anyhow::bail!("browser URL must start with http:// or https://");
    }
    Ok(())
}

fn loopback_callback_ports(url: &str) -> Vec<u16> {
    let mut candidates = vec![url.to_owned()];
    let mut current = url.to_owned();
    for _ in 0..3 {
        let decoded = percent_decode(&current);
        if decoded == current {
            break;
        }
        candidates.push(decoded.clone());
        current = decoded;
    }

    let mut ports = Vec::new();
    for candidate in candidates {
        collect_loopback_ports(&candidate, "localhost:", &mut ports);
        collect_loopback_ports(&candidate, "127.0.0.1:", &mut ports);
        collect_loopback_ports(&candidate, "[::1]:", &mut ports);
    }
    ports.sort_unstable();
    ports.dedup();
    ports
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            decoded.push(high << 4 | low);
            index += 3;
            continue;
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn collect_loopback_ports(value: &str, marker: &str, ports: &mut Vec<u16>) {
    let mut offset = 0;
    while let Some(position) = value[offset..].find(marker) {
        let port_start = offset + position + marker.len();
        let port_digits = value[port_start..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>();
        if let Ok(port) = port_digits.parse::<u16>()
            && port != 0
        {
            ports.push(port);
        }
        offset = port_start
            .saturating_add(port_digits.len())
            .max(offset + position + 1);
    }
}

struct CallbackForwarder {
    shutdown: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Drop for CallbackForwarder {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn ensure_callback_forwarders(
    forwarders: &Arc<Mutex<BTreeMap<u16, CallbackForwarder>>>,
    container_name: &str,
    ports: Vec<u16>,
) -> Result<()> {
    if ports.is_empty() {
        return Ok(());
    }

    let mut forwarders = forwarders
        .lock()
        .map_err(|_| anyhow::anyhow!("callback forwarder lock is poisoned"))?;
    for port in ports {
        if forwarders.contains_key(&port) {
            continue;
        }
        forwarders.insert(port, CallbackForwarder::start(container_name, port)?);
    }
    Ok(())
}

impl CallbackForwarder {
    fn start(container_name: &str, port: u16) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port))
            .with_context(|| format!("failed to bind host callback port {port}"))?;
        listener
            .set_nonblocking(true)
            .context("failed to configure callback forwarder listener")?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = shutdown.clone();
        let container_name = container_name.to_owned();
        let join = thread::spawn(move || {
            while !thread_shutdown.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let container_name = container_name.clone();
                        thread::spawn(move || {
                            let _ = forward_callback_connection(stream, &container_name, port);
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(25));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            shutdown,
            join: Some(join),
        })
    }
}

fn forward_callback_connection(
    mut stream: TcpStream,
    container_name: &str,
    port: u16,
) -> Result<()> {
    let request = normalize_callback_request(
        read_http_message(&mut stream).context("failed to read callback request")?,
    )?;
    let mut child = Command::new("docker")
        .args([
            "exec",
            "-i",
            container_name,
            "sh",
            "-lc",
            &format!("exec socat STDIO TCP:127.0.0.1:{port}"),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to start callback bridge in container")?;
    let mut child_stdin = child
        .stdin
        .take()
        .context("callback bridge stdin is unavailable")?;
    let mut child_stdout = child
        .stdout
        .take()
        .context("callback bridge stdout is unavailable")?;
    child_stdin
        .write_all(&request)
        .context("failed to forward callback request to container")?;
    drop(child_stdin);
    io::copy(&mut child_stdout, &mut stream)
        .context("failed to forward callback response from container")?;
    let _ = child.kill();
    let _ = child.wait();
    Ok(())
}

fn read_http_message(stream: &mut TcpStream) -> Result<Vec<u8>> {
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let count = stream
            .read(&mut chunk)
            .context("failed to read HTTP request")?;
        if count == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..count]);
        if buffer.len() > 1_048_576 {
            anyhow::bail!("HTTP request is too large");
        }
        if let Some((header_end, content_length)) = browser_request_shape(&buffer)?
            && buffer.len() >= header_end + content_length
        {
            break;
        }
    }
    if buffer.is_empty() {
        anyhow::bail!("HTTP request is empty");
    }
    Ok(buffer)
}

fn normalize_callback_request(request: Vec<u8>) -> Result<Vec<u8>> {
    let Some(header_end) = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + 4)
    else {
        anyhow::bail!("HTTP request is missing headers");
    };
    let headers = String::from_utf8_lossy(&request[..header_end]);
    let mut lines = headers.lines();
    let request_line = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("HTTP request line is missing"))?;
    let body = &request[header_end..];
    let mut normalized = Vec::with_capacity(request.len() + 64);
    write!(normalized, "{request_line}\r\n").context("failed to normalize callback request")?;
    write!(normalized, "Connection: close\r\n")
        .context("failed to normalize callback connection")?;
    for line in lines {
        let Some((name, _)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.eq_ignore_ascii_case("connection")
            || name.eq_ignore_ascii_case("proxy-connection")
            || name.eq_ignore_ascii_case("keep-alive")
            || name.eq_ignore_ascii_case("transfer-encoding")
            || name.eq_ignore_ascii_case("upgrade")
        {
            continue;
        }
        write!(normalized, "{line}\r\n").context("failed to copy callback header")?;
    }
    normalized.extend_from_slice(b"\r\n");
    normalized.extend_from_slice(body);
    Ok(normalized)
}

#[cfg(target_os = "macos")]
fn open_host_browser(url: &str) -> Result<()> {
    spawn_browser_command("open", [url])
}

#[cfg(target_os = "windows")]
fn open_host_browser(url: &str) -> Result<()> {
    spawn_browser_command("cmd", ["/C", "start", "", url])
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_host_browser(url: &str) -> Result<()> {
    spawn_browser_command("xdg-open", [url])
        .or_else(|_| spawn_browser_command("gio", ["open", url]))
}

fn spawn_browser_command<const N: usize>(program: &str, args: [&str; N]) -> Result<()> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to execute {program}"))?;
    Ok(())
}

fn initialize_cargo_cache(spec: &RunSpec) -> Result<()> {
    if !spec.cargo_cache {
        return Ok(());
    }

    initialize_owned_volume(spec, "agentbox-cargo-cache", "/cache", "Cargo cache")
}

fn initialize_gui_cache(spec: &RunSpec) -> Result<()> {
    if !spec.gui_cache {
        return Ok(());
    }

    initialize_owned_volume(spec, "agentbox-gui-cache", "/cache", "GUI cache")?;
    initialize_owned_volume(spec, "agentbox-gui-local", "/cache", "GUI local data")
}

fn initialize_desktop_caches(spec: &RunSpec) -> Result<()> {
    if spec.codex_desktop_cache {
        initialize_owned_volume(
            spec,
            "agentbox-codex-desktop",
            "/cache",
            "Codex Desktop cache",
        )?;
    }
    if spec.claude_desktop_cache {
        initialize_owned_volume(
            spec,
            "agentbox-claude-desktop",
            "/cache",
            "Claude Desktop cache",
        )?;
    }
    Ok(())
}

fn initialize_owned_volume(spec: &RunSpec, volume: &str, target: &str, label: &str) -> Result<()> {
    let output = Command::new("docker")
        .args(volume_init_args(spec, volume, target))
        .output()
        .with_context(|| format!("failed to initialize the {label} volume"))?;
    if !output.status.success() {
        anyhow::bail!(
            "failed to make the {label} volume writable by container user {}: {}",
            spec.uid_gid,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn volume_init_args(spec: &RunSpec, volume: &str, target: &str) -> Vec<OsString> {
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
        format!("type=volume,src={volume},dst={target}").into(),
        "--entrypoint".into(),
        "/bin/sh".into(),
        "--".into(),
        spec.runtime_image.clone().into(),
        "-c".into(),
        r#"owner="$(stat -c '%u:%g' "$2")"; if [ "$owner" != "$1" ]; then chown -R "$1" "$2"; fi"#
            .into(),
        "agentbox-volume-init".into(),
        spec.uid_gid.clone().into(),
        target.into(),
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
        assert!(rendered.contains("GIT_CONFIG_COUNT=1"));
        assert!(rendered.contains("GIT_CONFIG_KEY_0=safe.directory"));
        assert!(rendered.contains("GIT_CONFIG_VALUE_0=/workspace"));
        assert!(rendered.contains("--add-host host.docker.internal:host-gateway"));
        assert!(rendered.contains("AGENTBOX_HOST_BROWSER_URL=http://host.docker.internal:"));
        assert!(rendered.contains("AGENTBOX_HOST_BROWSER_TOKEN=<redacted>"));
        assert!(rendered.contains("BROWSER=/usr/local/bin/agentbox-open"));
        assert!(spec.host_browser);
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
    fn host_browser_bridge_can_be_disabled() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.host_browser.enabled = false;
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
        let rendered = format_command(&spec);

        assert!(!rendered.contains("AGENTBOX_HOST_BROWSER_URL"));
        assert!(!rendered.contains("host.docker.internal:host-gateway"));
        assert!(!spec.host_browser);
    }

    #[test]
    fn host_browser_bridge_is_skipped_without_network() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.network.mode = NetworkMode::None;
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
        let rendered = format_command(&spec);

        assert!(rendered.contains("--network none"));
        assert!(!rendered.contains("AGENTBOX_HOST_BROWSER_URL"));
        assert!(!spec.host_browser);
    }

    #[test]
    fn loopback_callback_ports_are_extracted_from_auth_urls() {
        let url = "https://auth.example/start?redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fcallback&other=http%253A%252F%252F127.0.0.1%253A4567%252Fdone";

        assert_eq!(loopback_callback_ports(url), vec![1455, 4567]);
    }

    #[test]
    fn callback_requests_are_normalized_for_container_loopback() {
        let request = b"GET /callback?code=abc HTTP/1.1\r\nHost: 127.0.0.1:9999\r\nConnection: keep-alive\r\nUser-Agent: test\r\n\r\n".to_vec();

        let normalized = String::from_utf8(normalize_callback_request(request).unwrap()).unwrap();

        assert!(normalized.starts_with("GET /callback?code=abc HTTP/1.1\r\n"));
        assert!(normalized.contains("\r\nHost: 127.0.0.1:9999\r\n"));
        assert!(normalized.contains("\r\nConnection: close\r\n"));
        assert!(normalized.contains("\r\nUser-Agent: test\r\n"));
        assert!(!normalized.contains("keep-alive"));
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
    fn linked_worktree_mounts_common_git_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let main_git = temp.path().join("main/.git");
        let worktree = temp.path().join("feature");
        let worktree_git = main_git.join("worktrees/feature");
        std::fs::create_dir_all(&worktree_git).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(worktree_git.join("commondir"), "../..").unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", worktree_git.display()),
        )
        .unwrap();

        let config = Config::default();
        let spec = build_run_spec(BuildInput {
            config: &config,
            repo_root: &worktree,
            workspace: &worktree,
            host_home: None,
            compose: None,
            environment: BTreeMap::new(),
            command: vec!["true".into()],
            interactive: false,
            session_id: None,
        })
        .unwrap();
        let rendered = format_command(&spec);

        assert!(rendered.contains(&format!(
            "type=bind,src={},dst={}",
            main_git.display(),
            main_git.display()
        )));
        assert!(!rendered.contains(&format!(
            "type=bind,src={},dst={}",
            worktree_git.display(),
            worktree_git.display()
        )));
    }

    #[test]
    fn relative_worktree_gitdir_mounts_at_container_relative_target() {
        let temp = tempfile::tempdir().unwrap();
        let main_git = temp.path().join("main/.git");
        let worktree = temp.path().join("feature");
        let worktree_git = main_git.join("worktrees/feature");
        std::fs::create_dir_all(&worktree_git).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(worktree_git.join("commondir"), "../..").unwrap();
        std::fs::write(
            &worktree.join(".git"),
            "gitdir: ../main/.git/worktrees/feature\n",
        )
        .unwrap();

        let paths = linked_worktree_git_paths(&worktree, Path::new("/workspace"))
            .unwrap()
            .unwrap();

        assert_eq!(paths.git_dir, worktree_git);
        assert_eq!(paths.git_target, Path::new("/main/.git/worktrees/feature"));
        assert_eq!(paths.common_dir, main_git);
        assert_eq!(paths.common_target, Path::new("/main/.git"));
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
        let rendered = volume_init_args(&spec, "agentbox-cargo-cache", "/cache")
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");

        assert!(rendered.contains("--network none"));
        assert!(rendered.contains("--cap-drop ALL --cap-add CHOWN"));
        assert!(rendered.contains("--user 0:0"));
        assert!(rendered.contains(&format!("-- {}", spec.runtime_image)));
        assert!(rendered.contains("agentbox-cargo-cache"));
        assert!(rendered.ends_with(&format!("{} /cache", spec.uid_gid)));
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
    fn gui_passthrough_mounts_x11_and_imports_codex_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let x11_socket = temp.path().join(".X11-unix");
        let xauthority = temp.path().join("xauthority");
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::create_dir_all(&x11_socket).unwrap();
        std::fs::write(home.join(".codex/auth.json"), "{}").unwrap();
        std::fs::write(&xauthority, "cookie").unwrap();
        let mut config = Config::default();
        config.gui.enabled = true;
        config.gui.wayland = false;
        config.gui.display = ":99".into();
        config.gui.x11_socket = x11_socket.clone();
        config.gui.xauthority = xauthority.clone();

        let spec = build_run_spec(BuildInput {
            config: &config,
            repo_root: temp.path(),
            workspace: temp.path(),
            host_home: Some(&home),
            compose: None,
            environment: BTreeMap::new(),
            command: vec!["/opt/codex-app/start.sh".into()],
            interactive: false,
            session_id: None,
        })
        .unwrap();
        let rendered = format_command(&spec);

        assert!(rendered.contains(&format!(
            "src={},dst=/tmp/.X11-unix,readonly",
            x11_socket.display()
        )));
        assert!(rendered.contains("DISPLAY=:99"));
        assert!(rendered.contains("XAUTHORITY=/tmp/agentbox-xauthority"));
        assert!(rendered.contains("AGENTBOX_GUI=1"));
        assert!(rendered.contains("agentbox-gui-cache"));
        assert!(rendered.contains("/home/agent/.cache"));
        assert!(rendered.contains("agentbox-gui-local"));
        assert!(rendered.contains("/home/agent/.local"));
        assert!(!rendered.contains("agentbox-pnpm-cache"));
        assert!(rendered.contains("agentbox-codex-desktop"));
        assert!(rendered.contains("/agentbox/codex-desktop"));
        assert!(!rendered.contains("agentbox-claude-desktop"));
        assert!(!rendered.contains("/agentbox/claude-desktop"));
        assert!(!rendered.contains("agentbox-pip-cache"));
        assert!(rendered.contains("AGENTBOX_IMPORT_CREDENTIALS=codex"));
        assert!(spec.gui_cache);
        assert!(spec.codex_desktop_cache);
        assert!(!spec.claude_desktop_cache);
        assert_eq!(spec.imported_credentials.as_deref(), Some("codex"));
    }

    #[test]
    fn gui_credential_command_routes_by_desktop_app() {
        let mut config = Config::default();
        config.gui.enabled = true;

        // Claude Desktop launchers import Claude credentials.
        assert_eq!(
            credential_command(&config, &["claude-desktop".into()]),
            vec!["claude".to_string()]
        );
        assert_eq!(
            credential_command(
                &config,
                &["/agentbox/claude-desktop/run-claude-desktop.sh".into()],
            ),
            vec!["claude".to_string()]
        );

        // Codex Desktop and other GUI commands default to Codex credentials.
        assert_eq!(
            credential_command(&config, &["codex-desktop".into()]),
            vec!["codex".to_string()]
        );
        assert_eq!(
            credential_command(&config, &["codex-login".into()]),
            vec!["codex-login".to_string()],
            "device-auth helper should persist auth in the container home"
        );

        // Disabling an import leaves the command untouched.
        config.gui.import_claude_credentials = false;
        assert_eq!(
            credential_command(&config, &["claude-desktop".into()]),
            vec!["claude-desktop".to_string()]
        );
    }

    #[test]
    fn desktop_launchers_select_agent_specific_runtime_setup() {
        assert_eq!(
            update_agent(&["codex-desktop".into()]),
            Some("codex"),
            "Codex Desktop should receive the same CLI update preflight as Codex"
        );
        assert_eq!(
            update_agent(&["/agentbox/codex-desktop/run-codex-desktop.sh".into()]),
            Some("codex")
        );
        assert_eq!(update_agent(&["codex-login".into()]), Some("codex"));
        assert_eq!(update_agent(&["claude-desktop".into()]), None);
    }

    #[test]
    fn claude_desktop_mounts_only_when_requested() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let x11_socket = temp.path().join(".X11-unix");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::create_dir_all(&x11_socket).unwrap();
        std::fs::write(home.join(".claude/.credentials.json"), "{}").unwrap();
        let mut config = Config::default();
        config.gui.enabled = true;
        config.gui.wayland = false;
        config.gui.display = ":99".into();
        config.gui.x11_socket = x11_socket;

        let spec = build_run_spec(BuildInput {
            config: &config,
            repo_root: temp.path(),
            workspace: temp.path(),
            host_home: Some(&home),
            compose: None,
            environment: BTreeMap::new(),
            command: vec!["claude-desktop".into()],
            interactive: false,
            session_id: None,
        })
        .unwrap();
        let rendered = format_command(&spec);

        assert!(rendered.contains("agentbox-codex-desktop"));
        assert!(rendered.contains("agentbox-claude-desktop"));
        assert!(rendered.contains("/agentbox/claude-desktop"));
        assert!(rendered.contains("AGENTBOX_IMPORT_CREDENTIALS=claude"));
        assert!(spec.gui_cache);
        assert!(spec.codex_desktop_cache);
        assert!(spec.claude_desktop_cache);
        assert_eq!(spec.imported_credentials.as_deref(), Some("claude"));
    }

    #[test]
    fn claude_desktop_prefers_x11_when_both_gui_backends_exist() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let x11_socket = temp.path().join(".X11-unix");
        let wayland_socket = temp.path().join("wayland-1");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::create_dir_all(&x11_socket).unwrap();
        std::fs::write(home.join(".claude/.credentials.json"), "{}").unwrap();
        std::fs::write(&wayland_socket, "").unwrap();
        let mut config = Config::default();
        config.gui.enabled = true;
        config.gui.display = ":0".into();
        config.gui.x11_socket = x11_socket;
        config.gui.wayland_display = "wayland-1".into();
        config.gui.wayland_socket = wayland_socket.clone();

        let spec = build_run_spec(BuildInput {
            config: &config,
            repo_root: temp.path(),
            workspace: temp.path(),
            host_home: Some(&home),
            compose: None,
            environment: BTreeMap::new(),
            command: vec!["claude-desktop".into()],
            interactive: false,
            session_id: None,
        })
        .unwrap();
        let rendered = format_command(&spec);

        assert!(rendered.contains(&format!(
            "src={},dst=/tmp/.X11-unix,readonly",
            config.gui.x11_socket.display()
        )));
        assert!(rendered.contains("DISPLAY=:0"));
        assert!(!rendered.contains("WAYLAND_DISPLAY=agentbox-wayland-1"));
        assert!(!rendered.contains("CLAUDE_USE_WAYLAND=1"));
        assert!(!rendered.contains("XDG_SESSION_TYPE=wayland"));
        assert!(!rendered.contains("dst=/tmp/agentbox-wayland-1"));
    }

    #[test]
    fn claude_desktop_uses_wayland_when_x11_is_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let wayland_socket = temp.path().join("wayland-1");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(home.join(".claude/.credentials.json"), "{}").unwrap();
        std::fs::write(&wayland_socket, "").unwrap();
        let mut config = Config::default();
        config.gui.enabled = true;
        config.gui.x11 = false;
        config.gui.wayland_display = "wayland-1".into();
        config.gui.wayland_socket = wayland_socket.clone();

        let spec = build_run_spec(BuildInput {
            config: &config,
            repo_root: temp.path(),
            workspace: temp.path(),
            host_home: Some(&home),
            compose: None,
            environment: BTreeMap::new(),
            command: vec!["claude-desktop".into()],
            interactive: false,
            session_id: None,
        })
        .unwrap();
        let rendered = format_command(&spec);

        assert!(rendered.contains(&format!(
            "src={},dst=/tmp/agentbox-wayland-1,readonly",
            wayland_socket.display()
        )));
        assert!(rendered.contains("WAYLAND_DISPLAY=agentbox-wayland-1"));
        assert!(rendered.contains("CLAUDE_USE_WAYLAND=1"));
        assert!(rendered.contains("XDG_SESSION_TYPE=wayland"));
        assert!(!rendered.contains("dst=/tmp/.X11-unix"));
    }

    #[test]
    fn gui_passthrough_mounts_explicit_wayland_socket() {
        let temp = tempfile::tempdir().unwrap();
        let wayland_socket = temp.path().join("wayland-99");
        std::fs::write(&wayland_socket, "").unwrap();
        let mut config = Config::default();
        config.gui.enabled = true;
        config.gui.x11 = false;
        config.gui.wayland_display = "/run/user/1000/wayland-99".into();
        config.gui.wayland_socket = wayland_socket.clone();

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
        let rendered = format_command(&spec);

        assert!(rendered.contains(&format!(
            "src={},dst=/tmp/agentbox-run-user-1000-wayland-99,readonly",
            wayland_socket.display()
        )));
        assert!(rendered.contains("XDG_RUNTIME_DIR=/tmp"));
        assert!(rendered.contains("WAYLAND_DISPLAY=agentbox-run-user-1000-wayland-99"));
    }

    #[test]
    fn enabled_gui_requires_a_display_socket() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.gui.enabled = true;
        config.gui.x11 = false;
        config.gui.wayland = false;

        let error = build_run_spec(BuildInput {
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
        .unwrap_err()
        .to_string();

        assert!(error.contains("no usable X11 or Wayland socket"));
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
