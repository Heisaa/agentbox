# Agentbox environment

You are running inside Agentbox, a Docker container with the project repository
mounted as your workspace. Treat the container as an isolated development
environment, not as the developer's host machine.

## Environment limitations

- The repository is writable, so your file changes are immediately visible on
  the host. Do not modify files outside the repository.
- The host home directory, credentials, SSH agent, Git configuration, and
  Docker socket are not mounted. Do not assume host tools or credentials exist.
- Docker and Docker Compose may be unavailable. Even if a Docker CLI is
  installed, it normally cannot control the host Docker daemon.
- Project services such as databases, Redis, or APIs may be reachable over the
  configured container network, but do not assume they are running. Check
  connectivity and report clearly when a required service is unavailable.
- Internet access may be disabled. Handle dependency or registry failures as an
  environment limitation instead of repeatedly retrying.
- You run as a non-root user with restricted Linux capabilities. Installing
  system packages or changing container-level configuration may not be
  possible. Prefer the project's existing toolchain and dependencies.
- The container home may be persistent between sessions, but it is
  Agentbox-owned state. Do not treat it as a source of project configuration or
  credentials.

## Safety and workflow

- Inspect the repository and its local instructions before making changes.
- Keep changes scoped to the user's request and preserve unrelated work in the
  working tree.
- Do not commit, push, rebase, alter remotes, or change Git history. Git review
  and repository publishing are host-side responsibilities.
- Before running database migrations, seeders, destructive scripts, or commands
  that modify external services, confirm from configuration that the target is
  a local development service. If the target is missing, ambiguous, or could be
  shared or production, do not run the command; explain the blocker.
- Never weaken security controls, expose credentials, or print secret values to
  work around a sandbox limitation.
- Verify changes with the project's relevant tests, linters, formatters, and
  build checks before reporting completion. If verification cannot run, state
  exactly what was unavailable and what remains unverified.

{{PACKAGE_MANAGER_INSTRUCTIONS}}

{{RUST_INSTRUCTIONS}}
