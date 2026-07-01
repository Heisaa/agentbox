use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const CONFIG_DIR: &str = ".agentbox";
pub const CONFIG_FILE: &str = ".agentbox/config.toml";
const DYNAMIC_TABLES: &[&str] = &["env.defaults"];
const DEPRECATED_KEYS: &[(&str, Option<&str>)] = &[
    ("agent.allow_git_write", None),
    ("agent.allow_network", Some("network.internet")),
    ("project.workdir", Some("workspace.container_path")),
    ("project.mount", Some("workspace.mount")),
    ("security.allow_git_mutation", None),
    ("network.auto_detect_project", None),
];

#[derive(Debug)]
pub struct LoadedConfig {
    pub config: Config,
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub struct ConfigUpdate {
    pub path: PathBuf,
    pub backup_path: PathBuf,
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

#[derive(Debug)]
pub struct RuntimeImageUpdate {
    pub path: PathBuf,
    pub dockerfile: PathBuf,
}

#[derive(Debug)]
pub struct ConfigValueUpdate {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    pub workspace: WorkspaceConfig,
    pub agent: AgentConfig,
    pub caveman: CavemanConfig,
    pub host_browser: HostBrowserConfig,
    pub gui: GuiConfig,
    pub headroom: HeadroomConfig,
    pub security: SecurityConfig,
    pub network: NetworkConfig,
    pub env: EnvConfig,
    pub caches: CacheConfig,
    pub limits: LimitsConfig,
    pub runtime: RuntimeConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceConfig {
    pub mount: PathBuf,
    pub container_path: String,
    pub write: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub default: String,
    pub command: String,
    pub home: HomeMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CavemanConfig {
    pub enabled: bool,
    pub level: CavemanLevel,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HostBrowserConfig {
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GuiConfig {
    pub enabled: bool,
    pub import_codex_credentials: bool,
    pub import_claude_credentials: bool,
    pub x11: bool,
    pub wayland: bool,
    pub display: String,
    pub x11_socket: PathBuf,
    pub xauthority: PathBuf,
    pub wayland_display: String,
    pub wayland_socket: PathBuf,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CavemanLevel {
    Lite,
    #[default]
    Full,
    Ultra,
    Wenyan,
}

impl CavemanLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lite => "lite",
            Self::Full => "full",
            Self::Ultra => "ultra",
            Self::Wenyan => "wenyan",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HeadroomConfig {
    pub enabled: bool,
    pub service: String,
    pub url: String,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HomeMode {
    #[default]
    Persistent,
    Ephemeral,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityConfig {
    pub mount_host_home: bool,
    pub mount_docker_socket: bool,
    pub pass_ssh_agent: bool,
    pub allow_host_network: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkConfig {
    pub mode: NetworkMode,
    pub compose_files: Vec<PathBuf>,
    pub compose_network: String,
    pub internet: bool,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    #[default]
    Compose,
    Bridge,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EnvConfig {
    pub allow: Vec<String>,
    pub defaults: toml::Table,
    pub file: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheConfig {
    pub npm: bool,
    pub pnpm: bool,
    pub cargo: bool,
    pub pip: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LimitsConfig {
    pub cpus: f32,
    pub memory: String,
    pub pids: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeConfig {
    pub image: String,
    pub dockerfile: PathBuf,
    pub build_context: PathBuf,
    pub auto_update: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            workspace: WorkspaceConfig::default(),
            agent: AgentConfig::default(),
            caveman: CavemanConfig::default(),
            host_browser: HostBrowserConfig::default(),
            gui: GuiConfig::default(),
            headroom: HeadroomConfig::default(),
            security: SecurityConfig::default(),
            network: NetworkConfig::default(),
            env: EnvConfig::default(),
            caches: CacheConfig::default(),
            limits: LimitsConfig::default(),
            runtime: RuntimeConfig::default(),
        }
    }
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            mount: PathBuf::from("."),
            container_path: "/workspace".into(),
            write: true,
        }
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            default: "claude".into(),
            command: "claude".into(),
            home: HomeMode::Persistent,
        }
    }
}

impl Default for CavemanConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            level: CavemanLevel::Full,
        }
    }
}

impl Default for HostBrowserConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Default for GuiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            import_codex_credentials: true,
            import_claude_credentials: true,
            x11: true,
            wayland: true,
            display: String::new(),
            x11_socket: PathBuf::from("/tmp/.X11-unix"),
            xauthority: PathBuf::new(),
            wayland_display: String::new(),
            wayland_socket: PathBuf::new(),
        }
    }
}

impl Default for HeadroomConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            service: "headroom".into(),
            url: "http://headroom:8787".into(),
        }
    }
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            mode: NetworkMode::Compose,
            compose_files: Vec::new(),
            compose_network: String::new(),
            internet: true,
        }
    }
}

impl Default for EnvConfig {
    fn default() -> Self {
        let mut defaults = toml::Table::new();
        defaults.insert("NODE_ENV".into(), toml::Value::String("development".into()));
        Self {
            allow: vec!["NODE_ENV".into(), "RUST_LOG".into()],
            defaults,
            file: Some(PathBuf::from(".agentbox/env")),
        }
    }
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            npm: true,
            pnpm: true,
            cargo: true,
            pip: true,
        }
    }
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            cpus: 4.0,
            memory: "8g".into(),
            pids: 2048,
        }
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            image: "agentbox/fullstack:latest".into(),
            dockerfile: PathBuf::new(),
            build_context: PathBuf::from("."),
            auto_update: true,
        }
    }
}

impl Config {
    pub fn load(repo_root: &Path) -> Result<LoadedConfig> {
        let path = repo_root.join(CONFIG_FILE);
        if !path.exists() {
            return Ok(LoadedConfig {
                config: Self::default(),
                warnings: Vec::new(),
            });
        }
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let raw: toml::Value =
            toml::from_str(&contents).with_context(|| format!("invalid {}", path.display()))?;
        let config: Self =
            toml::from_str(&contents).with_context(|| format!("invalid {}", path.display()))?;
        if config.version != 1 {
            anyhow::bail!(
                "unsupported config version {} in {}; expected 1",
                config.version,
                path.display()
            );
        }
        let expected =
            toml::Value::try_from(Self::default()).context("failed to build config schema")?;
        let mut warnings = Vec::new();
        compare_schema("", &raw, &expected, &mut warnings);
        Ok(LoadedConfig { config, warnings })
    }

    pub fn write_new(&self, repo_root: &Path) -> Result<PathBuf> {
        let directory = repo_root.join(CONFIG_DIR);
        fs::create_dir_all(&directory)
            .with_context(|| format!("failed to create {}", directory.display()))?;
        let path = repo_root.join(CONFIG_FILE);
        if path.exists() {
            anyhow::bail!("{} already exists", path.display());
        }
        let contents = toml::to_string_pretty(self).context("failed to serialize config")?;
        fs::write(&path, contents)
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(path)
    }

    pub fn update(repo_root: &Path) -> Result<ConfigUpdate> {
        let path = repo_root.join(CONFIG_FILE);
        if !path.exists() {
            anyhow::bail!(
                "{} does not exist; run `agentbox init` first",
                path.display()
            );
        }

        let contents = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut actual: toml::Value =
            toml::from_str(&contents).with_context(|| format!("invalid {}", path.display()))?;
        let parsed: Self =
            toml::from_str(&contents).with_context(|| format!("invalid {}", path.display()))?;
        if parsed.version != 1 {
            anyhow::bail!(
                "unsupported config version {} in {}; expected 1",
                parsed.version,
                path.display()
            );
        }

        let expected =
            toml::Value::try_from(Self::default()).context("failed to build config schema")?;
        let mut added = Vec::new();
        merge_missing("", &mut actual, &expected, &mut added);

        let mut removed = Vec::new();
        for (deprecated, _) in DEPRECATED_KEYS {
            if remove_path(&mut actual, deprecated) {
                removed.push((*deprecated).to_owned());
            }
        }

        let updated = toml::to_string_pretty(&actual).context("failed to serialize config")?;
        let backup_path = path.with_extension("toml.bak");
        fs::copy(&path, &backup_path)
            .with_context(|| format!("failed to create {}", backup_path.display()))?;
        fs::write(&path, updated).with_context(|| format!("failed to write {}", path.display()))?;

        Ok(ConfigUpdate {
            path,
            backup_path,
            added,
            removed,
        })
    }

    pub fn enable_runtime_image(
        repo_root: &Path,
        image: &str,
        dockerfile: &Path,
        build_context: &Path,
    ) -> Result<RuntimeImageUpdate> {
        let path = repo_root.join(CONFIG_FILE);
        if !path.exists() {
            anyhow::bail!(
                "{} does not exist; run `agentbox init` first",
                path.display()
            );
        }

        let contents = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut actual: toml::Value =
            toml::from_str(&contents).with_context(|| format!("invalid {}", path.display()))?;
        let table = actual
            .as_table_mut()
            .context("agentbox config must contain a TOML table")?;
        let runtime = table
            .entry("runtime")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .context("runtime config must be a TOML table")?;
        runtime.insert("image".into(), toml::Value::String(image.into()));
        runtime.insert(
            "dockerfile".into(),
            toml::Value::String(dockerfile.to_string_lossy().into_owned()),
        );
        runtime.insert(
            "build_context".into(),
            toml::Value::String(build_context.to_string_lossy().into_owned()),
        );

        let updated = toml::to_string_pretty(&actual).context("failed to serialize config")?;
        fs::write(&path, updated).with_context(|| format!("failed to write {}", path.display()))?;

        Ok(RuntimeImageUpdate {
            path,
            dockerfile: repo_root.join(dockerfile),
        })
    }

    pub fn set_gui_enabled(repo_root: &Path, enabled: bool) -> Result<ConfigValueUpdate> {
        let path = repo_root.join(CONFIG_FILE);
        if !path.exists() {
            let mut config = Self::default();
            config.gui.enabled = enabled;
            return Ok(ConfigValueUpdate {
                path: config.write_new(repo_root)?,
            });
        }

        let contents = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut actual: toml::Value =
            toml::from_str(&contents).with_context(|| format!("invalid {}", path.display()))?;
        let table = actual
            .as_table_mut()
            .context("agentbox config must contain a TOML table")?;
        let gui = table
            .entry("gui")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .context("gui config must be a TOML table")?;
        gui.insert("enabled".into(), toml::Value::Boolean(enabled));

        let updated = toml::to_string_pretty(&actual).context("failed to serialize config")?;
        fs::write(&path, updated).with_context(|| format!("failed to write {}", path.display()))?;

        Ok(ConfigValueUpdate { path })
    }
}

fn merge_missing(
    path: &str,
    actual: &mut toml::Value,
    expected: &toml::Value,
    added: &mut Vec<String>,
) {
    let (Some(actual), Some(expected)) = (actual.as_table_mut(), expected.as_table()) else {
        return;
    };

    for (key, expected_value) in expected {
        let key_path = join_path(path, key);
        match actual.get_mut(key) {
            None => {
                actual.insert(key.clone(), expected_value.clone());
                added.push(key_path);
            }
            Some(actual_value) if expected_value.is_table() && actual_value.is_table() => {
                if !DYNAMIC_TABLES.contains(&key_path.as_str()) {
                    merge_missing(&key_path, actual_value, expected_value, added);
                }
            }
            Some(_) => {}
        }
    }
}

fn remove_path(value: &mut toml::Value, path: &str) -> bool {
    fn remove(table: &mut toml::Table, segments: &[&str]) -> bool {
        let [segment, rest @ ..] = segments else {
            return false;
        };
        if rest.is_empty() {
            return table.remove(*segment).is_some();
        }

        let (removed, empty) = {
            let Some(next) = table.get_mut(*segment).and_then(toml::Value::as_table_mut) else {
                return false;
            };
            let removed = remove(next, rest);
            (removed, removed && next.is_empty())
        };
        if empty {
            table.remove(*segment);
        }
        removed
    }

    let Some(table) = value.as_table_mut() else {
        return false;
    };
    remove(table, &path.split('.').collect::<Vec<_>>())
}

fn compare_schema(
    path: &str,
    actual: &toml::Value,
    expected: &toml::Value,
    warnings: &mut Vec<String>,
) {
    let (Some(actual), Some(expected)) = (actual.as_table(), expected.as_table()) else {
        return;
    };

    for (key, expected_value) in expected {
        let key_path = join_path(path, key);
        match actual.get(key) {
            None => warnings.push(format!(
                "missing `{key_path}`; using default {}",
                display_default(expected_value)
            )),
            Some(actual_value) if expected_value.is_table() && actual_value.is_table() => {
                if !DYNAMIC_TABLES.contains(&key_path.as_str()) {
                    compare_schema(&key_path, actual_value, expected_value, warnings);
                }
            }
            Some(_) => {}
        }
    }

    for key in actual.keys() {
        if expected.contains_key(key) {
            continue;
        }
        let key_path = join_path(path, key);
        if let Some((_, replacement)) = DEPRECATED_KEYS
            .iter()
            .find(|(deprecated, _)| *deprecated == key_path)
        {
            let message = match replacement {
                Some(replacement) => {
                    format!("deprecated `{key_path}` is ignored; use `{replacement}` instead")
                }
                None => format!("deprecated `{key_path}` is ignored and has no replacement"),
            };
            warnings.push(message);
        } else {
            warnings.push(format!("unknown `{key_path}` is ignored"));
        }
    }
}

fn join_path(parent: &str, key: &str) -> String {
    if parent.is_empty() {
        key.to_owned()
    } else {
        format!("{parent}.{key}")
    }
}

fn display_default(value: &toml::Value) -> String {
    if value.is_table() {
        "the built-in section defaults".into()
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_round_trips() {
        let encoded = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(encoded.contains("compose_network = \"\""));
        let decoded: Config = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded.version, 1);
        assert_eq!(decoded.workspace.container_path, "/workspace");
        assert!(!decoded.security.mount_host_home);
        assert!(!decoded.caveman.enabled);
        assert_eq!(decoded.caveman.level, CavemanLevel::Full);
        assert!(decoded.host_browser.enabled);
        assert!(!decoded.gui.enabled);
        assert!(decoded.gui.import_codex_credentials);
        assert!(decoded.gui.import_claude_credentials);
        assert!(decoded.gui.x11);
        assert!(decoded.gui.wayland);
        assert_eq!(decoded.gui.x11_socket, Path::new("/tmp/.X11-unix"));
        assert!(decoded.gui.xauthority.as_os_str().is_empty());
        assert!(decoded.gui.wayland_socket.as_os_str().is_empty());
        assert!(!decoded.headroom.enabled);
        assert_eq!(decoded.headroom.service, "headroom");
        assert!(decoded.runtime.dockerfile.as_os_str().is_empty());
        assert_eq!(decoded.runtime.build_context, Path::new("."));
        assert!(decoded.runtime.auto_update);
    }

    #[test]
    fn warns_for_missing_and_unknown_options() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join(CONFIG_DIR);
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            temp.path().join(CONFIG_FILE),
            "version = 1\nmystery = true\n\n[workspace]\nmount = \".\"\n",
        )
        .unwrap();

        let loaded = Config::load(temp.path()).unwrap();
        assert!(
            loaded
                .warnings
                .iter()
                .any(|warning| warning.contains("missing `workspace.container_path`"))
        );
        assert!(
            loaded
                .warnings
                .iter()
                .any(|warning| warning == "unknown `mystery` is ignored")
        );
    }

    #[test]
    fn allows_dynamic_environment_defaults() {
        let raw: toml::Value =
            toml::from_str("[env.defaults]\nCUSTOM_PROJECT_VALUE = \"yes\"\n").unwrap();
        let expected = toml::Value::try_from(Config::default()).unwrap();
        let mut warnings = Vec::new();
        compare_schema("", &raw, &expected, &mut warnings);
        assert!(
            !warnings
                .iter()
                .any(|warning| warning.contains("CUSTOM_PROJECT_VALUE"))
        );
    }

    #[test]
    fn deprecated_option_names_include_replacement() {
        let raw: toml::Value = toml::from_str("[agent]\nallow_network = true\n").unwrap();
        let expected = toml::Value::try_from(Config::default()).unwrap();
        let mut warnings = Vec::new();
        compare_schema("", &raw, &expected, &mut warnings);
        assert!(warnings.iter().any(|warning| {
            warning == "deprecated `agent.allow_network` is ignored; use `network.internet` instead"
        }));
    }

    #[test]
    fn removed_options_warn_without_claiming_to_work() {
        let raw: toml::Value = toml::from_str(
            "[security]\nallow_git_mutation = false\n\n[network]\nauto_detect_project = true\n",
        )
        .unwrap();
        let expected = toml::Value::try_from(Config::default()).unwrap();
        let mut warnings = Vec::new();
        compare_schema("", &raw, &expected, &mut warnings);
        assert!(warnings.iter().any(|warning| {
            warning == "deprecated `security.allow_git_mutation` is ignored and has no replacement"
        }));
        assert!(warnings.iter().any(|warning| {
            warning == "deprecated `network.auto_detect_project` is ignored and has no replacement"
        }));
    }

    #[test]
    fn update_preserves_values_and_unknown_keys_while_normalizing_schema() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join(CONFIG_DIR);
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            temp.path().join(CONFIG_FILE),
            "version = 1\n\
             custom = \"keep\"\n\
             \n\
             [agent]\n\
             default = \"codex\"\n\
             command = \"codex\"\n\
             home = \"ephemeral\"\n\
             allow_network = false\n\
             \n\
             [project]\n\
             workdir = \"/old-workspace\"\n\
             mount = \".\"\n\
             \n\
             [env.defaults]\n\
             CUSTOM_VALUE = \"yes\"\n",
        )
        .unwrap();

        let update = Config::update(temp.path()).unwrap();
        let raw: toml::Value = toml::from_str(&fs::read_to_string(&update.path).unwrap()).unwrap();

        assert_eq!(raw["custom"].as_str(), Some("keep"));
        assert_eq!(raw["agent"]["default"].as_str(), Some("codex"));
        assert!(raw["agent"].get("allow_network").is_none());
        assert!(raw.get("project").is_none());
        assert_eq!(raw["env"]["defaults"]["CUSTOM_VALUE"].as_str(), Some("yes"));
        assert!(raw["caveman"].is_table());
        assert!(raw["host_browser"].is_table());
        assert!(raw["gui"].is_table());
        assert!(raw["headroom"].is_table());
        assert!(update.added.contains(&"caveman".into()));
        assert!(update.added.contains(&"host_browser".into()));
        assert!(update.added.contains(&"gui".into()));
        assert!(update.removed.contains(&"agent.allow_network".into()));
        assert_eq!(
            fs::read_to_string(update.backup_path).unwrap(),
            "version = 1\ncustom = \"keep\"\n\n[agent]\ndefault = \"codex\"\ncommand = \"codex\"\nhome = \"ephemeral\"\nallow_network = false\n\n[project]\nworkdir = \"/old-workspace\"\nmount = \".\"\n\n[env.defaults]\nCUSTOM_VALUE = \"yes\"\n"
        );
    }

    #[test]
    fn update_requires_an_existing_config() {
        let temp = tempfile::tempdir().unwrap();
        let error = Config::update(temp.path()).unwrap_err().to_string();
        assert!(error.contains("agentbox init"));
    }

    #[test]
    fn enabling_runtime_image_preserves_unknown_config() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join(CONFIG_DIR)).unwrap();
        fs::write(
            temp.path().join(CONFIG_FILE),
            "version = 1\ncustom = \"keep\"\n",
        )
        .unwrap();

        Config::enable_runtime_image(
            temp.path(),
            "example/base:1",
            Path::new(".agentbox/Dockerfile"),
            Path::new("."),
        )
        .unwrap();
        let raw: toml::Value =
            toml::from_str(&fs::read_to_string(temp.path().join(CONFIG_FILE)).unwrap()).unwrap();

        assert_eq!(raw["custom"].as_str(), Some("keep"));
        assert_eq!(raw["runtime"]["image"].as_str(), Some("example/base:1"));
        assert_eq!(
            raw["runtime"]["dockerfile"].as_str(),
            Some(".agentbox/Dockerfile")
        );
        assert_eq!(raw["runtime"]["build_context"].as_str(), Some("."));
    }

    #[test]
    fn enabling_gui_preserves_unknown_config() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join(CONFIG_DIR)).unwrap();
        fs::write(
            temp.path().join(CONFIG_FILE),
            "version = 1\ncustom = \"keep\"\n",
        )
        .unwrap();

        let update = Config::set_gui_enabled(temp.path(), true).unwrap();
        let raw: toml::Value = toml::from_str(&fs::read_to_string(&update.path).unwrap()).unwrap();

        assert_eq!(raw["custom"].as_str(), Some("keep"));
        assert_eq!(raw["gui"]["enabled"].as_bool(), Some(true));
    }

    #[test]
    fn enabling_gui_creates_missing_config() {
        let temp = tempfile::tempdir().unwrap();

        let update = Config::set_gui_enabled(temp.path(), true).unwrap();
        let raw: toml::Value = toml::from_str(&fs::read_to_string(&update.path).unwrap()).unwrap();

        assert_eq!(update.path, temp.path().join(CONFIG_FILE));
        assert_eq!(raw["gui"]["enabled"].as_bool(), Some(true));
        assert_eq!(raw["version"].as_integer(), Some(1));
    }
}
