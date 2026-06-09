#!/usr/bin/env bash

set -euo pipefail

readonly script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly image="${AGENTBOX_IMAGE:-agentbox/fullstack:latest}"
readonly install_root="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}"
readonly cargo_bin="${install_root%/}/bin"

require_command() {
    local command_name="$1"
    if ! command -v "$command_name" >/dev/null 2>&1; then
        printf 'error: required command not found: %s\n' "$command_name" >&2
        exit 1
    fi
}

require_command cargo
require_command docker

printf 'Building Agentbox runtime image: %s\n' "$image"
docker build \
    --tag "$image" \
    --file "$script_dir/images/fullstack/Dockerfile" \
    "$script_dir"

printf '\nInstalling Agentbox CLI with Cargo\n'
cargo install --locked --force --root "$install_root" --path "$script_dir"

printf '\nAgentbox installation complete.\n'
case ":${PATH:-}:" in
    *":$cargo_bin:"*)
        printf 'The Cargo bin directory is already in PATH: %s\n' "$cargo_bin"
        ;;
    *)
        printf 'Add the Cargo bin directory to PATH for this shell:\n'
        printf '  export PATH="%s:%s"\n' "$cargo_bin" "\$PATH"
        printf 'Add the same command to your shell profile to make it persistent.\n'
        ;;
esac
