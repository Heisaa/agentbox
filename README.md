# agentbox

`agentbox` runs coding agents in Docker with the current repository mounted
read-write, a synthetic container home, no host credentials, and no Docker
socket. The included full-stack image provides Claude Code, Codex, Node.js 24,
pnpm, Rust/Cargo, Python, PostgreSQL and Redis clients, native build tools, and
common command-line development tools.

## Install

Build the runtime image and install the CLI:

```bash
./install.sh
```

The installer uses `agentbox/fullstack:latest` by default. Override the image
tag with `AGENTBOX_IMAGE`. If Cargo's bin directory is not in `PATH`, the
installer prints the command needed to add it.

```bash
AGENTBOX_IMAGE=agentbox/fullstack:dev ./install.sh
```

## Manual Build

```bash
cargo build --release
docker build -t agentbox/fullstack:latest -f images/fullstack/Dockerfile .
```

Install the CLI with:

```bash
cargo install --path .
```

## Use

```bash
cd /path/to/project
agentbox init
agentbox doctor
agentbox
agentbox explain claude
agentbox run claude
agentbox shell
agentbox up postgres redis
agentbox down
```

Running `agentbox` without a subcommand starts the configured default agent.
For Claude, the generated default is `--permission-mode acceptEdits`: file
edits are accepted automatically while riskier operations may still prompt. Codex
is launched with `--dangerously-bypass-approvals-and-sandbox` because Agentbox
itself provides the external container sandbox.

Agentbox detects Rust projects and JavaScript package managers from common
manifest and lock files. Claude and Codex receive instructions to run relevant
verification commands. When pnpm is indicated by `packageManager`,
`pnpm-lock.yaml`, or `pnpm-workspace.yaml`, they are explicitly instructed to
use pnpm rather than npm or Yarn.

The injected agent guidance is maintained in
[`instructions/agent.md`](instructions/agent.md). Edit that template to change
the shared Claude and Codex instructions, then rebuild and reinstall the CLI.
The guidance explains that the agent is inside a restricted container, may not
have Docker or running project services, must validate migration targets, and
must leave Git publishing operations to the host.

At container startup, the runtime checks npm for newer Claude Code and Codex
versions. Registry failures do not prevent startup, and each query times out
after four seconds. Rebuild the image to install reported updates:

```bash
docker build --pull -t agentbox/fullstack:latest \
  -f images/fullstack/Dockerfile .
```

Disable startup checks for offline or latency-sensitive projects:

```toml
[env.defaults]
AGENTBOX_CHECK_UPDATES = "0"
```

`agentbox init` creates `.agentbox/config.toml`, an ignored local development
environment file location, and an example env file. Edit `runtime.image` if
you use a custom image.

When loading an existing config, Agentbox warns about missing current options,
unknown options, and recognized deprecated names. Missing options continue
using their built-in defaults, so compatible older configs still run.

Workspace mounts are restricted to the repository root or one of its
subdirectories, including after resolving symlinks. `doctor` and `explain`
redact sensitive environment values from displayed Docker commands.

In Compose mode, Agentbox selects the default network or the sole network used
by project services. Set `network.compose_network` to a Compose network key or
resolved Docker network name when a project uses multiple networks. Agent runs
verify that the selected network exists and otherwise direct you to run
`agentbox up`.

```toml
[network]
compose_network = "backend"
```

Only variables listed in `[env].allow` or `[env.defaults]`, plus values from
the configured project-local env file, enter the container. Sensitive names
are rejected unless explicitly approved:

```bash
agentbox run codex --allow-secret OPENAI_API_KEY
```

The approval only permits a variable already selected by the project config;
it does not automatically import that variable.

## Security boundary

The repository is intentionally writable, so the agent can alter or delete
project files. Review changes and perform Git commits on the host. Agentbox
does not mount the host home, SSH agent, cloud configuration, Git
configuration, package-manager credentials, or Docker socket.

The MVP uses Docker's normal network controls. Joining a Compose network
usually also permits internet access. Set `network.mode = "none"` for a fully
disconnected container; this also prevents access to Compose services.

See [PROJECT_PLAN.md](PROJECT_PLAN.md) for the threat model and roadmap.
