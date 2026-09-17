#!/bin/sh
set -eu

engine=${1:-podman}
image=${VDESK_TEST_IMAGE:-localhost/vdesk:dev}
binary=${VDESK_BIN:-target/debug/vdesk}
container="vdesk-embedded-${engine}-$$"
temporary=$(mktemp -d "/tmp/vdesk-embedded-${engine}.XXXXXX")
viewer_pid=

cleanup() {
    if test -n "$viewer_pid"; then
        kill "$viewer_pid" >/dev/null 2>&1 || true
        wait "$viewer_pid" >/dev/null 2>&1 || true
    fi
    "$engine" rm -f "$container" >/dev/null 2>&1 || true
    if test "${VDESK_KEEP:-0}" = 1; then
        printf 'preserved embedded test state at %s\n' "$temporary" >&2
        return
    fi
    case "$temporary" in
        /tmp/vdesk-embedded-*) rm -rf -- "$temporary" ;;
    esac
}
trap cleanup EXIT HUP INT TERM

"$engine" run --detach --name "$container" --network none \
    --entrypoint /usr/local/bin/vdesk "$image" serve --local >/dev/null

printf 'checking embedded runtime and local discovery\n'
attempt=0
while test "$attempt" -lt 30; do
    if "$engine" exec "$container" vdesk --json capabilities \
        > "$temporary/capabilities.json" 2>/dev/null; then
        break
    fi
    attempt=$((attempt + 1))
    sleep 0.2
done
test "$attempt" -lt 30
jq -e '.protocol == 2 and .hardware_acceleration == false and
        .files == false and .process == false and .viewer == true' \
    "$temporary/capabilities.json" >/dev/null
if "$engine" exec "$container" vdesk --session another capabilities >/dev/null 2>&1; then
    printf 'a differently named session unexpectedly selected the local runtime\n' >&2
    exit 1
fi

"$engine" exec "$container" vdesk --json see --output /tmp/embedded.png \
    > "$temporary/observation.json"
"$engine" exec "$container" test -s /tmp/embedded.png
"$engine" exec "$container" sh -lc \
    'test "$(stat -c %a "$XDG_RUNTIME_DIR/vdesk/client.json")" = 600'
"$engine" exec "$container" sh -lc \
    '! grep -Eq '"'"'engine|container|network|host_endpoint|viewer_token'"'"' "$XDG_RUNTIME_DIR/vdesk/client.json"'

before_hash=$("$engine" exec "$container" sha256sum /tmp/embedded.png | cut -d ' ' -f 1)
"$engine" exec "$container" vdesk click 20 20 >/dev/null
"$engine" exec "$container" vdesk see --output /tmp/embedded-after.png >/dev/null
after_hash=$("$engine" exec "$container" sha256sum /tmp/embedded-after.png | cut -d ' ' -f 1)
test "$before_hash" != "$after_hash"

printf 'checking launched-child reaping\n'
zombies_before=$("$engine" exec "$container" sh -lc "ps -eo stat= | grep -c '^Z' || true")
attempt=0
while test "$attempt" -lt 5; do
    "$engine" exec "$container" vdesk launch /bin/true >/dev/null
    attempt=$((attempt + 1))
done
sleep 0.3
zombies_after=$("$engine" exec "$container" sh -lc "ps -eo stat= | grep -c '^Z' || true")
test "$zombies_after" = "$zombies_before"

printf 'checking embedded viewer over explicit argv\n'
"$binary" view --print-url -- \
    "$engine" exec -i "$container" vdesk rfb-stdio \
    > "$temporary/viewer-url" 2> "$temporary/viewer-log" &
viewer_pid=$!

attempt=0
while test "$attempt" -lt 30; do
    if test -s "$temporary/viewer-url"; then
        break
    fi
    if ! kill -0 "$viewer_pid" 2>/dev/null; then
        cat "$temporary/viewer-log" >&2
        exit 1
    fi
    attempt=$((attempt + 1))
    sleep 0.1
done
test -s "$temporary/viewer-url"
viewer_url=$(head -1 "$temporary/viewer-url")
endpoint=${viewer_url%%/novnc/*}
viewer_token=${viewer_url##*token%3D}
curl --silent --show-error --fail --output /dev/null "$endpoint/novnc/vnc.html"
curl --silent --show-error --fail --output /dev/null "$endpoint/novnc/viewer.js"
curl --silent --show-error --fail --output "$temporary/rfb.js" "$endpoint/novnc/rfb.js"
test "$(wc -c < "$temporary/rfb.js")" -gt 100000

if command -v bun >/dev/null 2>&1; then
    idle_generation=$("$engine" exec "$container" vdesk --json cursor | jq .input_generation)
    VDESK_ENDPOINT="$endpoint" VDESK_VIEWER_TOKEN="$viewer_token" bun tests/rfb_click.js
    sleep 0.2
    human_generation=$("$engine" exec "$container" vdesk --json cursor | jq .input_generation)
    test "$human_generation" -gt "$idle_generation"
fi

printf 'checking embedded container boundary\n'
"$engine" inspect "$container" | jq -e \
    '.[0].HostConfig.NetworkMode == "none" and (.[0].HostConfig.Privileged | not) and
     ((.[0].HostConfig.PortBindings // {}) | length == 0)' >/dev/null

printf 'vdesk %s embedded end-to-end test passed\n' "$engine"
