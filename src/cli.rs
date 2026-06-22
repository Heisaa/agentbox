use std::{
    fs::{self, OpenOptions},
    io::Write,
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
    /// Update the project configuration to the current schema.
    UpdateConfig,
    /// Create a project-specific runtime Dockerfile.
    InitImage(InitImageArgs),
    /// Build the configured project-specific runtime image.
    Build,
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
    /// Open the multi-session terminal interface.
    Tui,
}

#[derive(Debug, Clone, Args)]
struct SandboxArgs {
    /// Explicitly permit a sensitive variable named in the env allowlist.
    #[arg(long = "allow-secret", value_name = "NAME")]
    allow_secrets: Vec<String>,
}

#[derive(Debug, Clone, Args)]
struct InitImageArgs {
    /// Base image for the project-specific runtime.
    #[arg(long, default_value = "agentbox/fullstack:latest")]
    base_image: String,
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

#[derive(Debug, PartialEq, Eq)]
enum RuntimeNetworkAction {
    StartHeadroom,
    UseCompose,
    UseBridge,
}

pub fn run() -> Result<u8> {
    let cli = Cli::parse();
    match cli.command {
        None => crate::tui::run(),
        Some(Commands::Init) => init(),
        Some(Commands::UpdateConfig) => update_config(),
        Some(Commands::InitImage(args)) => init_image(&args),
        Some(Commands::Build) => build_runtime(),
        Some(Commands::Run(args)) => run_agent(&args, false),
        Some(Commands::Shell(args)) => run_shell(&args),
        Some(Commands::Up { services }) => compose_action("up", &services),
        Some(Commands::Down) => compose_action("down", &[]),
        Some(Commands::Doctor(args)) => doctor(&args),
        Some(Commands::Explain(args)) => run_agent(&args, true),
        Some(Commands::Tui) => crate::tui::run(),
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
    fs::write(&gitignore, "env\nenv.local\nsecrets/\nuploads/\n")
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

fn update_config() -> Result<u8> {
    let repo_root = project::find_repo_root()?;
    let update = Config::update(&repo_root)?;

    println!("Updated {}", relative(&repo_root, &update.path));
    println!(
        "- Added: {}",
        if update.added.is_empty() {
            "none".into()
        } else {
            update.added.join(", ")
        }
    );
    println!(
        "- Removed deprecated: {}",
        if update.removed.is_empty() {
            "none".into()
        } else {
            update.removed.join(", ")
        }
    );
    println!("- Backup: {}", relative(&repo_root, &update.backup_path));
    Ok(0)
}

fn init_image(args: &InitImageArgs) -> Result<u8> {
    let repo_root = project::find_repo_root()?;
    let base_image = args.base_image.trim();
    if base_image.is_empty() || base_image.chars().any(char::is_whitespace) {
        anyhow::bail!("base image must be a non-empty container image reference");
    }
    let config_path = repo_root.join(crate::config::CONFIG_FILE);
    if !config_path.is_file() {
        anyhow::bail!(
            "{} does not exist; run `agentbox init` first",
            config_path.display()
        );
    }

    let relative_dockerfile = Path::new(".agentbox/Dockerfile");
    let dockerfile = repo_root.join(relative_dockerfile);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&dockerfile)
        .with_context(|| {
            if dockerfile.exists() {
                format!(
                    "{} already exists; refusing to overwrite it",
                    dockerfile.display()
                )
            } else {
                format!("failed to create {}", dockerfile.display())
            }
        })?;
    let template = runtime_dockerfile_template(base_image);
    if let Err(error) = file.write_all(template.as_bytes()) {
        let _ = fs::remove_file(&dockerfile);
        return Err(error).with_context(|| format!("failed to write {}", dockerfile.display()));
    }

    let update = match Config::enable_runtime_image(
        &repo_root,
        base_image,
        relative_dockerfile,
        Path::new("."),
    ) {
        Ok(update) => update,
        Err(error) => {
            let _ = fs::remove_file(&dockerfile);
            return Err(error);
        }
    };

    println!("Created {}", relative(&repo_root, &update.dockerfile));
    println!("Updated {}", relative(&repo_root, &update.path));
    println!("- Base image: {base_image}");
    println!("\nAdd project packages to the Dockerfile, then run `agentbox build`.");
    Ok(0)
}

fn runtime_dockerfile_template(base_image: &str) -> String {
    format!(
        "ARG AGENTBOX_BASE_IMAGE={base_image}\n\
         FROM ${{AGENTBOX_BASE_IMAGE}}\n\
         \n\
         USER root\n\
         \n\
         # Add project-specific system packages here. For example:\n\
         # RUN apt-get update \\\n\
         #     && apt-get install -y --no-install-recommends ffmpeg \\\n\
         #     && rm -rf /var/lib/apt/lists/*\n"
    )
}

fn load_context(inspect_compose: bool) -> Result<AppContext> {
    let repo_root = project::find_repo_root()?;
    let loaded = Config::load(&repo_root)?;
    if !loaded.warnings.is_empty() {
        eprintln!("warning: {} may be outdated:", crate::config::CONFIG_FILE);
        for warning in &loaded.warnings {
            eprintln!("- {warning}");
        }
        eprintln!("Run `agentbox update-config` to update the file to the current schema.\n");
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
    let mut context = load_context(true)?;
    let command = agent_command(&context.config, args, &context.tools)?;
    if explain {
        let spec = build_spec(&context, &args.allow_secrets, command, false)?;
        print_banner(&context, &spec);
        print_build_command(&context)?;
        println!("\n{}", docker::format_command(&spec));
        Ok(0)
    } else {
        ensure_docker_available()?;
        build_runtime_image(&context)?;
        prepare_runtime_network(&mut context)?;
        let spec = build_spec(&context, &args.allow_secrets, command, true)?;
        print_banner(&context, &spec);
        docker::execute(&spec)
    }
}

fn run_shell(args: &SandboxArgs) -> Result<u8> {
    let mut context = load_context(true)?;
    let shell = vec!["bash".into()];
    ensure_docker_available()?;
    build_runtime_image(&context)?;
    prepare_runtime_network(&mut context)?;
    let spec = build_spec(&context, &args.allow_secrets, shell, true)?;
    print_banner(&context, &spec);
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
    if let Some(build) = docker::build_image_spec(&context.config, &context.repo_root)? {
        println!("- Project runtime image: {}", build.image);
        println!(
            "\nDocker build command:\n{}",
            docker::format_image_build_command(&build)
        );
    }
    println!("\nDocker command:\n{}", docker::format_command(&spec));
    Ok(0)
}

fn build_runtime() -> Result<u8> {
    let context = load_context(false)?;
    ensure_docker_available()?;
    let Some(spec) = docker::build_image_spec(&context.config, &context.repo_root)? else {
        anyhow::bail!(
            "runtime.dockerfile is not configured in {}",
            crate::config::CONFIG_FILE
        );
    };
    println!("Building {}", spec.image);
    docker::execute_image_build(&spec)
}

fn build_runtime_image(context: &AppContext) -> Result<()> {
    let Some(spec) = docker::build_image_spec(&context.config, &context.repo_root)? else {
        return Ok(());
    };
    println!("Building project runtime image {}...", spec.image);
    let status = docker::execute_image_build(&spec)?;
    if status != 0 {
        anyhow::bail!("project runtime image build failed with exit code {status}");
    }
    Ok(())
}

fn print_build_command(context: &AppContext) -> Result<()> {
    if let Some(spec) = docker::build_image_spec(&context.config, &context.repo_root)? {
        println!(
            "\nDocker build command:\n{}",
            docker::format_image_build_command(&spec)
        );
    }
    Ok(())
}

fn compose_action(action: &str, services: &[String]) -> Result<u8> {
    let context = load_context(false)?;
    if action == "up" && !services.is_empty() {
        select_up_services(&context.config, &[], services)?;
    }
    ensure_docker_available()?;
    let selected_services;
    let services = if action == "up" && services.is_empty() && !context.config.headroom.enabled {
        let available = compose::list_services(&context.repo_root, &context.compose_files)?;
        let Some(selected) = select_up_services(&context.config, &available, services)? else {
            println!(
                "Headroom is disabled in {}; no Compose services to start.",
                crate::config::CONFIG_FILE
            );
            return Ok(0);
        };
        selected_services = selected;
        &selected_services
    } else {
        services
    };
    compose::run_action(&context.repo_root, &context.compose_files, action, services)
}

fn select_up_services(
    config: &Config,
    available: &[String],
    requested: &[String],
) -> Result<Option<Vec<String>>> {
    if config.headroom.enabled {
        return Ok(Some(requested.to_vec()));
    }

    if requested
        .iter()
        .any(|service| service == &config.headroom.service)
    {
        anyhow::bail!(
            "Compose service `{}` requires `headroom.enabled = true` in {}",
            config.headroom.service,
            crate::config::CONFIG_FILE
        );
    }
    if !requested.is_empty() {
        return Ok(Some(requested.to_vec()));
    }

    let services = available
        .iter()
        .filter(|service| *service != &config.headroom.service)
        .cloned()
        .collect::<Vec<_>>();
    Ok((!services.is_empty()).then_some(services))
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
    let session_id = std::env::var("AGENTBOX_SESSION_ID")
        .ok()
        .filter(|value| !value.is_empty());
    docker::build_run_spec(BuildInput {
        config: &context.config,
        repo_root: &context.repo_root,
        workspace: &workspace,
        host_home: dirs::home_dir().as_deref(),
        compose: context.compose.as_ref(),
        environment,
        command,
        interactive,
        session_id: session_id.as_deref(),
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
        Some(command_name) if is_codex_command_name(command_name)? => {
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
        Some(command_name)
            if is_codex_command_name(command_name).unwrap_or(false)
                && !command
                    .iter()
                    .any(|argument| argument == "--dangerously-bypass-approvals-and-sandbox") =>
        {
            command.push("--dangerously-bypass-approvals-and-sandbox".into());
        }
        _ => {}
    }
}

fn is_codex_command_name(command_name: &str) -> Result<bool> {
    Ok(command_name == "codex" || docker::codex_account_from_command_name(command_name)?.is_some())
}

fn project_guidance(config: &Config, tools: &ProjectTools) -> String {
    let package_manager = tools
        .package_manager
        .as_ref()
        .map(|manager| {
            format!(
                "This project uses {manager}; use {manager} rather than another JavaScript \
                 package manager."
            )
        })
        .unwrap_or_default();
    let rust = if tools.rust {
        String::from(
            "This is a Rust project with cargo and rustc available. Run cargo fmt, check, \
             clippy, test, and build as needed to verify your changes.",
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
    println!(
        "- Host agent credentials: {}",
        spec.imported_credentials
            .as_deref()
            .map(|agent| format!("{agent} (read-only source, container-local copy)"))
            .unwrap_or_else(|| "none".into())
    );
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
    println!(
        "- Runtime image: {}",
        docker::runtime_image(&context.config, &context.repo_root)
    );
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

fn prepare_runtime_network(context: &mut AppContext) -> Result<()> {
    if context.config.network.mode != NetworkMode::Compose {
        return Ok(());
    }
    let Some(project) = &context.compose else {
        return Ok(());
    };

    let network_exists = if context.config.headroom.enabled {
        false
    } else {
        compose::network_exists(&project.network)?
    };
    match runtime_network_action(context.config.headroom.enabled, network_exists) {
        RuntimeNetworkAction::StartHeadroom => {
            let services = vec![context.config.headroom.service.clone()];
            let status =
                compose::run_action(&context.repo_root, &context.compose_files, "up", &services)?;
            if status != 0 {
                anyhow::bail!(
                    "failed to start Headroom Compose service `{}`",
                    context.config.headroom.service
                );
            }
            compose::ensure_network_exists(&project.network)?;
        }
        RuntimeNetworkAction::UseCompose => {}
        RuntimeNetworkAction::UseBridge => {
            eprintln!(
                "warning: Compose network `{}` is unavailable; using Docker bridge networking",
                project.network
            );
            context.compose = None;
        }
    }
    Ok(())
}

fn runtime_network_action(
    headroom_enabled: bool,
    compose_network_exists: bool,
) -> RuntimeNetworkAction {
    if headroom_enabled {
        RuntimeNetworkAction::StartHeadroom
    } else if compose_network_exists {
        RuntimeNetworkAction::UseCompose
    } else {
        RuntimeNetworkAction::UseBridge
    }
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
    }

    #[test]
    fn named_codex_profile_disables_nested_sandbox() {
        let args = RunArgs {
            agent: Some("codex@work".into()),
            ..RunArgs::default()
        };

        let command = agent_command(&Config::default(), &args, &ProjectTools::default()).unwrap();
        assert_eq!(command[0], "codex@work");
        assert_eq!(command[1], "--dangerously-bypass-approvals-and-sandbox");
        assert_eq!(command[2], "-c");
        assert!(command[3].contains("# Agentbox environment"));
    }

    #[test]
    fn invalid_named_codex_profile_is_rejected() {
        let args = RunArgs {
            agent: Some("codex@../work".into()),
            ..RunArgs::default()
        };

        let error = agent_command(&Config::default(), &args, &ProjectTools::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("Codex account name"));
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
    fn bare_invocation_selects_default_tui() {
        let cli = Cli::try_parse_from(["agentbox"]).unwrap();
        assert!(cli.command.is_none());
    }

    #[test]
    fn build_subcommand_is_available() {
        let cli = Cli::try_parse_from(["agentbox", "build"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::Build)));
    }

    #[test]
    fn init_image_subcommand_has_a_default_base_image() {
        let cli = Cli::try_parse_from(["agentbox", "init-image"]).unwrap();
        let Some(Commands::InitImage(args)) = cli.command else {
            panic!("expected init-image command");
        };
        assert_eq!(args.base_image, "agentbox/fullstack:latest");
    }

    #[test]
    fn runtime_dockerfile_template_uses_the_selected_base() {
        let template = runtime_dockerfile_template("example/base:1");
        assert!(template.contains("ARG AGENTBOX_BASE_IMAGE=example/base:1"));
        assert!(template.contains("FROM ${AGENTBOX_BASE_IMAGE}"));
        assert!(template.contains("apt-get install"));
    }

    #[test]
    fn update_config_subcommand_is_available() {
        let cli = Cli::try_parse_from(["agentbox", "update-config"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::UpdateConfig)));
    }

    #[test]
    fn tui_subcommand_is_available() {
        let cli = Cli::try_parse_from(["agentbox", "tui"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::Tui)));
    }

    #[test]
    fn run_separator_passes_options_to_the_configured_agent() {
        let cli = Cli::try_parse_from(["agentbox", "run", "--", "--no-alt-screen"]).unwrap();
        let Some(Commands::Run(args)) = cli.command else {
            panic!("expected run command");
        };

        assert_eq!(args.agent, None);
        assert_eq!(args.arguments, ["--no-alt-screen"]);
    }

    #[test]
    fn disabled_headroom_is_excluded_from_implicit_compose_up() {
        let config = Config::default();
        let available = vec!["app".into(), "headroom".into(), "db".into()];

        let selected = select_up_services(&config, &available, &[])
            .unwrap()
            .unwrap();

        assert_eq!(selected, ["app", "db"]);
    }

    #[test]
    fn disabled_headroom_cannot_be_started_explicitly() {
        let config = Config::default();
        let error = select_up_services(&config, &["headroom".into()], &["headroom".into()])
            .unwrap_err()
            .to_string();

        assert!(error.contains("headroom.enabled = true"));
    }

    #[test]
    fn disabled_headroom_as_only_service_skips_compose_up() {
        let config = Config::default();

        assert!(
            select_up_services(&config, &["headroom".into()], &[])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn enabled_headroom_preserves_default_compose_up() {
        let mut config = Config::default();
        config.headroom.enabled = true;

        assert_eq!(
            select_up_services(&config, &["headroom".into()], &[]).unwrap(),
            Some(Vec::new())
        );
    }

    #[test]
    fn missing_compose_network_uses_bridge() {
        assert_eq!(
            runtime_network_action(false, false),
            RuntimeNetworkAction::UseBridge
        );
    }

    #[test]
    fn existing_compose_network_is_reused() {
        assert_eq!(
            runtime_network_action(false, true),
            RuntimeNetworkAction::UseCompose
        );
    }

    #[test]
    fn enabled_headroom_is_started_before_network_selection() {
        assert_eq!(
            runtime_network_action(true, false),
            RuntimeNetworkAction::StartHeadroom
        );
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
        assert!(guidance.contains("Run cargo fmt, check, clippy, test, and build"));
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
