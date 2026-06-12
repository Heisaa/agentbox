use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    io::IsTerminal,
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{Context, Result};

use crate::{
    compose::ComposeProject,
    config::{Config, HomeMode, NetworkMode},
};

#[derive(Debug, Clone)]
pub struct RunSpec {
    pub args: Vec<OsString>,
    pub network: String,
    pub environment: BTreeMap<String, String>,
    pub workspace_write: bool,
    pub uid_gid: String,
}

pub struct BuildInput<'a> {
    pub config: &'a Config,
    pub repo_root: &'a Path,
    pub workspace: &'a Path,
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
    let mut args = vec![OsString::from("run"), OsString::from("--rm")];

    if input.interactive {
        args.push(OsString::from("-i"));
        if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            args.push(OsString::from("-t"));
        }
    }

    let container_name = input
        .session_id
        .map(|id| format!("agentbox-{slug}-{}", container_suffix(id)))
        .unwrap_or_else(|| format!("agentbox-{slug}"));
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
        OsString::from("--security-opt"),
        OsString::from("no-new-privileges"),
        OsString::from("--cap-drop"),
        OsString::from("ALL"),
    ]);

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

    for (name, value) in &input.environment {
        args.push(OsString::from("--env"));
        args.push(OsString::from(format!("{name}={value}")));
    }
    args.push(OsString::from("--env"));
    args.push(OsString::from("HOME=/home/agent"));
    args.push(OsString::from("--env"));
    args.push(OsString::from("AGENTBOX=1"));
    args.push(OsString::from("--env"));
    args.push(OsString::from(format!(
        "AGENTBOX_AUTO_UPDATE={}",
        u8::from(config.runtime.auto_update)
    )));
    if let Some(agent) = update_agent(&input.command) {
        args.push(OsString::from("--env"));
        args.push(OsString::from(format!("AGENTBOX_UPDATE_AGENT={agent}")));
    }
    args.push(OsString::from("--env"));
    args.push(OsString::from(format!(
        "AGENTBOX_CAVEMAN={}",
        u8::from(config.caveman.enabled)
    )));

    args.push(OsString::from(&config.runtime.image));
    args.extend(input.command.iter().map(OsString::from));

    Ok(RunSpec {
        args,
        network,
        environment: input.environment,
        workspace_write: config.workspace.write,
        uid_gid,
    })
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

fn update_agent(command: &[String]) -> Option<&'static str> {
    match command.first().map(String::as_str) {
        Some("claude") => Some("claude"),
        Some("codex") => Some("codex"),
        _ => None,
    }
}

fn add_volume(args: &mut Vec<OsString>, source: &str, target: &str) {
    args.push(OsString::from("--mount"));
    args.push(OsString::from(format!(
        "type=volume,src={source},dst={target}"
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
    let status = Command::new("docker")
        .args(&spec.args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to execute docker; is Docker installed and running?")?;
    Ok(status.code().unwrap_or(1).clamp(0, 255) as u8)
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
        assert!(rendered.contains("--network bridge"));
        assert!(rendered.contains("type=bind,src="));
        assert!(!rendered.contains(",rw"));
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
    fn caveman_can_be_enabled() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.caveman.enabled = true;
        let spec = build_run_spec(BuildInput {
            config: &config,
            repo_root: temp.path(),
            workspace: temp.path(),
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
