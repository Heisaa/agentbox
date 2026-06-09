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
        compose::inspect(&repo_root, &compose_files)?
    } else {
        None
    };
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
        docker::execute(&spec)
    }
}

fn run_shell(args: &SandboxArgs) -> Result<u8> {
    let context = load_context(true)?;
    let shell = vec!["bash".into()];
    let spec = build_spec(&context, &args.allow_secrets, shell, true)?;
    print_banner(&context, &spec);
    ensure_docker_available()?;
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
    let is_plain_codex = executable == "codex";
    let mut command =
        shell_words::split(&executable).context("agent command contains invalid shell quoting")?;
    if is_plain_codex {
        command.push("--dangerously-bypass-approvals-and-sandbox".into());
    }
    let guidance = project_guidance(tools);
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

fn project_guidance(tools: &ProjectTools) -> String {
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

    AGENT_INSTRUCTIONS
        .replace(PACKAGE_MANAGER_PLACEHOLDER, &package_manager)
        .replace(RUST_PLACEHOLDER, &rust)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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
        assert!(command[3].contains("Verify changes"));
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
        let guidance = project_guidance(&ProjectTools {
            package_manager: Some("pnpm".into()),
            rust: true,
        });
        assert!(!guidance.contains("{{"));
        assert!(guidance.contains("uses pnpm"));
        assert!(guidance.contains("cargo clippy"));
    }
}
