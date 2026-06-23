#!/bin/sh

set -u

agent_update_prefix="$HOME/.agentbox/npm"
agentbox_bin="/tmp/agentbox-bin"
export PATH="$agentbox_bin:$agent_update_prefix/bin:$PATH"

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
    credential_label="$AGENTBOX_IMPORT_CREDENTIALS"
    if [ "$AGENTBOX_IMPORT_CREDENTIALS" = "codex" ] && [ -n "${AGENTBOX_CODEX_ACCOUNT:-}" ]; then
        credential_label="codex@$AGENTBOX_CODEX_ACCOUNT"
    fi
    printf '%s\n' "Agent credentials:"
    printf '%s\n' "- Imported $credential_label credentials from host"
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

print_codex_desktop_log_tail() {
    log="$1"
    if [ -s "$log" ]; then
        printf '%s\n' "- last build log lines:" >&2
        tail -n 80 "$log" >&2
    fi
}

# Seed Codex Desktop's persisted state so it launches ready to work:
#   * "full access" (YOLO) mode so it never pauses for approvals. Agentbox
#     already isolates the agent inside this container, so the app's own
#     sandbox/approval prompts are redundant; this mirrors the
#     `--dangerously-bypass-approvals-and-sandbox` flag the Codex CLI receives.
#   * the workspace folder open by default (the container working directory).
#   * the welcome / intro onboarding screens marked complete so they are skipped.
# Set AGENTBOX_CODEX_DESKTOP_FULL_ACCESS=0 to keep the app's built-in approval
# default.
configure_codex_desktop_defaults() {
    AGENTBOX_CODEX_DESKTOP_STATE="${CODEX_HOME:-$HOME/.codex}/.codex-global-state.json" \
    AGENTBOX_CODEX_DESKTOP_FULL_ACCESS="${AGENTBOX_CODEX_DESKTOP_FULL_ACCESS:-1}" \
    AGENTBOX_CODEX_DESKTOP_WORKSPACE="$(pwd 2>/dev/null || printf '/workspace')" \
        python3 - <<'PY' || { printf '%s\n' "- could not seed defaults" >&2; return 0; }
import json
import os
import time

path = os.environ["AGENTBOX_CODEX_DESKTOP_STATE"]
full_access = os.environ.get("AGENTBOX_CODEX_DESKTOP_FULL_ACCESS", "1") != "0"
workspace = os.environ.get("AGENTBOX_CODEX_DESKTOP_WORKSPACE", "")

try:
    with open(path) as handle:
        state = json.load(handle)
    if not isinstance(state, dict):
        state = {}
except (FileNotFoundError, ValueError):
    state = {}

atoms = state.get("electron-persisted-atom-state")
if not isinstance(atoms, dict):
    atoms = {}
    state["electron-persisted-atom-state"] = atoms

if full_access:
    modes = atoms.get("agent-mode-by-host-id")
    if not isinstance(modes, dict):
        modes = {}
    # Force every known host (and the default "local" host) to full-access.
    modes = {host: "full-access" for host in modes}
    modes.setdefault("local", "full-access")
    atoms["agent-mode-by-host-id"] = modes
    # Keep full-access visible in the composer mode picker.
    visibility = atoms.get("composer-permission-mode-visibility")
    if not isinstance(visibility, dict):
        visibility = {}
    visibility["full-access"] = True
    atoms["composer-permission-mode-visibility"] = visibility

# Open the workspace folder by default; the app reads roots[0] as the active
# project. Keep it first while preserving any other folders the user added.
if workspace:
    for key in (
        "electron-saved-workspace-roots",
        "project-order",
        "active-workspace-roots",
    ):
        roots = state.get(key)
        if not isinstance(roots, list):
            roots = []
        roots = [root for root in roots if root != workspace]
        roots.insert(0, workspace)
        state[key] = roots

# Mark onboarding complete so the welcome / intro screens are skipped. The app
# shows them when last_completed_onboarding is unset or older than its cutoff,
# so stamp it with the current time.
atoms["electron:onboarding-welcome-pending"] = False
atoms["electron:onboarding-projectless-completed"] = True
atoms["last_completed_onboarding"] = int(time.time())
role_state = atoms.get("electron:onboarding-welcome-v2-role-state")
if not isinstance(role_state, dict) or not role_state.get("roles"):
    atoms["electron:onboarding-welcome-v2-role-state"] = {
        "roles": ["engineering"],
        "personalizedSuggestionsEnabled": False,
        "workMode": "coding",
    }

os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
tmp = path + ".tmp"
with open(tmp, "w") as handle:
    json.dump(state, handle)
os.replace(tmp, path)
PY
    if [ "${AGENTBOX_CODEX_DESKTOP_FULL_ACCESS:-1}" != "0" ]; then
        printf '%s\n' "- full-access (no approval prompts) enabled"
    fi
    printf '%s\n' "- opens $(pwd 2>/dev/null || printf '/workspace'); onboarding skipped"
}

ensure_codex_desktop() {
    if [ "${AGENTBOX_GUI:-0}" != "1" ]; then
        return 0
    fi

    root="${AGENTBOX_CODEX_DESKTOP_DIR:-/agentbox/codex-desktop}"
    repo="$root/codex-desktop-linux"
    launcher="$repo/codex-app/start.sh"
    link="$agentbox_bin/codex-desktop"
    build_log="$root/install.log"
    build_tmp="$root/tmp"
    requested=0
    case "${1:-}" in
        codex-desktop|"$launcher"|"$link")
            requested=1
            ;;
    esac

    if [ "$requested" = "0" ]; then
        return 0
    fi

    printf '%s\n' "Codex Desktop:"
    configure_codex_desktop_defaults

    if [ -x "$launcher" ]; then
        mkdir -p "$agentbox_bin"
        ln -sfn "$launcher" "$link"
        printf '%s\n' "- available at $link"
        return 0
    fi

    printf '%s\n' "- building Linux wrapper; this can take several minutes on first use"
    if ! mkdir -p "$root" "$agentbox_bin"; then
        printf '%s\n' "- failed to prepare Codex Desktop directories" >&2
        exit 1
    fi
    rm -rf "$build_tmp"
    mkdir -p "$root/cache" "$build_tmp"
    : >"$build_log"
    if [ ! -d "$repo/.git" ]; then
        rm -rf "$repo"
        if ! git clone --depth=1 https://github.com/ilysenko/codex-desktop-linux.git "$repo" \
            >"$build_log" 2>&1
        then
            printf '%s\n' \
                "- clone failed; see $build_log" >&2
            print_codex_desktop_log_tail "$build_log"
            [ "$requested" = "0" ] || exit 1
            return 0
        fi
    fi

    if (
        cd "$repo" &&
            export XDG_CACHE_HOME="$root/cache" &&
            export CODEX_MANAGED_NODE_CACHE_DIR="$root/cache/node-runtime" &&
            export npm_config_cache="$root/cache/npm" &&
            export TMPDIR="$build_tmp" &&
            PACKAGE_WITH_UPDATER=0 MAX_BUILD_THREADS="${AGENTBOX_CODEX_DESKTOP_BUILD_THREADS:-2}" \
                make build-app-fresh >>"$build_log" 2>&1
    ); then
        rm -rf "$build_tmp"
        ln -sfn "$launcher" "$link"
        printf '%s\n' "- installed at $link"
    else
        rm -rf "$build_tmp"
        printf '%s\n' \
            "- build failed; see $build_log" >&2
        print_codex_desktop_log_tail "$build_log"
        [ "$requested" = "0" ] || exit 1
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

ensure_codex_desktop "$@"

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
