#!/bin/sh

set -u

agent_update_prefix="$HOME/.agentbox/npm"
export PATH="$agent_update_prefix/bin:$PATH"

import_agent_credentials() {
    case "${AGENTBOX_IMPORT_CREDENTIALS:-}" in
        claude)
            source="/tmp/agentbox-claude-credentials.json"
            destination="$HOME/.claude/.credentials.json"
            ;;
        codex)
            source="/tmp/agentbox-codex-auth.json"
            destination="$HOME/.codex/auth.json"
            ;;
        "")
            return
            ;;
        *)
            printf '%s\n' \
                "- Unknown credential type '$AGENTBOX_IMPORT_CREDENTIALS'; skipping import" >&2
            return
            ;;
    esac

    if [ ! -f "$source" ]; then
        printf '%s\n' "- Credential source is unavailable; skipping import" >&2
        return
    fi

    destination_directory="$(dirname "$destination")"
    session_directory="/tmp/agentbox-credentials/$AGENTBOX_IMPORT_CREDENTIALS"
    session_credential="$session_directory/$(basename "$destination")"
    mkdir -p "$destination_directory" "$session_directory"
    chmod 700 "$destination_directory" "$session_directory"
    cp "$source" "$session_credential"
    chmod 600 "$session_credential"
    rm -f "$destination"
    ln -s "$session_credential" "$destination"
    printf '%s\n' "Agent credentials:"
    printf '%s\n' "- Imported $AGENTBOX_IMPORT_CREDENTIALS credentials from host"
}

import_claude_state() {
    if [ "${AGENTBOX_IMPORT_CLAUDE_STATE:-0}" != "1" ]; then
        return
    fi

    source="/tmp/agentbox-claude-state.json"
    destination="$HOME/.claude.json"
    if [ ! -f "$source" ]; then
        printf '%s\n' "- Claude onboarding state is unavailable; skipping import" >&2
        return
    fi

    temporary="/tmp/agentbox-claude-state-merged.json"
    if [ -f "$destination" ] && jq -e 'type == "object"' "$destination" >/dev/null 2>&1; then
        jq -s '.[0] * .[1]' "$destination" "$source" >"$temporary"
    else
        cp "$source" "$temporary"
    fi
    chmod 600 "$temporary"
    mv "$temporary" "$destination"
}

update_agent() {
    package="$1"
    label="$2"
    installed_version="$(
        npm list --global --prefix "$agent_update_prefix" --depth=0 --json 2>/dev/null |
            jq -r --arg package "$package" '.dependencies[$package].version // empty'
    )"
    latest_version="$(
        timeout 15s npm view \
            --cache /tmp/agentbox-npm-cache \
            --loglevel error \
            "$package@latest" version 2>/dev/null
    )"

    if [ -n "$installed_version" ] &&
        [ -n "$latest_version" ] &&
        [ "$installed_version" = "$latest_version" ]
    then
        printf '%s\n' "- $label: up to date ($installed_version)"
        return
    fi

    if timeout 60s npm install \
        --global \
        --prefix "$agent_update_prefix" \
        --cache /tmp/agentbox-npm-cache \
        --no-audit \
        --no-fund \
        --loglevel error \
        "$package@latest" >/tmp/agentbox-npm-install.log 2>&1
    then
        version="$(
            npm list --global --prefix "$agent_update_prefix" --depth=0 --json 2>/dev/null |
                jq -r --arg package "$package" '.dependencies[$package].version // "unknown"'
        )"
        printf '%s\n' "- $label: updated ($version)"
    else
        printf '%s\n' "- $label: update failed; using installed version" >&2
    fi
}

install_caveman() {
    version="v1.8.2"
    marker="$HOME/.agentbox/caveman-$version"

    if [ -f "$marker" ]; then
        return
    fi

    printf '%s\n' "Caveman:"
    mkdir -p "$HOME/.agentbox"
    if timeout 90s npx --yes "github:JuliusBrussee/caveman#$version" -- \
        --only claude \
        --only codex \
        --minimal \
        --non-interactive >/tmp/agentbox-caveman-install.log 2>&1
    then
        touch "$marker"
        printf '%s\n' "- installed ($version)"
    else
        printf '%s\n' "- install failed; continuing without Caveman" >&2
    fi
}

import_agent_credentials
import_claude_state

if [ "${AGENTBOX_AUTO_UPDATE:-1}" != "0" ] && [ -n "${AGENTBOX_UPDATE_AGENT:-}" ]; then
    printf '%s\n' "Agent updates:"
    mkdir -p "$agent_update_prefix"
    case "$AGENTBOX_UPDATE_AGENT" in
        claude)
            update_agent "@anthropic-ai/claude-code" "Claude Code"
            ;;
        codex)
            update_agent "@openai/codex" "Codex"
            ;;
        *)
            printf '%s\n' "- Unknown agent '$AGENTBOX_UPDATE_AGENT'; skipping update" >&2
            ;;
    esac
fi

if [ "${AGENTBOX_CAVEMAN:-0}" = "1" ]; then
    install_caveman
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
