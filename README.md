# agentbox

`agentbox` runs coding agents in Docker with the current repository mounted
read-write, a synthetic container home, narrowly scoped agent credentials, and
no Docker socket. The included full-stack image provides Claude Code, Codex,
Node.js 24, pnpm, Rust/Cargo, Python, PostgreSQL and Redis clients, native build
tools, and common command-line development tools.

Before starting an agent, Agentbox ensures the shared Cargo registry cache is
owned by the container user. This repairs cache volumes created by older images
as `root` without running the main agent container as `root`.

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
agentbox init-image
agentbox build
agentbox doctor
agentbox
agentbox explain claude
agentbox run claude
agentbox shell
agentbox up postgres redis
agentbox down
```

Running `agentbox` without a subcommand opens the multi-session terminal
interface; `agentbox tui` remains as an explicit alias. Agentbox starts with the
repository picker when no sessions exist, so it can be launched from any
directory. The left sidebar lists active sessions with `starting`, `working`,
`waiting`, `done`, or `exited` state, and `Ctrl-N` opens another
repository. The picker keeps the eight most recently opened repositories at the
top and discovers Git repositories under your home directory; a full path can
still be entered. After choosing a repository, select Claude or Codex before
the session starts. Repositories configured with a custom agent also show that
command as an option.

Agentbox saves the open repository tabs between launches. `Ctrl-Q` stops their
ephemeral containers but keeps the saved tabs. On the next launch, Claude tabs
restart with `--continue` and Codex tabs restart with `resume --last`, using the
per-project persistent agent home. The selected agent is saved with each tab.
Custom agents restart normally because Agentbox cannot infer a generic resume
command. Any labeled containers left by a crash are removed before the saved
sessions restart. `Ctrl-W` closes the active session and removes it from the
saved tabs; `Ctrl-C` is an equivalent shortcut.

Containers use the host timezone. Agentbox mounts `/etc/localtime` read-only and
sets `TZ` when it can determine the host's IANA timezone name.

Use `Ctrl-J` and `Ctrl-K` to move down and up through the session list. `F6`
still cycles to the next session. Use `F3` to open the host's `lazygit` in the active
repository, `F5` to refresh usage immediately, and `F2` to open the captured
detail view.
Usage is fetched automatically every 30 seconds by a hidden process in the
session container. It invokes only Claude's `/usage` or Codex's `/status`
built-in command, so it does not submit a model prompt or consume model tokens.
The first check waits five seconds for the session container to start. Closing
`lazygit` returns to the agent view. The 5-hour usage window is shown subtly
below the active terminal in a compact form such as
`50% left, resets 19:30`; `F2` retains the complete status, including weekly
usage. Conversation history scrolls
with the mouse wheel or Page Up/Down without triggering prompt history; the
view stays anchored while the agent streams output, and `End` jumps back to
the live view. Drag with the left mouse button to select text in the
conversation — releasing the button copies it to the system clipboard via
OSC 52 (Codex runs in inline mode so its history is preserved too). Press `F1`
for the complete key reference. Press `F4` to save a PNG image from the host
clipboard under `.agentbox/uploads/` and insert its container path into the
active prompt without submitting it. `Ctrl-V` provides the same behavior when
the host terminal passes that key through. Linux requires `wl-paste` on Wayland
or `xclip` on X11; macOS uses `pngpaste` when available and otherwise falls back
to the system AppleScript clipboard API.

Direct Claude commands are launched with `--dangerously-skip-permissions`, and
direct Codex commands use `--dangerously-bypass-approvals-and-sandbox`.
Agentbox provides the external container boundary, so the agents do not add
nested approval prompts or sandboxes. Custom wrapper executables are left
unchanged.

## Project runtime image

Projects can extend the standard Agentbox image with system packages and other
tools. Generate a starter Dockerfile and configure it automatically:

```bash
agentbox init-image
```

The default base is `agentbox/fullstack:latest`. Select another base with
`agentbox init-image --base-image example/custom:tag`. The command refuses to
overwrite an existing `.agentbox/Dockerfile`.

The resulting `.agentbox/config.toml` contains:

```toml
[runtime]
image = "agentbox/fullstack:latest"
dockerfile = ".agentbox/Dockerfile"
build_context = "."
auto_update = true
```

The configured `image` is passed to the build as `AGENTBOX_BASE_IMAGE`. A
minimal `.agentbox/Dockerfile` is:

```dockerfile
ARG AGENTBOX_BASE_IMAGE
FROM ${AGENTBOX_BASE_IMAGE}

USER root
RUN apt-get update \
    && apt-get install -y --no-install-recommends ffmpeg \
    && rm -rf /var/lib/apt/lists/*
```

`agentbox run`, `agentbox shell`, and TUI sessions build the project image
before launching. Docker reuses unchanged layers. Run `agentbox build` to build
it explicitly, and use `agentbox explain` to inspect both the build and run
commands. The Dockerfile and build context must be inside the repository.

Agentbox tags the derived image locally using the project name and a hash of
its repository path. If `runtime.dockerfile` is empty, `runtime.image` continues
to be used directly without a build.

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

At startup, Agentbox imports credentials only for the agent being started when
the standard host credential exists. Claude Code receives
`~/.claude/.credentials.json` on Linux; on macOS, Agentbox falls back to the
`Claude Code-credentials` Keychain entry. Codex receives `~/.codex/auth.json`.
The host credential is staged read-only. A writable copy is created under the
container's `/tmp` tmpfs and linked into the synthetic home, so refreshes do not
modify the host credential and the credential copy disappears with the
container. For Claude, Agentbox also imports only the theme from
`~/.claude.json` and marks first-run onboarding complete; account metadata, MCP
servers, project history, and other settings are excluded. Shells, custom
agents, and the other built-in agent receive no credential.

`agentbox init` creates `.agentbox/config.toml`, an ignored local development
environment file location, and an example env file. Edit `runtime.image` to use
a custom image directly, or configure `runtime.dockerfile` to derive a
project-specific image.

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
does not mount the host home, SSH agent, general cloud configuration, Git
configuration, package-manager credentials, or Docker socket. The read-only
`/etc/localtime` system file is mounted to match the host timezone. Agentbox
imports only the selected Claude Code or Codex credential file as described
above.

The MVP uses Docker's normal network controls. Joining a Compose network
usually also permits internet access. Set `network.mode = "none"` for a fully
disconnected container; this also prevents access to Compose services.

See [PROJECT_PLAN.md](PROJECT_PLAN.md) for the threat model and roadmap.
