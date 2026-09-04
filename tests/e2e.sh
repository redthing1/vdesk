#!/bin/sh
set -eu

engine=${1:-podman}
image=${VDESK_TEST_IMAGE:-localhost/vdesk:dev}
binary=${VDESK_BIN:-target/debug/vdesk}
session="e2e-${engine}-$$"
temporary=$(mktemp -d "/tmp/vdesk-e2e-${engine}.XXXXXX")
state="$temporary/state"
workspace="$temporary/workspace"
mkdir -m 700 "$state" "$workspace"

cleanup() {
    if test "${VDESK_KEEP:-0}" = 1; then
        printf 'preserved failed test state at %s\n' "$temporary" >&2
        return
    fi
    VDESK_STATE_DIR="$state" "$binary" --session "$session" delete >/dev/null 2>&1 || true
    case "$temporary" in
        /tmp/vdesk-e2e-*) rm -rf -- "$temporary" ;;
    esac
}
trap cleanup EXIT HUP INT TERM

VDESK_STATE_DIR="$state" "$binary" --session "$session" open \
    --engine "$engine" --image "$image" --workspace "$workspace" --size 1024x768
printf 'checking protocol and framebuffer\n'

descriptor="$state/sessions/$session.json"
endpoint=$(jq -r .host_endpoint "$descriptor")
viewer_token=$(jq -r .viewer_token "$descriptor")
container=$(jq -r .container_name "$descriptor")

test "$(curl --silent --output /dev/null --write-out '%{http_code}' "$endpoint/v1/health")" = 401

VDESK_STATE_DIR="$state" "$binary" --json --session "$session" capabilities \
    | jq -e '.protocol == 1 and .geometry == {"width":1024,"height":768,"dpi":96,"scale":1}' >/dev/null
VDESK_STATE_DIR="$state" "$binary" --session "$session" see --output "$temporary/before.png"
VDESK_STATE_DIR="$state" "$binary" --session "$session" click 20 20
VDESK_STATE_DIR="$state" "$binary" --session "$session" see --output "$temporary/after.png"
test "$(sha256sum "$temporary/before.png" | cut -d ' ' -f 1)" != \
     "$(sha256sum "$temporary/after.png" | cut -d ' ' -f 1)"

printf 'checking viewer and human input\n'
idle_generation=$(VDESK_STATE_DIR="$state" "$binary" --json --session "$session" cursor | jq .input_generation)
curl --silent --show-error --fail --output /dev/null "$endpoint/novnc/vnc.html"
test "$(VDESK_STATE_DIR="$state" "$binary" --json --session "$session" cursor | jq .input_generation)" = "$idle_generation"
test "$(curl --silent --output /dev/null --write-out '%{http_code}' \
    -H 'Connection: Upgrade' -H 'Upgrade: websocket' -H 'Sec-WebSocket-Version: 13' \
    -H 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==' \
    "$endpoint/viewer/ws?token=invalid-credential")" = 401

if command -v bun >/dev/null 2>&1; then
    VDESK_ENDPOINT="$endpoint" VDESK_VIEWER_TOKEN="$viewer_token" bun tests/rfb_click.js
    sleep 0.2
    human_generation=$(VDESK_STATE_DIR="$state" "$binary" --json --session "$session" cursor | jq .input_generation)
    test "$human_generation" -gt "$idle_generation"

    jq -n --argjson generation "$human_generation" --arg request "e2e-interference-$$" \
        '{protocol:1, request_id:$request, expected_input_generation:$generation,
          actions:[{type:"wait",duration_ms:500},{type:"click",x:300,y:300}], observe:"none"}' \
        > "$temporary/interference.json"
    VDESK_STATE_DIR="$state" "$binary" --json --session "$session" \
        batch "$temporary/interference.json" > "$temporary/interference-result.json" &
    batch_pid=$!
    sleep 0.1
    VDESK_ENDPOINT="$endpoint" VDESK_VIEWER_TOKEN="$viewer_token" bun tests/rfb_click.js
    wait "$batch_pid"
    jq -e '.interference_detected == true' "$temporary/interference-result.json" >/dev/null
fi

printf 'checking GUI, Unicode, windows, and accessibility\n'
VDESK_STATE_DIR="$state" "$binary" --session "$session" move 0 0
VDESK_STATE_DIR="$state" "$binary" --session "$session" move 1023 767
VDESK_STATE_DIR="$state" "$binary" --session "$session" move 1023 767
if VDESK_STATE_DIR="$state" "$binary" --session "$session" move 1024 768 >/dev/null 2>&1; then
    echo 'out-of-bounds move unexpectedly succeeded' >&2
    exit 1
fi
VDESK_STATE_DIR="$state" "$binary" --session "$session" drag 400 400 450 450 --duration-ms 100
VDESK_STATE_DIR="$state" "$binary" --session "$session" scroll 1 1
VDESK_STATE_DIR="$state" "$binary" --session "$session" key ESC
jq -n --arg request "e2e-input-families-$$" \
    '{protocol:1, request_id:$request,
      actions:[
        {type:"move",x:500,y:500,duration_ms:10},
        {type:"mouse_down",button:"middle"},
        {type:"mouse_up",button:"middle"},
        {type:"click",x:500,y:500,button:"right",count:1},
        {type:"key_press",keys:["ESC"],repeat:1},
        {type:"key_down",key:"SHIFT"},
        {type:"key_up",key:"SHIFT"},
        {type:"hold",keys:["CTRL"],duration_ms:10},
        {type:"wait",duration_ms:10}
      ], observe:"none"}' > "$temporary/input-families.json"
VDESK_STATE_DIR="$state" "$binary" --json --session "$session" \
    batch "$temporary/input-families.json" \
    | jq -e '.executed == 9 and .failed == 0 and
        (.actions | all(.status == "completed")) and
        (.actions[0:8] | all(.delivery == "attempted")) and
        .actions[8].delivery == "not_attempted"' >/dev/null
VDESK_STATE_DIR="$state" "$binary" --session "$session" launch mousepad
mousepad_id=
attempt=0
while test "$attempt" -lt 30; do
    mousepad_id=$(VDESK_STATE_DIR="$state" "$binary" --json --session "$session" windows \
        | jq -r '.windows[] | select(.title | contains("Mousepad")) | .id' | head -1)
    test -n "$mousepad_id" && break
    attempt=$((attempt + 1))
    sleep 0.1
done
test -n "$mousepad_id"
VDESK_STATE_DIR="$state" "$binary" --session "$session" windows --focus "$mousepad_id" >/dev/null
VDESK_STATE_DIR="$state" "$binary" --session "$session" type 'Unicode — café ✓'
sleep 0.3
VDESK_STATE_DIR="$state" "$binary" --session "$session" key CTRL+A
VDESK_STATE_DIR="$state" "$binary" --session "$session" key CTRL+C
sleep 0.3
test "$(VDESK_STATE_DIR="$state" "$binary" --session "$session" clipboard get)" = 'Unicode — café ✓'
VDESK_STATE_DIR="$state" "$binary" --json --session "$session" windows \
    | jq -e '.windows | any(.title | contains("Mousepad"))' >/dev/null
VDESK_STATE_DIR="$state" "$binary" --json --session "$session" a11y \
    | jq -e '.nodes | length > 0' >/dev/null
browser_id=$(VDESK_STATE_DIR="$state" "$binary" --json --session "$session" \
    process start -- chromium --no-first-run about:blank | jq -r .process_id)
sleep 2
VDESK_STATE_DIR="$state" "$binary" --json --session "$session" process status "$browser_id" \
    | jq -e '.state == "running"' >/dev/null
VDESK_STATE_DIR="$state" "$binary" --json --session "$session" windows \
    | jq -e '.windows | any(.title | contains("Chromium"))' >/dev/null
VDESK_STATE_DIR="$state" "$binary" --session "$session" process kill "$browser_id" >/dev/null

printf 'checking scoped files and managed processes\n'
VDESK_STATE_DIR="$state" "$binary" --session "$session" import README.md workspace/readme.md
VDESK_STATE_DIR="$state" "$binary" --session "$session" export workspace/readme.md "$temporary/readme.md"
cmp README.md "$temporary/readme.md"

process_id=$(VDESK_STATE_DIR="$state" "$binary" --json --session "$session" \
    process start -- /usr/bin/printf 'process-ok\n' | jq -r .process_id)
VDESK_STATE_DIR="$state" "$binary" --session "$session" process wait "$process_id" --timeout 5s
test "$(VDESK_STATE_DIR="$state" "$binary" --session "$session" process output "$process_id")" = process-ok

printf 'checking container boundary\n'
"$engine" inspect "$container" | jq -e \
    '.[0].Config.User == "vdesk" and (.[0].HostConfig.Privileged | not) and
     (.[0].HostConfig.PortBindings | keys == ["7777/tcp"]) and
     (.[0].HostConfig.SecurityOpt | any(contains("no-new-privileges")))' >/dev/null

if test "${VDESK_AGENT_TESTS:-0}" = 1; then
    printf 'checking Debian and Alpine agent siblings\n'
    VDESK_STATE_DIR="$state" "$binary" --session "$session" run \
        --agent-image docker.io/library/debian:bookworm-slim -- \
        sh -c 'test ! -S /var/run/docker.sock && ! command -v podman && vdesk --json capabilities >/dev/null'
    VDESK_STATE_DIR="$state" "$binary" --session "$session" run \
        --agent-image docker.io/library/alpine:3.22 -- \
        sh -c 'test ! -S /var/run/docker.sock && ! command -v podman && vdesk --json cursor >/dev/null'
fi

printf 'checking stop and resume\n'
VDESK_STATE_DIR="$state" "$binary" --session "$session" stop
VDESK_STATE_DIR="$state" "$binary" --session "$session" open
VDESK_STATE_DIR="$state" "$binary" --json --session "$session" status \
    | jq -e '.container.running and .service_ready' >/dev/null
VDESK_STATE_DIR="$state" "$binary" --session "$session" import README.md downloads/ephemeral.md
old_data_token=$(jq -r .data_token "$descriptor")
old_generation=$(jq -r .runtime_generation "$descriptor")
VDESK_STATE_DIR="$state" "$binary" --session "$session" reset
new_endpoint=$(jq -r .host_endpoint "$descriptor")
new_data_token=$(jq -r .data_token "$descriptor")
new_generation=$(jq -r .runtime_generation "$descriptor")
test "$old_data_token" != "$new_data_token"
test "$new_generation" -eq $((old_generation + 1))
test "$(curl --silent --output /dev/null --write-out '%{http_code}' \
    -H "Authorization: Bearer $old_data_token" "$new_endpoint/v1/health")" = 401
if VDESK_STATE_DIR="$state" "$binary" --session "$session" \
    export downloads/ephemeral.md "$temporary/should-not-exist" >/dev/null 2>&1; then
    echo 'reset unexpectedly retained session-owned downloads' >&2
    exit 1
fi
VDESK_STATE_DIR="$state" "$binary" --session "$session" export \
    workspace/readme.md "$temporary/workspace-after-reset.md"
cmp README.md "$temporary/workspace-after-reset.md"

printf 'vdesk %s end-to-end test passed\n' "$engine"
