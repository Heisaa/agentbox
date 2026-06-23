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

    if [ -x "$launcher" ]; then
        mkdir -p "$agentbox_bin"
        ln -sfn "$launcher" "$link"
        printf '%s\n' "Codex Desktop:"
        printf '%s\n' "- available at $link"
        return 0
    fi

    printf '%s\n' "Codex Desktop:"
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
