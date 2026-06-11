use std::{
    collections::{BTreeMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::config::{Config, NetworkMode};

pub const SENSITIVE_ENV_VARS: &[&str] = &[
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "KUBECONFIG",
    "SSH_AUTH_SOCK",
];

pub fn validate_config(config: &Config) -> Result<()> {
    if config.security.mount_host_home {
        anyhow::bail!("mount_host_home is not supported by the safe MVP");
    }
    if config.security.mount_docker_socket {
        anyhow::bail!("mount_docker_socket is not supported by the safe MVP");
    }
    if config.security.pass_ssh_agent {
        anyhow::bail!("pass_ssh_agent is not supported by the safe MVP");
    }
    if config.security.allow_host_network {
        anyhow::bail!("host networking is not supported by the safe MVP");
    }
    if !config.workspace.container_path.starts_with('/') {
        anyhow::bail!("workspace.container_path must be absolute");
    }
    if config.headroom.enabled {
        if config.network.mode != NetworkMode::Compose {
            anyhow::bail!("headroom.enabled requires network.mode = \"compose\"");
        }
        if config.headroom.service.trim().is_empty() {
            anyhow::bail!("headroom.service must not be empty");
        }
        if !config.headroom.url.starts_with("http://")
            && !config.headroom.url.starts_with("https://")
        {
            anyhow::bail!("headroom.url must start with http:// or https://");
        }
    }
    if !config.network.internet && config.network.mode != NetworkMode::None {
        anyhow::bail!(
            "internet = false requires network.mode = \"none\" in the MVP; Docker Compose \
             networks do not reliably isolate external traffic"
        );
    }
    Ok(())
}

pub fn resolve_workspace(repo_root: &Path, mount: &Path) -> Result<PathBuf> {
    let canonical_repo = fs::canonicalize(repo_root)
        .with_context(|| format!("failed to resolve repository root {}", repo_root.display()))?;
    let path = if mount.is_absolute() {
        mount.to_path_buf()
    } else {
        canonical_repo.join(mount)
    };
    let canonical = fs::canonicalize(&path)
        .with_context(|| format!("workspace mount does not exist: {}", path.display()))?;
    validate_workspace_path(&canonical)?;
    if !canonical.starts_with(&canonical_repo) {
        anyhow::bail!(
            "workspace mount {} is outside repository {}; only the repository or one of its \
             subdirectories may be mounted",
            canonical.display(),
            canonical_repo.display()
        );
    }
    Ok(canonical)
}

pub fn validate_workspace_path(path: &Path) -> Result<()> {
    if path == Path::new("/") {
        anyhow::bail!("refusing to mount the filesystem root");
    }
    if let Some(home) = dirs::home_dir()
        && path == home
    {
        anyhow::bail!("refusing to mount the entire host home directory");
    }
    if path.parent().is_none() {
        anyhow::bail!(
            "refusing suspiciously broad workspace mount {}",
            path.display()
        );
    }
    Ok(())
}

pub fn collect_environment(
    config: &Config,
    repo_root: &Path,
    allowed_secrets: &[String],
) -> Result<BTreeMap<String, String>> {
    let explicit: HashSet<&str> = allowed_secrets.iter().map(String::as_str).collect();
    let mut values = BTreeMap::new();

    for (name, value) in &config.env.defaults {
        let rendered = value
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| value.to_string());
        values.insert(name.clone(), rendered);
    }

    if let Some(file) = &config.env.file {
        let path = repo_root.join(file);
        if path.exists() {
            for (name, value) in parse_env_file(&path)? {
                values.insert(name, value);
            }
        }
    }

    for name in &config.env.allow {
        if let Ok(value) = env::var(name) {
            values.insert(name.clone(), value);
        }
    }

    for name in values.keys() {
        if is_sensitive(name) && !explicit.contains(name.as_str()) {
            anyhow::bail!(
                "refusing to pass sensitive variable {name}; repeat with --allow-secret {name}"
            );
        }
    }

    Ok(values)
}

pub(crate) fn is_sensitive(name: &str) -> bool {
    SENSITIVE_ENV_VARS.contains(&name)
        || name.ends_with("_TOKEN")
        || name.ends_with("_PASSWORD")
        || name.ends_with("_SECRET")
        || name.ends_with("_PRIVATE_KEY")
}

fn parse_env_file(path: &Path) -> Result<BTreeMap<String, String>> {
    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut values = BTreeMap::new();
    for (index, raw_line) in contents.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((name, value)) = line.split_once('=') else {
            anyhow::bail!("invalid env entry at {}:{}", path.display(), index + 1);
        };
        let name = name.trim();
        if name.is_empty()
            || !name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        {
            anyhow::bail!("invalid env name at {}:{}", path.display(), index + 1);
        }
        let value = value.trim();
        let value = if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            &value[1..value.len() - 1]
        } else {
            value
        };
        values.insert(name.to_owned(), value.to_owned());
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_root_mount() {
        assert!(validate_workspace_path(Path::new("/")).is_err());
    }

    #[test]
    fn headroom_requires_compose_networking() {
        let mut config = Config::default();
        config.headroom.enabled = true;
        config.network.mode = NetworkMode::Bridge;

        assert!(
            validate_config(&config)
                .unwrap_err()
                .to_string()
                .contains("network.mode = \"compose\"")
        );
    }

    #[test]
    fn workspace_must_be_inside_repository() {
        let repo = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        assert!(resolve_workspace(repo.path(), outside.path()).is_err());

        let nested = repo.path().join("nested");
        fs::create_dir(&nested).unwrap();
        assert_eq!(
            resolve_workspace(repo.path(), Path::new("nested")).unwrap(),
            nested
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_symlink_cannot_escape_repository() {
        use std::os::unix::fs::symlink;

        let repo = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), repo.path().join("escape")).unwrap();
        assert!(resolve_workspace(repo.path(), Path::new("escape")).is_err());
    }

    #[test]
    fn parses_basic_env_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("env");
        fs::write(&path, "A=one\nexport B=\"two words\"\n# comment\n").unwrap();
        let values = parse_env_file(&path).unwrap();
        assert_eq!(values["A"], "one");
        assert_eq!(values["B"], "two words");
    }

    #[test]
    fn sensitive_defaults_require_explicit_approval() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.env.defaults.insert(
            "OPENAI_API_KEY".into(),
            toml::Value::String("test-only".into()),
        );
        assert!(collect_environment(&config, temp.path(), &[]).is_err());
        let values = collect_environment(&config, temp.path(), &["OPENAI_API_KEY".into()]).unwrap();
        assert_eq!(values["OPENAI_API_KEY"], "test-only");
    }
}
