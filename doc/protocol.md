# Data-plane protocol

The CLI is the normal interface. `--json` exposes stable responses for agents
and scripts, while `vdesk capabilities` reports the operations and limits of the
running service.

Protocol v1 uses authenticated HTTP with JSON request and response bodies.
Screenshots are separate lossless PNG resources. Unknown JSON fields, unsupported
protocol versions, invalid coordinates, oversized payloads, and unbalanced held
input are rejected before execution.

## Observation loop

```sh
vdesk --json capabilities
vdesk --json see --output desktop.png --include-cursor
```

The observation response correlates `desktop.png` with:

- `runtime_generation`, which changes when the container is replaced;
- `observation_id`, which increases for every capture;
- `input_generation`, which increases when input reaches the desktop;
- exact `geometry`, timestamp, cursor, content length, and SHA-256;
- the coordinate space and whether the cursor is included in the PNG.

Coordinates are integer framebuffer pixels. `(0,0)` is the top-left pixel and
`(width-1,height-1)` is the bottom-right pixel. Always use the original PNG rather
than a scaled preview.

## Actions

Simple CLI operations create one-action batches:

```sh
vdesk click 640 400
vdesk type 'hello'
vdesk key CTRL+S
vdesk scroll 0 6
```

For a coherent sequence, pass a batch file:

```json
{
  "protocol": 1,
  "request_id": "save-document-1",
  "expected_input_generation": 12,
  "actions": [
    { "type": "click", "x": 640, "y": 400 },
    { "type": "type_text", "text": "hello", "delay_ms": 0 },
    { "type": "key_press", "keys": ["CTRL", "S"], "repeat": 1 }
  ],
  "stop_on_error": true,
  "observe": "final"
}
```

```sh
vdesk --json batch actions.json
```

Supported action families are `move`, `click`, `mouse_down`, `mouse_up`, `drag`,
`scroll`, `type_text`, `key_press`, `key_down`, `key_up`, `hold`, and `wait`.
`observe` is `none`, `final`, or `each`.

A batch is serialized with all other data-plane input. Its result distinguishes
attempted delivery from task success, includes per-action errors, records input
generation before and after execution, and reports detected interference. At
most 64 actions execute in one batch.

`request_id` provides bounded idempotency. A retry with the same ID and identical
body receives the first result. Reusing an ID with a different body is an error.

## Structured desktop context

```sh
vdesk --json windows
vdesk windows --focus WINDOW_ID
vdesk --json a11y
vdesk clipboard get
vdesk clipboard set 'text'
```

Window and AT-SPI data supplement the screenshot. They are not guaranteed to
represent custom, canvas, or inaccessible controls, so pixels remain the source
of truth.

Direct ASCII typing uses keyboard events. Text that cannot be typed reliably
through the active X11 layout is written to the clipboard and pasted, then
verified; that path intentionally leaves the text in the clipboard.

## Applications and processes

```sh
vdesk launch mousepad
vdesk --json process start --cwd workspace -- python3 task.py
vdesk process wait PROCESS_ID --timeout 30s
vdesk process output PROCESS_ID
vdesk process kill PROCESS_ID
```

Both launch paths accept explicit argv. Managed processes additionally expose
status, exit code, and bounded combined output. No command is reparsed through a
shell.

## Files

```sh
vdesk import ./input.pdf workspace/input.pdf
vdesk export downloads/result.pdf ./result.pdf
```

Remote paths must begin with `workspace/` or `downloads/`. The service normalizes
the relative path, refuses traversal and symlinks, bounds payload size, and
returns byte count and SHA-256 metadata. Local exports are replaced atomically.

## Authentication and errors

Every `/v1` endpoint requires the session data token. Viewer WebSocket access
uses a separate viewer token. Errors use one JSON envelope with a stable code,
message, and retryability flag. Secrets are excluded from normal status and JSON
output.

The endpoint list is intentionally narrow: health, capabilities, observations,
images, actions, windows, accessibility, clipboard, launch, processes, and files.
It does not expose container lifecycle or arbitrary shell execution.
