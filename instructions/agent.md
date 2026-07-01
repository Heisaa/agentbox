# Agentbox environment

You run inside Agentbox, a Docker container with the project repository mounted
as your workspace. Treat it as an isolated development environment, not the
developer's host machine. Agent-level permission prompts and nested sandboxes
may be disabled because Agentbox provides the container boundary; this does not
remove the limitations below.

## Environment limitations

- The repository is writable and changes are visible on the host. Do not modify
  files outside the repository.
- The host home, credentials, SSH agent, Git config, and Docker socket are not
  mounted. Do not assume host tools or credentials exist.
- Git is available for local commands against the mounted repository, including
  linked worktrees. Do not assume Git remotes can authenticate from inside the
  container.
- Docker and Docker Compose may be unavailable, and an installed Docker CLI
  normally cannot reach the host daemon.
- Project services (databases, Redis, APIs) may be reachable on the container
  network, but may not be running. Check connectivity and report clearly when a
  required service is unavailable.
- Internet access may be disabled. Treat dependency or registry failures as an
  environment limitation rather than retrying repeatedly.
- You run as a non-root user with restricted capabilities; installing system
  packages or changing container config may not be possible. Prefer the
  project's existing toolchain.
- The container home may persist between sessions but is Agentbox-owned state.
  Do not treat it as a source of project configuration or credentials.

## Safety and workflow

- Inspect the repository and its local instructions before making changes.
- Keep changes scoped to the user's request and preserve unrelated work.
- Do not push, rebase, alter remotes, or change Git history; publishing is a
  host-side responsibility.
- Before running migrations, seeders, or other destructive commands against
  external services, confirm from configuration that the target is a local
  development service. If it is missing, ambiguous, or possibly shared or
  production, do not run it; explain the blocker.
- Never weaken security controls, expose credentials, or print secret values.
- Verify changes with the project's tests, linters, formatters, and build
  checks before reporting completion. If verification cannot run, state what was
  unavailable and what remains unverified.

{{PACKAGE_MANAGER_INSTRUCTIONS}}

{{RUST_INSTRUCTIONS}}
