# Project Plan: Frictionless Docker Sandboxes for Coding Agents

## 1. Goal

Build a local developer tool that makes it easy to run coding agents inside Docker with enough freedom to install packages, run tests, start servers, inspect logs, and talk to project services, while preventing accidental access to host secrets and credentials.

The host remains the source of truth for Git. The project repository is bind-mounted into the agent container so edits are immediately visible on the host. The developer reviews, stages, commits, pushes, and handles real credentials from the host, not from inside the agent environment.

## 2. Working name

`agentbox`

Possible CLI shape:

```bash
agentbox init
agentbox run claude
agentbox run codex
agentbox shell
agentbox up
agentbox down
agentbox doctor
agentbox policy explain
```

## 3. Core principles

1. **Host secrets stay on the host**
   The agent container should not mount `$HOME`, SSH keys, cloud credentials, shell history, password stores, browser profiles, Git credential helpers, or global config by default.

2. **Repo edits are real**
   The project directory is bind-mounted read-write so the agent can modify the actual working tree.

3. **Git remains human-controlled**
   The agent may inspect `git diff` and propose commits, but staging, committing, pushing, rebasing, and force-pushing are host-side actions by default.

4. **Project services are reachable**
   The agent container should join the same Docker network as the repo’s Compose services so it can reach `postgres`, `redis`, `minio`, `api`, etc. by service name.

5. **Freedom inside the box**
   Inside the container, the agent should be able to install dev dependencies, run package managers, execute tests, start local servers, and use temporary files without approval fatigue.

6. **Dangerous access is explicit**
   Network, Docker socket access, extra mounts, credentials, and host-level operations require explicit opt-in.

7. **The default mode should be boring and safe**
   A new user should be able to run `agentbox`, select a repository, and resume that session later without thinking through Docker networking, UID mapping, secrets, or shell setup.

## 4. Threat model

### Main risks to prevent

* Agent reads host secrets from:

  * `~/.ssh`
  * `~/.aws`
  * `~/.config/gcloud`
  * `~/.kube`
  * password stores
  * browser profiles
  * shell history
  * global `.npmrc`, `.pypirc`, `.cargo/credentials`, etc.

* Agent exfiltrates secrets over the network.

* Agent modifies files outside the repository.

* Agent deletes or corrupts the host repo.

* Agent accesses the host Docker socket and controls unrelated containers.

* Agent connects to production databases or cloud APIs.

* Agent commits or pushes code without review.

* Agent bakes secrets into Docker images or logs.

### Accepted risks

* The agent can modify files inside the mounted repo.
* The agent can delete files inside the mounted repo.
* The agent can run arbitrary commands inside the container.
* The agent can access project-local development services.
* The agent can consume CPU, memory, and disk within configured limits.

## 5. Architecture

### Components

#### 5.1 CLI

A small host-side CLI, probably written in Rust.

Responsibilities:

* Detect repo root.
* Provide a repository-first TUI when launched without a subcommand.
* Detect Docker Compose files.
* Build or pull an agent runtime image.
* Create an isolated agent container.
* Persist TUI repository tabs and restart agents with their resume commands.
* Clean up labeled orphaned Agentbox containers.
* Mount the repo into the container.
* Attach the agent container to the correct Compose network.
* Inject safe environment variables.
* Start the requested agent command.
* Print service connection hints.
* Provide diagnostics.

Example:

```bash
agentbox run claude
agentbox run codex
agentbox shell
```

#### 5.2 Agent runtime image

A base image with common full-stack tooling:

* Git
* Node.js
* pnpm, npm, yarn
* Python
* Rust optional feature
* Go optional feature
* Docker CLI optional, but no Docker socket by default
* PostgreSQL client
* Redis CLI
* curl, jq, ripgrep, fd, git-delta
* Playwright dependencies optional
* Claude Code / Codex installed through configurable install steps

The image should run as a non-root user whose UID/GID matches the host user where possible.

#### 5.3 Per-project config

A checked-in or local config file.

Possible names:

```text
.agentbox/config.toml
agentbox.toml
```

Example:

```toml
[project]
workdir = "/workspace"
mount = "."

[agent]
default = "claude"
allow_git_write = false
allow_network = true

[compose]
files = ["docker-compose.yml"]
project_name = "my-app"
auto_up = true
services = ["postgres", "redis", "minio"]

[network]
mode = "compose"
extra_hosts = []

[env]
pass = [
  "NODE_ENV",
  "RUST_LOG"
]

[env.defaults]
NODE_ENV = "development"

[secrets]
mode = "deny-by-default"

[limits]
cpus = 4
memory = "8g"
pids = 2048
```

#### 5.4 Runtime state directory

Use a tool-owned directory on the host, not the user’s home config directly.

Example:

```text
~/.local/share/agentbox/
  images/
  caches/
  policies/
  sessions/
```

Mount only specific cache directories into the container, not the whole home directory.

Example safe-ish cache mounts:

```text
agentbox-pnpm-cache:/home/agent/.pnpm-store
agentbox-npm-cache:/home/agent/.npm
agentbox-cargo-cache:/home/agent/.cargo/registry
```

Avoid mounting:

```text
~/.ssh
~/.aws
~/.config
~/.gnupg
~/.docker
~/.kube
~/.npmrc
~/.gitconfig
```

## 6. Docker model

### 6.1 Repo mount

Mount the repo read-write:

```bash
-v "$REPO_ROOT:/workspace"
-w /workspace
```

This is the main intentional escape hatch: edits are reflected on the host.

### 6.2 No host home mount

The container gets a synthetic home:

```text
/home/agent
```

This home is either ephemeral or backed by a named Docker volume.

Modes:

```text
--home ephemeral
--home persistent
--home reset
```

Default: persistent tool-owned home volume, but with no host secrets.

### 6.3 UID/GID mapping

Run the container as the host user’s UID/GID to avoid root-owned files in the repo:

```bash
--user "$(id -u):$(id -g)"
```

For Linux this is important. For macOS/Windows Docker Desktop, behavior is different but still manageable.

### 6.4 Compose network attachment

When the repo has Compose services, the CLI should:

1. Find the Compose project name.
2. Start services if requested.
3. Detect the default network.
4. Run the agent container attached to that network.

Example:

```bash
docker compose up -d postgres redis
docker network inspect myapp_default
docker run --network myapp_default ...
```

Inside the agent container:

```bash
psql postgres://postgres:postgres@postgres:5432/app
redis-cli -h redis
curl http://api:3000
```

### 6.5 Avoid host network mode

Do not use `--network host` by default.

Host network mode makes isolation weaker and behaves differently across platforms.

### 6.6 Docker socket

Do not mount:

```text
/var/run/docker.sock
```

by default.

If the project needs Docker-from-inside-Docker, support explicit modes:

```bash
agentbox run --docker none        # default
agentbox run --docker cli-only    # docker CLI installed, no socket
agentbox run --docker socket      # dangerous, explicit warning
agentbox run --docker sidecar     # controlled helper API
```

Preferred long-term direction: a sidecar helper that only exposes safe operations like reading logs for this project’s Compose services, not full Docker daemon control.

## 7. Secrets strategy

### 7.1 Default deny

By default, pass no credentials.

Do not pass:

* SSH agent
* Git credential helper
* cloud credentials
* Docker credentials
* package registry tokens
* global `.env`
* host `.npmrc`
* host `.gitconfig`

### 7.2 Project `.env`

The tool should not blindly pass host `.env` files into the container.

Instead, support a development-only env file:

```text
.agentbox/env
.agentbox/env.local
.env.agent
```

The CLI can generate a template:

```bash
agentbox init env
```

Example:

```env
DATABASE_URL=postgres://postgres:postgres@postgres:5432/app
REDIS_URL=redis://redis:6379
S3_ENDPOINT=http://minio:9000
S3_ACCESS_KEY=minio
S3_SECRET_KEY=minio123
```

These should be local/test credentials only.

### 7.3 Secret scanner

Before launching, run a fast check for risky mounts and env vars:

```text
AWS_ACCESS_KEY_ID
AWS_SECRET_ACCESS_KEY
GITHUB_TOKEN
OPENAI_API_KEY
ANTHROPIC_API_KEY
GOOGLE_APPLICATION_CREDENTIALS
KUBECONFIG
SSH_AUTH_SOCK
```

If detected, block or require `--allow-secret NAME`.

### 7.4 Build secrets

If building project images, use BuildKit secret mounts instead of build args or normal env vars.

Example:

```bash
docker build \
  --secret id=npmrc,src=.agentbox/secrets/npmrc \
  .
```

### 7.5 Agent API keys

This is tricky because the agent itself may need credentials to run.

Options:

1. **Host launches agent with OAuth/session already handled by the tool**
   Best UX, but integration-dependent.

2. **Pass only the agent provider token**
   Acceptable, but isolate it as the only secret and warn that the agent can potentially read it.

3. **Use a local proxy**
   The container talks to a localhost or sidecar proxy. The proxy holds the API key and enforces request limits/logging.

Best long-term design: provider-token proxy.

## 8. Permission modes

### 8.1 Safe default

```text
repo: read-write
host home: none
network: compose network + internet optional
docker socket: none
git commit/push: blocked
secrets: none
```

### 8.2 Offline mode

```bash
agentbox run --offline
```

* No external network.
* Still can talk to project Compose services.
* Useful for refactors and tests.

### 8.3 Online dev mode

```bash
agentbox run --online
```

* Allows internet for package installs and docs.
* Still no host secrets.

### 8.4 Locked repo mode

```bash
agentbox run --readonly
```

* Repo mounted read-only.
* Useful for code review and analysis.

### 8.5 Patch mode

Instead of mounting repo read-write, mount read-only and have the agent output a patch.

```bash
agentbox run --patch
agentbox apply session-123.patch
```

This is safer but less frictionless, so it should not be the main mode.

### 8.6 Dangerous mode

```bash
agentbox run --dangerous
```

Requires explicit confirmation.

Allows selected extra powers, such as Docker socket, host network, or secret passthrough.

Should print a clear warning and log what was enabled.

## 9. Git strategy

Default:

* Agent can run:

  * `git status`
  * `git diff`
  * `git log`
  * `git grep`
* Agent should not run:

  * `git add`
  * `git commit`
  * `git push`
  * `git rebase`
  * `git reset --hard`
  * `git clean -fd`
  * `git checkout .`

Technical enforcement can start simple:

* Add shell wrapper functions for Git.
* Provide policy warnings.
* Optionally install a wrapper script earlier in `PATH`.

Stronger enforcement later:

* Use a command broker.
* Use seccomp/AppArmor where practical.
* Run the agent in patch mode for stricter workflows.
* Make `.git` read-only while the worktree is writable. This is advanced and may be awkward, but worth exploring.

A pragmatic MVP should rely on the human reviewing Git changes on the host.

## 10. Package manager strategy

The agent should be free to install dependencies inside the repo, because that is part of normal development.

Allow:

```bash
npm install
pnpm install
cargo test
pip install -r requirements.txt
```

But avoid host-level global installs.

Use container-local caches:

```text
agentbox-npm-cache
agentbox-pnpm-cache
agentbox-cargo-cache
agentbox-pip-cache
```

Block or warn on:

```bash
sudo
apt install
curl ... | bash
```

For system packages, prefer rebuilding the agent image or using devcontainer features.

## 11. Devcontainer compatibility

Many full-stack repos already have:

```text
.devcontainer/devcontainer.json
```

The project should support this rather than competing with it.

Modes:

```bash
agentbox run --from-devcontainer
agentbox init --devcontainer
```

Implementation options:

* Parse `devcontainer.json`.
* Reuse its image/build settings.
* Reuse features where possible.
* Join the same Compose services.
* Add agent-specific hardening on top.

This makes adoption much easier for existing teams.

## 12. CLI user flows

### 12.1 First run in a repo

```bash
cd my-app
agentbox init
```

Output:

```text
Detected:
- docker-compose.yml
- services: app, postgres, redis
- package manager: pnpm
- Node.js project
- no .agentbox/env found

Created:
- .agentbox/config.toml
- .agentbox/env.example
- .agentbox/.gitignore
```

Then:

```bash
agentbox run claude
```

### 12.2 Starting services

```bash
agentbox up
```

Runs:

```bash
docker compose up -d postgres redis minio
```

### 12.3 Running the agent

```bash
agentbox run claude
```

Inside the agent:

```text
Workspace: /workspace
Network: myapp_default
Reachable services:
- postgres:5432
- redis:6379
- minio:9000

Host secrets mounted: none
Docker socket: disabled
Git write operations: discouraged
```

### 12.4 Host-side review

After the agent exits:

```bash
git diff
pnpm test
git add .
git commit -m "Fix auth callback"
```

## 13. MVP scope

### MVP must have

* CLI that runs an agent container from any repo.
* Bind-mount current repo to `/workspace`.
* Run as host UID/GID where possible.
* No host home mount.
* No Docker socket mount.
* Synthetic home directory.
* Optional persistent cache volumes.
* Detect Compose project network.
* Join the Compose network.
* Generate `.agentbox/config.toml`.
* Generate `.agentbox/env.example`.
* Pass only allowlisted env vars.
* `doctor` command that explains what is mounted, what env vars are passed, and what network is used.
* Clear startup banner showing security posture.

### MVP should not have

* Complex GUI.
* Full policy engine.
* Kubernetes support.
* Cloud credential passthrough.
* Automatic Git commits.
* Production deploy workflows.
* Full Docker daemon proxy.

## 14. V1 scope

* Devcontainer support.
* Multiple agent profiles.
* Offline/online modes.
* Network allowlist.
* Secret detection.
* Git command wrapper.
* Service discovery summary.
* Per-project cache volumes.
* Patch mode.
* Basic logs from Compose services.
* `agentbox explain` to show the exact `docker run` command.

## 15. V2 scope

* Sidecar Docker helper with limited Compose access.
* Local LLM/API proxy for provider keys.
* AppArmor/seccomp profiles.
* Read-only `.git` experiment.
* Snapshot/restore of repo state.
* Web UI or TUI session manager.
* Policy templates for teams.
* CI-compatible agent runs.
* Remote sandbox support.

## 16. Suggested repository structure

```text
agentbox/
  crates/ or packages/
    cli/
    config/
    docker/
    policy/
    doctor/
  images/
    base/
      Dockerfile
    node/
      Dockerfile
    rust/
      Dockerfile
    fullstack/
      Dockerfile
  examples/
    node-postgres/
    solidstart-postgres/
    rust-postgres/
  docs/
    threat-model.md
    security.md
    config.md
    compose-networking.md
    devcontainer.md
  tests/
```

## 17. Example generated config

```toml
version = 1

[workspace]
mount = "."
container_path = "/workspace"
write = true

[agent]
default = "claude"
command = "claude"
home = "persistent"

[security]
mount_host_home = false
mount_docker_socket = false
pass_ssh_agent = false
allow_host_network = false

[network]
mode = "compose"
compose_files = ["docker-compose.yml"]
compose_network = "default"
internet = true

[env]
allow = ["NODE_ENV", "RUST_LOG"]

[env.defaults]
NODE_ENV = "development"

[caches]
npm = true
pnpm = true
cargo = true
pip = true

[limits]
cpus = 4
memory = "8g"
pids = 2048
```

## 18. Example `docker run` generated by MVP

```bash
docker run --rm -it \
  --name agentbox-myapp \
  --user "$(id -u):$(id -g)" \
  --workdir /workspace \
  --network myapp_default \
  --memory 8g \
  --cpus 4 \
  --pids-limit 2048 \
  -e NODE_ENV=development \
  -v "$PWD:/workspace" \
  -v agentbox-myapp-home:/home/agent \
  -v agentbox-pnpm-cache:/home/agent/.pnpm-store \
  agentbox/fullstack:latest \
  claude
```

## 19. Important product decisions

### Decision 1: Should internet be on by default?

Recommended default: on for MVP, but visible and configurable.

Rationale: package installs and documentation lookups are common. The bigger security win is blocking host secrets. Without secrets, internet access is less dangerous, though still not risk-free.

Alternative: default to Compose-only network and require `--online`.

### Decision 2: Should the agent be able to use Git?

Recommended default: read-only Git behavior socially enforced in MVP, technically improved later.

Rationale: making `.git` read-only while the worktree is writable is possible but complicated. The user already wants to add and commit from the host.

### Decision 3: Should the Docker socket ever be mounted?

Recommended default: no.

Provide an explicit escape hatch with a scary warning. Long-term, build a restricted helper instead.

### Decision 4: Should this use devcontainers?

Recommended default: support devcontainers, but do not require them.

Rationale: many projects do not have devcontainers, but when they do, reusing them reduces setup friction.

## 20. Security checklist

On every run, print:

```text
Security summary:
- Repo mounted read-write: yes
- Host home mounted: no
- SSH agent mounted: no
- Docker socket mounted: no
- Cloud credentials mounted: no
- Internet access: yes
- Compose network: myapp_default
- Container user: 501:20
```

Block startup if:

* User tries to mount `$HOME`.
* User tries to pass known cloud credentials without explicit allow.
* User tries to mount Docker socket without explicit dangerous mode.
* The repo path is `/`, `$HOME`, or another suspicious broad path.
* The Compose file references production-looking env files unless explicitly allowed.

Warn if:

* `.env` contains production-looking names.
* `DATABASE_URL` points to a non-container hostname.
* `KUBECONFIG` is present.
* `AWS_PROFILE` is present.
* Git remote looks like production deployment automation is configured.

## 21. Success criteria

The project is successful when a developer can run:

```bash
cd existing-fullstack-repo
agentbox run claude
```

and get an agent that can:

* edit files in the repo,
* run tests,
* install normal project dependencies,
* connect to `postgres`/`redis`/other Compose services,
* inspect the app,
* avoid host secrets,
* avoid host Docker control,
* leave Git review and commits to the human.

The ideal experience should feel almost as convenient as running the agent directly on the host, but with a much smaller blast radius.
