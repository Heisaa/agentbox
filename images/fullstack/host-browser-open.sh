#!/bin/sh
set -u

url=""
if [ "${1:-}" = "open" ]; then
    shift
fi

for argument in "$@"; do
    case "$argument" in
        http://*|https://*)
            url="$argument"
            break
            ;;
    esac
done

if [ -z "$url" ] && [ "$#" -eq 1 ]; then
    url="$1"
fi

case "$url" in
    http://*|https://*) ;;
    *)
        printf '%s\n' "agentbox-open: expected an http:// or https:// URL" >&2
        exit 2
        ;;
esac

if [ -z "${AGENTBOX_HOST_BROWSER_URL:-}" ] || [ -z "${AGENTBOX_HOST_BROWSER_TOKEN:-}" ]; then
    printf '%s\n' "agentbox-open: host browser bridge is not configured" >&2
    exit 1
fi

curl \
    --fail \
    --silent \
    --show-error \
    --max-time 10 \
    --request POST \
    --header "X-Agentbox-Token: ${AGENTBOX_HOST_BROWSER_TOKEN:-}" \
    --data-binary "$url" \
    "${AGENTBOX_HOST_BROWSER_URL:-}" >/dev/null
