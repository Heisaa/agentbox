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
agentbox update-config
agentbox doctor
agentbox
agentbox tui
agentbox explain claude
agentbox run claude
agentbox shell
agentbox up postgres redis
agentbox down
```

Running `agentbox` without a subcommand starts the configured default agent.
`agentbox tui` opens a multi-session terminal interface. The left sidebar lists
active sessions, and `Ctrl-N` opens a session in another repository. Use `F6`
to switch sessions, `F3` to open the host's `lazygit` in the active repository,
`F5` to run Claude's `/usage` or Codex's `/status`, and `F2` to open the
captured detail view. Closing `lazygit` returns to the agent view. The latest
status is shown subtly below the active terminal. Conversation history scrolls
with the mouse wheel or Page Up/Down without triggering prompt history; the
view stays anchored while the agent streams output, and `End` jumps back to
the live view. Drag with the left mouse button to select text in the
conversation — releasing the button copies it to the system clipboard via
OSC 52 (Codex runs in inline mode so its history is preserved too). Press `F1`
for the complete key reference.

Direct Claude commands are launched with `--dangerously-skip-permissions`, and
direct Codex commands use `--dangerously-bypass-approvals-and-sandbox`.
Agentbox provides the external container boundary, so the agents do not add
nested approval prompts or sandboxes. Custom wrapper executables are left
unchanged.

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

At container startup, the runtime updates only the agent being started from npm
into `$HOME/.agentbox/npm` in the persistent agent home. For example,
`agentbox run codex` checks Codex but not Claude Code. If the persisted version
matches npm's `latest` version, installation is skipped. Version checks time
out after 15 seconds, updates time out after 60 seconds, and registry or
installation failures do not prevent startup. Disable automatic updates for
offline or latency-sensitive projects:

```toml
[runtime]
auto_update = false
```

`agentbox init` creates `.agentbox/config.toml`, an ignored local development
environment file location, and an example env file. Edit `runtime.image` if
you use a custom image.

When loading an existing config, Agentbox warns about missing current options,
unknown options, and recognized deprecated names. Missing options continue
using their built-in defaults, so compatible older configs still run.
Run `agentbox update-config` to add missing options with their current defaults
and remove recognized deprecated options. Existing supported values and
unknown custom options are preserved. The previous file is saved as
`.agentbox/config.toml.bak`.

Workspace mounts are restricted to the repository root or one of its
subdirectories, including after resolving symlinks. `doctor` and `explain`
redact sensitive environment values from displayed Docker commands.

In Compose mode, Agentbox selects the default network or the sole network used
by project services. Set `network.compose_network` to a Compose network key or
resolved Docker network name when a project uses multiple networks. If the
selected network does not exist yet, agent runs fall back to Docker's bridge
network. Run `agentbox up` when the agent needs to reach project services by
their Compose service names.

```toml
[network]
compose_network = "backend"
```

## Headroom proxy

Agentbox can route provider traffic through a
[Headroom](https://github.com/chopratejas/headroom) proxy running as a Docker
Compose service. Enable it in `.agentbox/config.toml`:

```toml
[headroom]
enabled = true
service = "headroom"
url = "http://headroom:8787"
```

Add the service to the project's Compose file:

```yaml
services:
  headroom:
    image: ghcr.io/chopratejas/headroom:latest
    restart: unless-stopped
    environment:
      HEADROOM_HOST: 0.0.0.0
      HEADROOM_TELEMETRY: "off"
      ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY:-}
      OPENAI_API_KEY: ${OPENAI_API_KEY:-}
```

When enabled, Agentbox starts the configured Headroom service automatically
before launching an agent or shell. Agentbox refuses to start that service
while `headroom.enabled` is false, and excludes it from a bare `agentbox up`.
The agent container receives only the proxy URLs; provider keys remain in the
Headroom container. The included [`compose.yaml`](compose.yaml) provides this
service for developing Agentbox.

## Caveman

Agentbox can install and activate the
[Caveman](https://github.com/JuliusBrussee/caveman) output-compression skill
for Claude Code and Codex:

```toml
[caveman]
enabled = true
level = "full"
```

Supported levels are `lite`, `full`, `ultra`, and `wenyan`. Agentbox installs
the pinned Caveman `v1.8.2` release into the persistent agent home on first
use. Caveman affects response verbosity, while Headroom compresses request
context; both options can be enabled together.

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
