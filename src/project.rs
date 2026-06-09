use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result};
use serde_json::Value;

const COMPOSE_CANDIDATES: &[&str] = &[
    "compose.yml",
    "compose.yaml",
    "docker-compose.yml",
    "docker-compose.yaml",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectTools {
    pub package_manager: Option<String>,
    pub rust: bool,
}

pub fn find_repo_root() -> Result<PathBuf> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    find_repo_root_from(&cwd)
}

pub fn find_repo_root_from(start: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(start)
        .output();

    if let Ok(output) = output
        && output.status.success()
    {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !path.is_empty() {
            return fs::canonicalize(&path)
                .with_context(|| format!("failed to resolve repository root {path}"));
        }
    }

    fs::canonicalize(start).with_context(|| format!("failed to resolve {}", start.display()))
}

pub fn detect_compose_files(repo_root: &Path) -> Vec<PathBuf> {
    COMPOSE_CANDIDATES
        .iter()
        .map(PathBuf::from)
        .filter(|path| repo_root.join(path).is_file())
        .collect()
}

pub fn project_slug(repo_root: &Path) -> String {
    let raw = repo_root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("project");
    let slug: String = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    slug.trim_matches('-').to_owned().chars().take(40).collect()
}

pub fn detect_tools(repo_root: &Path) -> ProjectTools {
    ProjectTools {
        package_manager: detect_package_manager(repo_root),
        rust: repo_root.join("Cargo.toml").is_file()
            || repo_root.join("rust-toolchain").is_file()
            || repo_root.join("rust-toolchain.toml").is_file(),
    }
}

fn detect_package_manager(repo_root: &Path) -> Option<String> {
    if let Ok(contents) = fs::read_to_string(repo_root.join("package.json"))
        && let Ok(value) = serde_json::from_str::<Value>(&contents)
        && let Some(manager) = value.get("packageManager").and_then(Value::as_str)
    {
        let name = manager.split('@').next().unwrap_or(manager);
        if matches!(name, "pnpm" | "npm" | "yarn") {
            return Some(name.into());
        }
    }

    [
        ("pnpm-lock.yaml", "pnpm"),
        ("pnpm-workspace.yaml", "pnpm"),
        ("yarn.lock", "yarn"),
        ("package-lock.json", "npm"),
        ("npm-shrinkwrap.json", "npm"),
    ]
    .into_iter()
    .find(|(file, _)| repo_root.join(file).is_file())
    .map(|(_, manager)| manager.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_is_docker_friendly() {
        assert_eq!(
            project_slug(Path::new("/tmp/My Cool_Project")),
            "my-cool-project"
        );
    }

    #[test]
    fn detects_pnpm_from_lockfile() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("pnpm-lock.yaml"),
            "lockfileVersion: '9.0'\n",
        )
        .unwrap();
        assert_eq!(
            detect_tools(temp.path()).package_manager.as_deref(),
            Some("pnpm")
        );
    }

    #[test]
    fn package_manager_field_takes_precedence() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("package.json"),
            r#"{"packageManager":"pnpm@10.12.1"}"#,
        )
        .unwrap();
        fs::write(temp.path().join("yarn.lock"), "").unwrap();
        assert_eq!(
            detect_tools(temp.path()).package_manager.as_deref(),
            Some("pnpm")
        );
    }

    #[test]
    fn detects_rust_project() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\n",
        )
        .unwrap();
        assert!(detect_tools(temp.path()).rust);
    }
}
