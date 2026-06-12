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

pub fn inspect(
    repo_root: &Path,
    files: &[PathBuf],
    requested_network: Option<&str>,
) -> Result<Option<ComposeProject>> {
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
    let network = select_network(&value, requested_network)?;

    Ok(Some(ComposeProject {
        name,
        network,
        services,
    }))
}

fn select_network(value: &Value, requested: Option<&str>) -> Result<String> {
    let networks = value
        .get("networks")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    if let Some(requested) = requested {
        if let Some(network) = networks.get(requested) {
            return Ok(network_name(requested, network));
        }
        if networks
            .iter()
            .any(|(key, network)| network_name(key, network) == requested)
        {
            return Ok(requested.to_owned());
        }
        anyhow::bail!(
            "configured Compose network `{requested}` was not found; available networks: {}",
            display_networks(&networks)
        );
    }

    if let Some(network) = networks.get("default") {
        return Ok(network_name("default", network));
    }

    let mut used = std::collections::BTreeSet::new();
    if let Some(services) = value.get("services").and_then(Value::as_object) {
        for service in services.values() {
            if let Some(service_networks) = service.get("networks") {
                match service_networks {
                    Value::Object(networks) => used.extend(networks.keys().cloned()),
                    Value::Array(networks) => {
                        used.extend(networks.iter().filter_map(Value::as_str).map(str::to_owned))
                    }
                    _ => {}
                }
            }
        }
    }

    if used.len() == 1 {
        let key = used.into_iter().next().expect("one network was found");
        if let Some(network) = networks.get(&key) {
            return Ok(network_name(&key, network));
        }
    }

    anyhow::bail!(
        "could not choose a Compose network automatically; set `network.compose_network` to one \
         of: {}",
        display_networks(&networks)
    )
}

fn network_name(key: &str, network: &Value) -> String {
    network
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(key)
        .to_owned()
}

fn display_networks(networks: &serde_json::Map<String, Value>) -> String {
    if networks.is_empty() {
        return "none".into();
    }
    networks
        .iter()
        .map(|(key, value)| {
            let name = network_name(key, value);
            if name == *key {
                key.clone()
            } else {
                format!("{key} ({name})")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn network_exists(network: &str) -> Result<bool> {
    let output = Command::new("docker")
        .args(["network", "inspect", network])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .context("failed to inspect Docker network")?;
    Ok(output.status.success())
}

pub fn ensure_network_exists(network: &str) -> Result<()> {
    if !network_exists(network)? {
        anyhow::bail!(
            "Compose network `{network}` is unavailable; run `agentbox up` first or select an \
             existing network with `network.compose_network`"
        );
    }
    Ok(())
}

pub fn list_services(repo_root: &Path, files: &[PathBuf]) -> Result<Vec<String>> {
    if files.is_empty() {
        anyhow::bail!("no Compose file configured or detected");
    }
    let output = command(repo_root, files)
        .args(["config", "--services"])
        .stderr(Stdio::piped())
        .output()
        .context("failed to list Docker Compose services")?;
    if !output.status.success() {
        anyhow::bail!(
            "docker compose config --services failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|service| !service.is_empty())
        .map(str::to_owned)
        .collect())
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn selects_default_compose_network() {
        let config = json!({
            "networks": {
                "default": {"name": "demo_default"},
                "backend": {"name": "demo_backend"}
            },
            "services": {}
        });
        assert_eq!(select_network(&config, None).unwrap(), "demo_default");
    }

    #[test]
    fn selects_sole_named_service_network() {
        let config = json!({
            "networks": {
                "backend": {"name": "shared_backend"}
            },
            "services": {
                "api": {"networks": {"backend": null}},
                "db": {"networks": ["backend"]}
            }
        });
        assert_eq!(select_network(&config, None).unwrap(), "shared_backend");
    }

    #[test]
    fn ambiguous_networks_require_explicit_selection() {
        let config = json!({
            "networks": {
                "frontend": {"name": "demo_frontend"},
                "backend": {"name": "demo_backend"}
            },
            "services": {
                "api": {"networks": {"frontend": null, "backend": null}}
            }
        });
        let error = select_network(&config, None).unwrap_err().to_string();
        assert!(error.contains("network.compose_network"));
    }

    #[test]
    fn explicit_network_accepts_compose_key_or_resolved_name() {
        let config = json!({
            "networks": {
                "backend": {"name": "shared_backend"}
            },
            "services": {}
        });
        assert_eq!(
            select_network(&config, Some("backend")).unwrap(),
            "shared_backend"
        );
        assert_eq!(
            select_network(&config, Some("shared_backend")).unwrap(),
            "shared_backend"
        );
    }
}
