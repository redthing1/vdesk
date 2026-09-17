# vdesk

`vdesk` gives an agent and a person the same isolated Linux desktop. The agent
gets exact screenshots and a typed CLI; the person gets an interactive noVNC
view. One runtime supports two deliberately small deployment shapes:

- `vdesk open` manages a ready-made desktop container.
- `vdesk serve --local` runs the desktop inside an existing container, mimchine,
  or VM.

Podman is preferred for managed desktops and Docker has the same public
behavior. Embedded mode does not need either engine inside the sandbox.

The v1 implementation is tested on amd64 Linux with rootless Podman and Docker.

## Quick start

Requires Rust 1.97+ and either Podman or Docker.

```sh
cargo install --locked --path crates/vdesk
vdesk image build
vdesk open
vdesk view
```

`image build` and `open` prefer Podman. Pass `--engine docker` to select Docker.
Images are engine-local, so build and open with the same engine.
`open` mounts the current directory read-write at `/workspace`; use
`--no-workspace` or `--workspace PATH` to choose otherwise.

## Use the desktop

```sh
vdesk see --output desktop.png --include-cursor
vdesk click 612 438
vdesk type 'Hello, desktop'
vdesk key CTRL+S
```

Coordinates refer to the original screenshot, with `(0,0)` at its top-left.
Use `--json` for machine-readable output and `--session NAME` for another
desktop.

Useful structured operations:

```sh
vdesk capabilities
vdesk windows
vdesk windows --focus WINDOW_ID
vdesk a11y
vdesk clipboard get
vdesk launch mousepad

vdesk import ./report.pdf workspace/report.pdf
vdesk export downloads/result.pdf ./result.pdf

vdesk process start -- python3 task.py
vdesk process wait PROCESS_ID --timeout 30s
vdesk process output PROCESS_ID
```

Action batches are versioned JSON with a stable request ID. They execute in
order, report partial completion, and can reject work based on a stale input
generation. See `vdesk batch --help` and `vdesk capabilities`.

## Sessions

| Command | Behavior |
|---|---|
| `vdesk open` | Create, resume, or reconnect |
| `vdesk stop` | Stop while preserving container state |
| `vdesk reset` | Recreate the desktop, rotate credentials, retain the mounted workspace |
| `vdesk delete` | Remove the desktop container and session network |

New sessions are online by default. `vdesk open --offline` creates an internal
engine network. Offline host access works with rootless Podman and native Linux
Docker; Docker Desktop has not been verified.

## Containerized agents

The trusted host can start an arbitrary sibling agent without sharing the
container-engine socket:

```sh
vdesk open
vdesk run --agent-image debian:bookworm-slim -- my-agent --task task.txt
```

The agent receives only a static `vdesk` client and a read-only data-plane
descriptor. The desktop remains running when the agent exits, so a person can
inspect or help through `vdesk view`.

Agent instructions are in [skills/vdesk/SKILL.md](skills/vdesk/SKILL.md).

## Existing containers and VMs

Install the runtime artifacts and graphical packages in the existing image,
then run one foreground process:

```sh
exec vdesk serve --local
```

Commands inside that same sandbox discover it automatically:

```sh
vdesk capabilities
vdesk see --output /tmp/desktop.png
vdesk launch xfce4-terminal
```

To help from the host, pass the sandbox's ordinary interactive exec command as
an explicit argv vector:

```sh
vdesk view -- mim exec -i work -- vdesk rfb-stdio
vdesk view -- podman exec -i work vdesk rfb-stdio
```

The noVNC client is built into the host binary. The bridge exposes only the
runtime's fixed private RFB endpoint; it is not a general proxy. There is no
second container, published port, engine socket, shared display, or
vdesk-specific provider integration.

See [doc/embedded.md](doc/embedded.md) for image integration, the optional
mimchine startup hook, scoped files/processes, and the trust boundary.

## Images

```sh
vdesk image build --profile minimal --tag localhost/vdesk:minimal
vdesk image build --profile default --tag localhost/vdesk:dev
```

The minimal image contains Xvfb, Xfce, x11vnc, AT-SPI, and the vdesk
service. The default image adds Chromium, Mousepad, and Thunar.

The `artifacts` target contains only the static binary and AT-SPI helper. Copy it
into any image and install that distribution's graphical packages; vdesk does
not replace the image's user, HOME, workdir, entrypoint, or lifecycle.

The desktop runs as a non-root user with dropped capabilities,
`no-new-privileges`, resource limits, one scoped workspace mount, and no engine
socket. It is a shared-kernel container, not a hostile multi-tenant VM.
Chromium currently uses `--no-sandbox` inside that container.

The current Xvfb backend is software-rendered. Proper GUI GPU acceleration
requires a different display backend as well as explicit device delegation;
passing `/dev/dri` to Xvfb alone is not presented as acceleration.

## Develop

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings

VDESK_AGENT_TESTS=1 tests/e2e.sh podman
VDESK_AGENT_TESTS=1 tests/e2e.sh docker
tests/embedded-e2e.sh podman
tests/embedded-e2e.sh docker
```

The E2E suite additionally needs `curl`, `jq`, and Bun. See
[doc/architecture.md](doc/architecture.md) for the design and
[doc/protocol.md](doc/protocol.md) for the data-plane contract.
