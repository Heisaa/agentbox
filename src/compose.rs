use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct ComposeProject {
    pub name: String,
    pub network: String,
    pub services: Vec<String>,
}

pub fn command(repo_root: &Path, files: &[PathBuf]) -> Command {
    let mut command = Command::new("docker");
    command.arg("compose");
    for file in files {
        command.arg("-f").arg(file);
    }
    command.current_dir(repo_root);
    command
}

pub fn inspect(repo_root: &Path, files: &[PathBuf]) -> Result<Option<ComposeProject>> {
    if files.is_empty() {
        return Ok(None);
    }

    let output = command(repo_root, files)
        .args(["config", "--format", "json"])
        .stderr(Stdio::piped())
        .output()
        .context("failed to execute docker compose")?;
    if !output.status.success() {
        anyhow::bail!(
            "docker compose config failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let value: Value = serde_json::from_slice(&output.stdout)
        .context("invalid JSON from docker compose config")?;

    let name = value
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| crate::project::project_slug(repo_root));
    let services = value
        .get("services")
        .and_then(Value::as_object)
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default();
    let network = value
        .pointer("/networks/default/name")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{name}_default"));

    Ok(Some(ComposeProject {
        name,
        network,
        services,
    }))
}

pub fn run_action(
    repo_root: &Path,
    files: &[PathBuf],
    action: &str,
    services: &[String],
) -> Result<u8> {
    if files.is_empty() {
        anyhow::bail!("no Compose file configured or detected");
    }
    let mut command = command(repo_root, files);
    match action {
        "up" => {
            command.args(["up", "-d"]);
            command.args(services);
        }
        "down" => {
            command.arg("down");
        }
        _ => anyhow::bail!("unsupported Compose action {action}"),
    }
    let status = command
        .status()
        .with_context(|| format!("failed to execute docker compose {action}"))?;
    Ok(status.code().unwrap_or(1).clamp(0, 255) as u8)
}
