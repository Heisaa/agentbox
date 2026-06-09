#!/bin/sh

set -u

check_agent_update() {
    package="$1"
    label="$2"
    installed="$3"

    latest="$(timeout 4s npm view "$package" version --silent 2>/dev/null || true)"
    if [ -z "$latest" ]; then
        printf '%s\n' "- $label: registry unavailable"
    elif [ "$installed" = "$latest" ]; then
        printf '%s\n' "- $label: current ($installed)"
    else
        printf '%s\n' "- $label: update available ($installed -> $latest)"
    fi
}

if [ "${AGENTBOX_CHECK_UPDATES:-1}" != "0" ]; then
    installed_packages="$(npm list --global --depth=0 --json 2>/dev/null || true)"
    claude_version="$(
        printf '%s' "$installed_packages" |
            jq -r '.dependencies["@anthropic-ai/claude-code"].version // "unknown"'
    )"
    codex_version="$(
        printf '%s' "$installed_packages" |
            jq -r '.dependencies["@openai/codex"].version // "unknown"'
    )"

    printf '%s\n' "Agent updates:"
    check_agent_update "@anthropic-ai/claude-code" "Claude Code" "$claude_version"
    check_agent_update "@openai/codex" "Codex" "$codex_version"
fi

if [ -n "${AGENTBOX_PACKAGE_MANAGER:-}" ] || [ "${AGENTBOX_RUST_PROJECT:-0}" = "1" ]; then
    printf '%s\n' "Project tools:"
    if [ -n "${AGENTBOX_PACKAGE_MANAGER:-}" ]; then
        printf '%s\n' "- Package manager: $AGENTBOX_PACKAGE_MANAGER"
    fi
    if [ "${AGENTBOX_RUST_PROJECT:-0}" = "1" ]; then
        printf '%s\n' "- Rust: cargo and rustc available"
    fi
fi

exec "$@"
