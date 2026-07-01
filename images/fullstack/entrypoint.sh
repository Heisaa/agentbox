#!/bin/sh

set -u

agent_update_prefix="$HOME/.agentbox/npm"
agentbox_bin="/tmp/agentbox-bin"
export PATH="$agentbox_bin:$agent_update_prefix/bin:$PATH"

configure_host_browser() {
    if [ -z "${AGENTBOX_HOST_BROWSER_URL:-}" ] || [ -z "${AGENTBOX_HOST_BROWSER_TOKEN:-}" ]; then
        return
    fi

    mkdir -p "$agentbox_bin"
    for command in xdg-open open sensible-browser www-browser browser; do
        ln -sfn /usr/local/bin/agentbox-open "$agentbox_bin/$command"
    done
    ln -sfn /usr/local/bin/agentbox-open "$agentbox_bin/agentbox-open"
    ln -sfn /usr/local/bin/agentbox-open "$agentbox_bin/gio"
    export BROWSER="/usr/local/bin/agentbox-open"
}

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
    credential_label="$AGENTBOX_IMPORT_CREDENTIALS"
    if [ "$AGENTBOX_IMPORT_CREDENTIALS" = "codex" ] && [ -n "${AGENTBOX_CODEX_ACCOUNT:-}" ]; then
        credential_label="codex@$AGENTBOX_CODEX_ACCOUNT"
    fi

    if [ -L "$destination" ] && [ ! -e "$destination" ]; then
        rm -f "$destination"
    fi

    if [ -e "$destination" ] && [ -z "${AGENTBOX_CODEX_ACCOUNT:-}" ]; then
        printf '%s\n' "Agent credentials:"
        printf '%s\n' "- Preserved existing $credential_label credentials in container home"
        return
    fi

    temporary="$destination.tmp.$$"
    mkdir -p "$destination_directory"
    chmod 700 "$destination_directory"
    cp "$source" "$temporary"
    chmod 600 "$temporary"
    mv "$temporary" "$destination"
    printf '%s\n' "Agent credentials:"
    printf '%s\n' "- Seeded $credential_label credentials from host"
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

print_build_log_tail() {
    log="$1"
    if [ -s "$log" ]; then
        printf '%s\n' "- last build log lines:" >&2
        tail -n 80 "$log" >&2
    fi
}

print_runtime_log_tail() {
    log="$1"
    if [ -s "$log" ]; then
        printf '%s\n' "- last runtime log lines:" >&2
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

# Keep this tiny wrapper in the persistent Codex Desktop volume, but regenerate
# it on each run so launcher diagnostics improve without forcing a rebuild of
# the upstream Linux wrapper. The upstream launcher writes most failures to its
# own log; surface that tail when it exits non-zero so `agentbox run
# codex-desktop` does not fail silently after the startup banner.
write_codex_desktop_launcher() {
    launcher_path="$1"
    upstream_launcher="$2"
    cat >"$launcher_path" <<EOF
#!/bin/sh
export CODEX_SYNC_CLI_PREFLIGHT="\${CODEX_SYNC_CLI_PREFLIGHT:-1}"
runtime_log="\${XDG_CACHE_HOME:-\$HOME/.cache}/codex-desktop/launcher.log"
"$upstream_launcher" "\$@"
status="\$?"
if [ "\$status" -ne 0 ]; then
    printf '%s\n' "- Codex Desktop exited with status \$status" >&2
    if [ -s "\$runtime_log" ]; then
        printf '%s\n' "- last runtime log lines:" >&2
        tail -n 80 "\$runtime_log" >&2
    else
        printf '%s\n' "- runtime log is empty or missing: \$runtime_log" >&2
    fi
fi
exit "\$status"
EOF
    chmod +x "$launcher_path"
}

ensure_codex_desktop() {
    if [ "${AGENTBOX_GUI:-0}" != "1" ]; then
        return 0
    fi

    root="${AGENTBOX_CODEX_DESKTOP_DIR:-/agentbox/codex-desktop}"
    repo="$root/codex-desktop-linux"
    upstream_launcher="$repo/codex-app/start.sh"
    launcher="$root/run-codex-desktop.sh"
    link="$agentbox_bin/codex-desktop"
    build_log="$root/install.log"
    build_tmp="$root/tmp"
    requested=0
    case "${1:-}" in
        codex-desktop|"$upstream_launcher"|"$launcher"|"$link")
            requested=1
            ;;
    esac

    if [ "$requested" = "0" ]; then
        return 0
    fi

    printf '%s\n' "Codex Desktop:"
    configure_codex_desktop_defaults

    if [ -x "$upstream_launcher" ]; then
        mkdir -p "$agentbox_bin"
        write_codex_desktop_launcher "$launcher" "$upstream_launcher"
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
            print_build_log_tail "$build_log"
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
        write_codex_desktop_launcher "$launcher" "$upstream_launcher"
        ln -sfn "$launcher" "$link"
        printf '%s\n' "- installed at $link"
    else
        rm -rf "$build_tmp"
        printf '%s\n' \
            "- build failed; see $build_log" >&2
        print_build_log_tail "$build_log"
        [ "$requested" = "0" ] || exit 1
    fi
}

# (Re)write the launcher that runs the staged AppImage. Regenerated on every
# run so changes here take effect without rebuilding the image from scratch (it
# lives on the persistent claude-desktop volume). The bundled AppRun already
# adds --no-sandbox for AppImage builds and routes --doctor itself (only when it
# is the first argument), so the user's arguments are forwarded untouched.
# APPIMAGE_EXTRACT_AND_RUN lets the AppImage run without host FUSE.
#
write_claude_desktop_launcher() {
    launcher_path="$1"
    appimage_path="$2"
    cat >"$launcher_path" <<EOF
#!/bin/sh
export APPIMAGE_EXTRACT_AND_RUN=1
export LIBGL_ALWAYS_SOFTWARE="\${LIBGL_ALWAYS_SOFTWARE:-1}"
export LIBGL_DRI3_DISABLE="\${LIBGL_DRI3_DISABLE:-1}"
if [ "\${AGENTBOX_CLAUDE_DESKTOP_SOFTPIPE:-0}" = "1" ]; then
    export MESA_LOADER_DRIVER_OVERRIDE="\${MESA_LOADER_DRIVER_OVERRIDE:-softpipe}"
fi
# Agentbox may expose both Wayland and X11. In auto mode, keep Claude
# Desktop's upstream backend choice intact; native Wayland is still available
# via AGENTBOX_CLAUDE_DESKTOP_BACKEND=wayland or CLAUDE_USE_WAYLAND=1.
case "\${AGENTBOX_CLAUDE_DESKTOP_BACKEND:-auto}" in
    x11)
        export CLAUDE_USE_WAYLAND=0
        ;;
    wayland)
        if [ -n "\${WAYLAND_DISPLAY:-}" ] && [ "\${CLAUDE_USE_WAYLAND:-1}" != "0" ]; then
            export CLAUDE_USE_WAYLAND=1
            export XDG_SESSION_TYPE="\${XDG_SESSION_TYPE:-wayland}"
            unset DISPLAY
        fi
        ;;
    auto|"")
        ;;
    *)
        printf '%s\n' "- Unknown AGENTBOX_CLAUDE_DESKTOP_BACKEND='\$AGENTBOX_CLAUDE_DESKTOP_BACKEND'; using auto" >&2
        ;;
esac
if [ "\${AGENTBOX_CLAUDE_DESKTOP_DISABLE_GPU:-1}" = "1" ] && [ "\${1:-}" != "--doctor" ]; then
    has_disable_gpu=0
    has_disable_features=0
    has_disable_vulkan=0
    has_gpu_rasterization=0
    has_ozone_platform=0
    has_use_gl=0
    for arg in "\$@"; do
        case "\$arg" in
            --disable-gpu)
                has_disable_gpu=1
                ;;
            --disable-features|--disable-features=*)
                has_disable_features=1
                ;;
            --disable-vulkan)
                has_disable_vulkan=1
                ;;
            --disable-gpu-rasterization)
                has_gpu_rasterization=1
                ;;
            --ozone-platform|--ozone-platform=*)
                has_ozone_platform=1
                ;;
            --use-gl|--use-gl=*)
                has_use_gl=1
                ;;
        esac
    done
    if [ "\$has_ozone_platform" = "0" ] && [ -n "\${DISPLAY:-}" ] && [ "\${CLAUDE_USE_WAYLAND:-}" != "1" ]; then
        export ELECTRON_OZONE_PLATFORM_HINT="\${ELECTRON_OZONE_PLATFORM_HINT:-x11}"
        export GDK_BACKEND="\${GDK_BACKEND:-x11}"
        export QT_QPA_PLATFORM="\${QT_QPA_PLATFORM:-xcb}"
        set -- --ozone-platform=x11 "\$@"
    fi
    if [ "\$has_disable_features" = "0" ]; then
        set -- --disable-features=CustomTitlebar,Vulkan,DefaultANGLEVulkan,VulkanFromANGLE "\$@"
        set -- --disable-features=none "\$@"
    fi
    if [ "\$has_disable_vulkan" = "0" ]; then
        set -- --disable-vulkan "\$@"
    fi
    if [ "\$has_gpu_rasterization" = "0" ]; then
        set -- --disable-gpu-rasterization "\$@"
    fi
    if [ "\$has_use_gl" = "0" ]; then
        set -- --use-gl=swiftshader "\$@"
    fi
    if [ "\$has_disable_gpu" = "0" ]; then
        set -- --disable-gpu "\$@"
    fi
fi
cache_dir="\${XDG_CACHE_HOME:-\$HOME/.cache}/claude-desktop-debian"
runtime_log="\$cache_dir/launcher.log"
agentbox_log="\$cache_dir/agentbox-launcher.log"
mkdir -p "\$cache_dir"
: >"\$agentbox_log"

run_claude_desktop() {
    if command -v dbus-run-session >/dev/null 2>&1 && [ -z "\${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
        exec dbus-run-session -- "$appimage_path" "\$@"
    fi
    exec "$appimage_path" "\$@"
}

print_x11_diagnostics() {
    [ -n "\${DISPLAY:-}" ] || return 0
    if command -v xprop >/dev/null 2>&1; then
        printf '%s\n' "- X11 client list:" >&2
        xprop -root _NET_CLIENT_LIST _NET_CLIENT_LIST_STACKING 2>&1 | tail -n 20 >&2 || true
    fi
    if command -v xwininfo >/dev/null 2>&1; then
        printf '%s\n' "- X11 Claude windows:" >&2
        xwininfo -root -tree 2>&1 | grep -i 'Claude\\|claude' | tail -n 20 >&2 || true
    fi
}

repair_tiny_claude_windows() {
    [ -n "\${DISPLAY:-}" ] || return 0
    command -v xdotool >/dev/null 2>&1 || return 0
    command -v xwininfo >/dev/null 2>&1 || return 0

    repaired=0
    for window in \$(xdotool search --class Claude 2>/dev/null || true); do
        geometry=\$(xwininfo -id "\$window" 2>/dev/null || true)
        width=\$(printf '%s\n' "\$geometry" | awk '/Width:/ {print \$2; exit}')
        height=\$(printf '%s\n' "\$geometry" | awk '/Height:/ {print \$2; exit}')
        [ -n "\$width" ] && [ -n "\$height" ] || continue
        if [ "\$width" -le 100 ] || [ "\$height" -le 100 ]; then
            printf '%s\n' "- repairing tiny Claude X11 window \$window (\${width}x\${height})" >&2
            xdotool windowmap "\$window" 2>/dev/null || true
            xdotool windowsize "\$window" "\${AGENTBOX_CLAUDE_DESKTOP_WIDTH:-1200}" "\${AGENTBOX_CLAUDE_DESKTOP_HEIGHT:-800}" 2>/dev/null || true
            xdotool windowmove "\$window" "\${AGENTBOX_CLAUDE_DESKTOP_X:-80}" "\${AGENTBOX_CLAUDE_DESKTOP_Y:-80}" 2>/dev/null || true
            xdotool windowraise "\$window" 2>/dev/null || true
            xdotool windowactivate "\$window" 2>/dev/null || true
            repaired=1
        fi
    done
    if [ "\$repaired" = "1" ]; then
        printf '%s\n' "- X11 Claude windows after repair:" >&2
        xwininfo -root -tree 2>&1 | grep -i 'Claude\\|claude' | tail -n 20 >&2 || true
    fi
}

run_claude_desktop "\$@" >"\$agentbox_log" 2>&1 &
pid="\$!"

# Give the launcher enough time to write useful diagnostics. If the process is
# still running, keep it attached to this container but print the current log
# tail so a no-window start is not silent.
sleep "\${AGENTBOX_CLAUDE_DESKTOP_DIAGNOSTIC_DELAY:-5}"
if kill -0 "\$pid" 2>/dev/null; then
    printf '%s\n' "- Claude Desktop process is running (pid \$pid)"
    if [ -s "\$runtime_log" ]; then
        printf '%s\n' "- current runtime log lines:" >&2
        tail -n 80 "\$runtime_log" >&2
    elif [ -s "\$agentbox_log" ]; then
        printf '%s\n' "- current launcher output:" >&2
        tail -n 80 "\$agentbox_log" >&2
    else
        printf '%s\n' "- no launcher output yet; runtime log path: \$runtime_log" >&2
    fi
    print_x11_diagnostics
    repair_tiny_claude_windows
    wait "\$pid"
    exit "\$?"
fi

wait "\$pid"
status="\$?"
printf '%s\n' "- Claude Desktop exited with status \$status" >&2
if [ -s "\$runtime_log" ]; then
    printf '%s\n' "- last runtime log lines:" >&2
    tail -n 80 "\$runtime_log" >&2
elif [ -s "\$agentbox_log" ]; then
    printf '%s\n' "- last launcher output:" >&2
    tail -n 80 "\$agentbox_log" >&2
else
    printf '%s\n' "- runtime log is empty or missing: \$runtime_log" >&2
fi
exit "\$status"
EOF
    chmod +x "$launcher_path"
}

ensure_claude_desktop() {
    if [ "${AGENTBOX_GUI:-0}" != "1" ]; then
        return 0
    fi

    root="${AGENTBOX_CLAUDE_DESKTOP_DIR:-/agentbox/claude-desktop}"
    repo="$root/claude-desktop-debian"
    appimage="$root/claude-desktop.AppImage"
    launcher="$root/run-claude-desktop.sh"
    link="$agentbox_bin/claude-desktop"
    build_log="$root/install.log"
    build_tmp="$root/tmp"
    requested=0
    case "${1:-}" in
        claude-desktop|"$launcher"|"$link")
            requested=1
            ;;
    esac

    if [ "$requested" = "0" ]; then
        return 0
    fi

    printf '%s\n' "Claude Desktop:"

    if [ -f "$appimage" ]; then
        mkdir -p "$agentbox_bin"
        write_claude_desktop_launcher "$launcher" "$appimage"
        ln -sfn "$launcher" "$link"
        printf '%s\n' "- available at $link"
        return 0
    fi

    printf '%s\n' "- building AppImage; this can take several minutes on first use"
    if ! mkdir -p "$root" "$agentbox_bin"; then
        printf '%s\n' "- failed to prepare Claude Desktop directories" >&2
        exit 1
    fi
    rm -rf "$build_tmp"
    mkdir -p "$root/cache" "$build_tmp"
    : >"$build_log"
    if [ ! -d "$repo/.git" ]; then
        rm -rf "$repo"
        if ! git clone --depth=1 https://github.com/aaddrick/claude-desktop-debian.git "$repo" \
            >"$build_log" 2>&1
        then
            printf '%s\n' "- clone failed; see $build_log" >&2
            print_build_log_tail "$build_log"
            exit 1
        fi
    fi

    # Keep the default XDG_CACHE_HOME ($HOME/.cache, a persistent writable
    # volume): claude-desktop-debian downloads the Electron binary with
    # @electron/get (which honors XDG_CACHE_HOME) but its system-unzip fallback
    # searches the hardcoded $HOME/.cache/electron, so the two must agree.
    # APPIMAGE_EXTRACT_AND_RUN lets the bundled `appimagetool` (itself an
    # AppImage) run without host FUSE while it packages the output AppImage.
    built=""
    if (
        cd "$repo" &&
            export npm_config_cache="$root/cache/npm" &&
            export TMPDIR="$build_tmp" &&
            export APPIMAGE_EXTRACT_AND_RUN=1 &&
            ./build.sh --build appimage --clean yes >>"$build_log" 2>&1
    ); then
        built="$(find "$repo" -maxdepth 1 -name '*.AppImage' -print 2>/dev/null | head -n 1)"
    fi

    if [ -z "$built" ]; then
        rm -rf "$build_tmp"
        printf '%s\n' "- build failed; see $build_log" >&2
        print_build_log_tail "$build_log"
        exit 1
    fi

    # Stage the AppImage at a stable path and wrap it with a launcher.
    mv -f "$built" "$appimage"
    chmod +x "$appimage"
    write_claude_desktop_launcher "$launcher" "$appimage"
    rm -rf "$build_tmp"
    ln -sfn "$launcher" "$link"
    printf '%s\n' "- installed at $link"
}

configure_host_browser
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
ensure_claude_desktop "$@"

if [ -n "${AGENTBOX_PACKAGE_MANAGER:-}" ] || [ "${AGENTBOX_RUST_PROJECT:-0}" = "1" ]; then
    printf '%s\n' "Project tools:"
    if [ -n "${AGENTBOX_PACKAGE_MANAGER:-}" ]; then
        printf '%s\n' "- Package manager: $AGENTBOX_PACKAGE_MANAGER"
    fi
    if [ "${AGENTBOX_RUST_PROJECT:-0}" = "1" ]; then
        printf '%s\n' "- Rust: cargo and rustc available"
    fi
fi

if [ "${1:-}" = "codex-login" ]; then
    shift
    exec codex login --device-auth "$@"
fi

exec "$@"
