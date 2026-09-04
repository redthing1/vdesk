# vdesk

`vdesk` gives an agent and a person the same containerized Linux desktop. The
agent gets exact screenshots and a typed CLI; the person gets an interactive
noVNC view. Podman is preferred and Docker supports the same workflow.

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

## Images

```sh
vdesk image build --profile minimal --tag localhost/vdesk:minimal
vdesk image build --profile default --tag localhost/vdesk:dev
```

The minimal image contains Xvfb, Xfce, x11vnc, noVNC, AT-SPI, and the vdesk
service. The default image adds Chromium, Mousepad, and Thunar.

The desktop runs as a non-root user with dropped capabilities,
`no-new-privileges`, resource limits, one scoped workspace mount, and no engine
socket. It is a shared-kernel container, not a hostile multi-tenant VM.
Chromium currently uses `--no-sandbox` inside that container.

## Develop

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings

VDESK_AGENT_TESTS=1 tests/e2e.sh podman
VDESK_AGENT_TESTS=1 tests/e2e.sh docker
```

The E2E suite additionally needs `curl`, `jq`, and Bun. See
[doc/architecture.md](doc/architecture.md) for the design and
[doc/protocol.md](doc/protocol.md) for the data-plane contract.
