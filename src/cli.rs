use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

use crate::{
    compose::{self, ComposeProject},
    config::{Config, NetworkMode},
    docker::{self, BuildInput, RunSpec},
    project::{self, ProjectTools},
    security,
};

const AGENT_INSTRUCTIONS: &str = include_str!("../instructions/agent.md");
const PACKAGE_MANAGER_PLACEHOLDER: &str = "{{PACKAGE_MANAGER_INSTRUCTIONS}}";
const RUST_PLACEHOLDER: &str = "{{RUST_INSTRUCTIONS}}";

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Generate a safe project configuration.
    Init,
    /// Run a coding agent in the sandbox.
    Run(RunArgs),
    /// Open a shell in the sandbox.
    Shell(SandboxArgs),
    /// Start configured Docker Compose services.
    Up {
        /// Services to start; all services are started when omitted.
        services: Vec<String>,
    },
    /// Stop configured Docker Compose services.
    Down,
    /// Explain configuration, mounts, environment, and connectivity.
    Doctor(SandboxArgs),
    /// Print the exact docker run command without executing it.
    Explain(RunArgs),
}

#[derive(Debug, Clone, Args)]
struct SandboxArgs {
    /// Explicitly permit a sensitive variable named in the env allowlist.
    #[arg(long = "allow-secret", value_name = "NAME")]
    allow_secrets: Vec<String>,
}

#[derive(Debug, Clone, Default, Args)]
struct RunArgs {
    /// Agent executable or configured profile name.
    agent: Option<String>,
    /// Explicitly permit a sensitive variable named in the env allowlist.
    #[arg(long = "allow-secret", value_name = "NAME")]
    allow_secrets: Vec<String>,
    /// Arguments passed to the agent.
    #[arg(last = true)]
    arguments: Vec<String>,
}

struct AppContext {
    repo_root: PathBuf,
    config: Config,
    compose_files: Vec<PathBuf>,
    compose: Option<ComposeProject>,
    tools: ProjectTools,
}

pub fn run() -> Result<u8> {
    let cli = Cli::parse();
    match cli.command {
        None => run_agent(&RunArgs::default(), false),
        Some(Commands::Init) => init(),
        Some(Commands::Run(args)) => run_agent(&args, false),
        Some(Commands::Shell(args)) => run_shell(&args),
        Some(Commands::Up { services }) => compose_action("up", &services),
        Some(Commands::Down) => compose_action("down", &[]),
        Some(Commands::Doctor(args)) => doctor(&args),
        Some(Commands::Explain(args)) => run_agent(&args, true),
    }
}

fn init() -> Result<u8> {
    let repo_root = project::find_repo_root()?;
    security::validate_workspace_path(&repo_root)?;
    let compose_files = project::detect_compose_files(&repo_root);
    let mut config = Config::default();
    config.network.compose_files = compose_files.clone();
    if compose_files.is_empty() {
        config.network.mode = NetworkMode::Bridge;
    }
    let config_path = config.write_new(&repo_root)?;
    let directory = repo_root.join(".agentbox");
    let env_example = directory.join("env.example");
    let gitignore = directory.join(".gitignore");
    fs::write(
        &env_example,
        "# Local development values only. Copy to env to enable.\n\
         # DATABASE_URL=postgres://postgres:postgres@postgres:5432/app\n\
         # REDIS_URL=redis://redis:6379\n",
    )
    .with_context(|| format!("failed to write {}", env_example.display()))?;
    fs::write(&gitignore, "env\nenv.local\nsecrets/\n")
        .with_context(|| format!("failed to write {}", gitignore.display()))?;

    println!("Created:");
    println!("- {}", relative(&repo_root, &config_path));
    println!("- {}", relative(&repo_root, &env_example));
    println!("- {}", relative(&repo_root, &gitignore));
    if compose_files.is_empty() {
        println!("\nNo Compose file detected; network mode set to bridge.");
    } else {
        println!("\nDetected Compose files:");
        for file in compose_files {
            println!("- {}", file.display());
        }
    }
    Ok(0)
}

fn load_context(inspect_compose: bool) -> Result<AppContext> {
    let repo_root = project::find_repo_root()?;
    let loaded = Config::load(&repo_root)?;
    if !loaded.warnings.is_empty() {
        eprintln!("warning: {} may be outdated:", crate::config::CONFIG_FILE);
        for warning in &loaded.warnings {
            eprintln!("- {warning}");
        }
        eprintln!("Run `agentbox init` in a temporary directory to view the current schema.\n");
    }
    let config = loaded.config;
    security::validate_config(&config)?;
    let compose_files = if config.network.compose_files.is_empty() {
        project::detect_compose_files(&repo_root)
    } else {
        config.network.compose_files.clone()
    };
    let compose = if inspect_compose && config.network.mode == NetworkMode::Compose {
        compose::inspect(
            &repo_root,
            &compose_files,
            (!config.network.compose_network.is_empty())
                .then_some(config.network.compose_network.as_str()),
        )?
    } else {
        None
    };
    if config.headroom.enabled && inspect_compose {
        let compose = compose.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "headroom is enabled but no Compose project was found; add a `{}` service to a \
                 configured Compose file",
                config.headroom.service
            )
        })?;
        if !compose.services.contains(&config.headroom.service) {
            anyhow::bail!(
                "headroom is enabled but Compose service `{}` was not found",
                config.headroom.service
            );
        }
    }
    let tools = project::detect_tools(&repo_root);
    Ok(AppContext {
        repo_root,
        config,
        compose_files,
        compose,
        tools,
    })
}

fn run_agent(args: &RunArgs, explain: bool) -> Result<u8> {
    let context = load_context(true)?;
    let command = agent_command(&context.config, args, &context.tools)?;
    let spec = build_spec(&context, &args.allow_secrets, command, !explain)?;
    print_banner(&context, &spec);
    if explain {
        println!("\n{}", docker::format_command(&spec));
        Ok(0)
    } else {
        ensure_docker_available()?;
        ensure_compose_network(&context)?;
        docker::execute(&spec)
    }
}

fn run_shell(args: &SandboxArgs) -> Result<u8> {
    let context = load_context(true)?;
    let shell = vec!["bash".into()];
    let spec = build_spec(&context, &args.allow_secrets, shell, true)?;
    print_banner(&context, &spec);
    ensure_docker_available()?;
    ensure_compose_network(&context)?;
    docker::execute(&spec)
}

fn doctor(args: &SandboxArgs) -> Result<u8> {
    let context = load_context(true)?;
    let spec = build_spec(&context, &args.allow_secrets, vec!["true".into()], false)?;
    print_banner(&context, &spec);

    println!("\nDiagnostics:");
    println!("- Repository: {}", context.repo_root.display());
    println!(
        "- Config: {}",
        if context.repo_root.join(crate::config::CONFIG_FILE).exists() {
            crate::config::CONFIG_FILE
        } else {
            "defaults (run `agentbox init` to persist)"
        }
    );
    println!("- Docker CLI: {}", command_health("docker", &["--version"]));
    println!(
        "- Docker daemon: {}",
        command_health("docker", &["info", "--format", "{{.ServerVersion}}"])
    );
    println!(
        "- Compose: {}",
        command_health("docker", &["compose", "version"])
    );
    if let Some(compose) = &context.compose {
        println!("- Compose project: {}", compose.name);
        println!(
            "- Compose network: {} ({})",
            compose.network,
            command_health(
                "docker",
                &[
                    "network",
                    "inspect",
                    "--format",
                    "{{.Name}}",
                    &compose.network,
                ],
            )
        );
        println!(
            "- Services: {}",
            if compose.services.is_empty() {
                "none".into()
            } else {
                compose.services.join(", ")
            }
        );
    }
    println!("\nDocker command:\n{}", docker::format_command(&spec));
    Ok(0)
}

fn compose_action(action: &str, services: &[String]) -> Result<u8> {
    let context = load_context(false)?;
    ensure_docker_available()?;
    compose::run_action(&context.repo_root, &context.compose_files, action, services)
}

fn build_spec(
    context: &AppContext,
    allowed_secrets: &[String],
    command: Vec<String>,
    interactive: bool,
) -> Result<RunSpec> {
    let workspace =
        security::resolve_workspace(&context.repo_root, &context.config.workspace.mount)?;
    let mut environment =
        security::collect_environment(&context.config, &context.repo_root, allowed_secrets)?;
    if let Some(package_manager) = &context.tools.package_manager {
        environment.insert(
            "AGENTBOX_PACKAGE_MANAGER".into(),
            package_manager.to_owned(),
        );
    }
    if context.tools.rust {
        environment.insert("AGENTBOX_RUST_PROJECT".into(), "1".into());
    }
    if context.config.headroom.enabled {
        let url = context.config.headroom.url.trim_end_matches('/');
        environment.insert("ANTHROPIC_BASE_URL".into(), url.into());
        environment.insert("OPENAI_BASE_URL".into(), format!("{url}/v1"));
    }
    docker::build_run_spec(BuildInput {
        config: &context.config,
        repo_root: &context.repo_root,
        workspace: &workspace,
        compose: context.compose.as_ref(),
        environment,
        command,
        interactive,
    })
}

fn agent_command(config: &Config, args: &RunArgs, tools: &ProjectTools) -> Result<Vec<String>> {
    let executable = match &args.agent {
        Some(agent) => agent.clone(),
        None if !config.agent.command.is_empty() => config.agent.command.clone(),
        None => config.agent.default.clone(),
    };
    let mut command =
        shell_words::split(&executable).context("agent command contains invalid shell quoting")?;
    apply_agentbox_permissions(&mut command);
    let guidance = project_guidance(config, tools);
    match command.first().map(String::as_str) {
        Some("claude") => {
            command.push("--append-system-prompt".into());
            command.push(guidance);
        }
        Some("codex") => {
            command.push("-c".into());
            command.push(format!("developer_instructions={guidance:?}"));
        }
        _ => {}
    }
    command.extend(args.arguments.clone());
    if command.is_empty() {
        anyhow::bail!("agent command is empty");
    }
    Ok(command)
}

fn apply_agentbox_permissions(command: &mut Vec<String>) {
    match command.first().map(String::as_str) {
        Some("claude") => {
            let mut index = 1;
            while index < command.len() {
                if command[index] == "--permission-mode" {
                    command.remove(index);
                    if index < command.len() {
                        command.remove(index);
                    }
                } else if command[index].starts_with("--permission-mode=") {
                    command.remove(index);
                } else {
                    index += 1;
                }
            }
            if !command
                .iter()
                .any(|argument| argument == "--dangerously-skip-permissions")
            {
                command.push("--dangerously-skip-permissions".into());
            }
        }
        Some("codex")
            if !command
                .iter()
                .any(|argument| argument == "--dangerously-bypass-approvals-and-sandbox") =>
        {
            command.push("--dangerously-bypass-approvals-and-sandbox".into());
        }
        _ => {}
    }
}

fn project_guidance(config: &Config, tools: &ProjectTools) -> String {
    let package_manager = tools
        .package_manager
        .as_ref()
        .map(|manager| {
            format!(
            "This project uses {manager}; use {manager} for dependency installation and package \
             scripts instead of another JavaScript package manager."
            )
        })
        .unwrap_or_default();
    let rust = if tools.rust {
        String::from(
            "This is a Rust project with cargo and rustc available. When relevant, run cargo fmt \
             --check, cargo check, cargo test, and cargo clippy.",
        )
    } else {
        String::new()
    };

    let caveman = if config.caveman.enabled {
        format!(
            "Caveman is enabled at the `{}` level. Use the installed Caveman skill and keep \
             responses in that mode unless the user explicitly asks for normal mode.",
            config.caveman.level.as_str()
        )
    } else {
        String::new()
    };

    format!(
        "{}\n\n{}",
        AGENT_INSTRUCTIONS
            .replace(PACKAGE_MANAGER_PLACEHOLDER, &package_manager)
            .replace(RUST_PLACEHOLDER, &rust)
            .trim(),
        caveman
    )
    .trim()
    .to_owned()
}

fn print_banner(context: &AppContext, spec: &RunSpec) {
    println!("Agentbox security summary:");
    println!(
        "- Repo mounted read-write: {}",
        if spec.workspace_write { "yes" } else { "no" }
    );
    println!("- Host home mounted: no");
    println!("- SSH agent mounted: no");
    println!("- Docker socket mounted: no");
    println!("- Host credentials imported implicitly: no");
    println!(
        "- Internet access: {}",
        if spec.network != "none" {
            "yes (network policy is Docker-managed)"
        } else {
            "no"
        }
    );
    println!("- Network: {}", spec.network);
    println!("- Container user: {}", spec.uid_gid);
    println!("- Runtime image: {}", context.config.runtime.image);
    println!(
        "- Headroom proxy: {}",
        if context.config.headroom.enabled {
            context.config.headroom.url.as_str()
        } else {
            "disabled"
        }
    );
    println!(
        "- Caveman: {}",
        if context.config.caveman.enabled {
            context.config.caveman.level.as_str()
        } else {
            "disabled"
        }
    );
    println!(
        "- Package manager: {}",
        context
            .tools
            .package_manager
            .as_deref()
            .unwrap_or("not detected")
    );
    println!(
        "- Rust project: {}",
        if context.tools.rust { "yes" } else { "no" }
    );
    println!(
        "- Environment variables: {}",
        if spec.environment.is_empty() {
            "none".into()
        } else {
            spec.environment
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
}

fn ensure_docker_available() -> Result<()> {
    let status = Command::new("docker")
        .arg("info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("Docker CLI was not found")?;
    if !status.success() {
        anyhow::bail!("Docker daemon is unavailable; start Docker and retry");
    }
    Ok(())
}

fn ensure_compose_network(context: &AppContext) -> Result<()> {
    if context.config.network.mode == NetworkMode::Compose
        && let Some(compose) = &context.compose
    {
        compose::ensure_network_exists(&compose.network)?;
    }
    Ok(())
}

fn command_health(program: &str, args: &[&str]) -> String {
    match Command::new(program).args(args).output() {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let value = stdout.trim();
            if value.is_empty() {
                "ok".into()
            } else {
                value.into()
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            format!("unavailable ({})", stderr.trim())
        }
        Err(error) => format!("unavailable ({error})"),
    }
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_profile_disables_nested_sandbox() {
        let args = RunArgs {
            agent: Some("codex".into()),
            ..RunArgs::default()
        };

        let command = agent_command(&Config::default(), &args, &ProjectTools::default()).unwrap();
        assert_eq!(command[0], "codex");
        assert_eq!(command[1], "--dangerously-bypass-approvals-and-sandbox");
        assert_eq!(command[2], "-c");
        assert!(command[3].contains("# Agentbox environment"));
        assert!(command[3].contains("database migrations"));
    }

    #[test]
    fn claude_profile_skips_permissions() {
        let args = RunArgs {
            agent: Some("claude".into()),
            ..RunArgs::default()
        };

        let command = agent_command(&Config::default(), &args, &ProjectTools::default()).unwrap();
        assert_eq!(command[0], "claude");
        assert_eq!(command[1], "--dangerously-skip-permissions");
        assert_eq!(command[2], "--append-system-prompt");
    }

    #[test]
    fn legacy_claude_permission_mode_is_replaced() {
        let mut config = Config::default();
        config.agent.command = "claude --permission-mode acceptEdits".into();

        let command =
            agent_command(&config, &RunArgs::default(), &ProjectTools::default()).unwrap();
        assert!(command.contains(&"--dangerously-skip-permissions".into()));
        assert!(
            !command
                .iter()
                .any(|argument| argument == "--permission-mode")
        );
        assert!(!command.iter().any(|argument| argument == "acceptEdits"));
    }

    #[test]
    fn permission_flags_are_not_duplicated() {
        let mut config = Config::default();
        config.agent.command = "claude --dangerously-skip-permissions".into();
        let claude = agent_command(&config, &RunArgs::default(), &ProjectTools::default()).unwrap();
        assert_eq!(
            claude
                .iter()
                .filter(|argument| *argument == "--dangerously-skip-permissions")
                .count(),
            1
        );

        let args = RunArgs {
            agent: Some("codex --dangerously-bypass-approvals-and-sandbox".into()),
            ..RunArgs::default()
        };
        let codex = agent_command(&config, &args, &ProjectTools::default()).unwrap();
        assert_eq!(
            codex
                .iter()
                .filter(|argument| *argument == "--dangerously-bypass-approvals-and-sandbox")
                .count(),
            1
        );
    }

    #[test]
    fn custom_agent_wrappers_are_not_modified() {
        let args = RunArgs {
            agent: Some("my-agent-wrapper claude".into()),
            ..RunArgs::default()
        };
        let command = agent_command(&Config::default(), &args, &ProjectTools::default()).unwrap();
        assert_eq!(command, ["my-agent-wrapper", "claude"]);
    }

    #[test]
    fn bare_invocation_selects_default_run() {
        let cli = Cli::try_parse_from(["agentbox"]).unwrap();
        assert!(cli.command.is_none());
    }

    #[test]
    fn pnpm_guidance_is_added_to_claude() {
        let tools = ProjectTools {
            package_manager: Some("pnpm".into()),
            rust: false,
        };
        let command = agent_command(&Config::default(), &RunArgs::default(), &tools).unwrap();
        assert!(
            command
                .iter()
                .any(|argument| argument == "--append-system-prompt")
        );
        assert!(
            command
                .iter()
                .any(|argument| argument.contains("uses pnpm"))
        );
    }

    #[test]
    fn instruction_template_placeholders_are_fully_resolved() {
        let guidance = project_guidance(
            &Config::default(),
            &ProjectTools {
                package_manager: Some("pnpm".into()),
                rust: true,
            },
        );
        assert!(!guidance.contains("{{"));
        assert!(guidance.contains("uses pnpm"));
        assert!(guidance.contains("cargo clippy"));
        assert!(guidance.contains("\n## Environment limitations\n"));
        assert!(guidance.contains("Docker and Docker Compose may be unavailable"));
    }

    #[test]
    fn caveman_guidance_uses_configured_level() {
        let mut config = Config::default();
        config.caveman.enabled = true;
        config.caveman.level = crate::config::CavemanLevel::Ultra;

        let guidance = project_guidance(&config, &ProjectTools::default());

        assert!(guidance.contains("Caveman is enabled at the `ultra` level"));
    }

    #[test]
    fn headroom_routes_provider_traffic_through_proxy() {
        let mut config = Config::default();
        config.headroom.enabled = true;
        let temp = tempfile::tempdir().unwrap();
        let context = AppContext {
            repo_root: temp.path().to_path_buf(),
            config,
            compose_files: vec![],
            compose: None,
            tools: ProjectTools::default(),
        };

        let spec = build_spec(&context, &[], vec!["true".into()], false).unwrap();

        assert_eq!(
            spec.environment["ANTHROPIC_BASE_URL"],
            "http://headroom:8787"
        );
        assert_eq!(
            spec.environment["OPENAI_BASE_URL"],
            "http://headroom:8787/v1"
        );
    }
}
