# Working on vdesk

vdesk is a small, provider-neutral executor for an isolated graphical Linux
desktop. The same runtime works in a vdesk-managed OCI desktop or inside an
existing sandbox. Keep the common path simple: short-lived CLI calls operate
one durable desktop, and `vdesk view` lets a person help immediately.

## Invariants

- Keep one public CLI and one versioned data-plane protocol.
- Podman is preferred; Docker must retain the same public behavior.
- The outer trusted environment owns lifecycle. The desktop service owns
  display state and serializes agent actions.
- An ordinary agent container must never receive the host engine socket,
  lifecycle credentials, host display, or arbitrary host mounts.
- noVNC is the human interface. Agent observations come directly from the
  framebuffer at their original geometry.
- Human input requires no takeover ceremony. Detect interference and make the
  agent observe again.
- Do not report action delivery as application-level success.
- Keep shell execution explicit. Process and application APIs accept argv;
  never construct shell commands from agent input.
- Keep file access beneath the explicitly configured workspace/download roots,
  with bytes crossing the protocol rather than shared-path assumptions.
- Prefer a small ordinary interface over plugins, policy languages, brokers,
  or host daemons. Add machinery only for a demonstrated workflow.

## Code map

- `crates/vdesk-protocol`: strict wire types, limits, and errors; no engine,
  desktop, viewer, or model dependencies.
- `crates/vdesk/src/cli.rs`: public UX and lifecycle orchestration.
- `client.rs`: authenticated HTTP client and safe local artifact writes.
- `engine.rs`: narrow Podman/Docker command adapter.
- `service.rs`: authenticated API, action serialization, resources, files, and
  managed processes.
- `desktop.rs`: X11 capture/input, windows, clipboard, launch, and AT-SPI bridge.
- `supervisor.rs`: D-Bus, Xvfb, Xfce, x11vnc, readiness, and shutdown.
- `viewer.rs`: authenticated WebSocket bridge to one fixed private RFB target.
- `state.rs`: private descriptors, credentials, and runtime generations.
- `container/Containerfile`: neutral runtime artifacts plus managed images.
- `tests/e2e.sh`: managed Podman/Docker behavior and boundary checks.
- `tests/embedded-e2e.sh`: in-sandbox runtime and exec-transport viewer checks.

## Before changing behavior

Read [doc/architecture.md](doc/architecture.md) and
[doc/protocol.md](doc/protocol.md). Preserve existing CLI and JSON behavior
unless the task explicitly changes the contract. Put provider or model
translation above the client; do not leak it into the protocol.

When adding a protocol field or action:

1. Define and validate it in `vdesk-protocol`.
2. Implement it behind the desktop driver or service boundary.
3. Expose matching human and JSON CLI behavior where appropriate.
4. Test malformed input and the real desktop effect at the lowest useful layer.

## Verification

Always run:

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

For runtime, lifecycle, networking, viewer, or image changes, build the affected
image and run:

```sh
VDESK_AGENT_TESTS=1 tests/e2e.sh podman
VDESK_AGENT_TESTS=1 tests/e2e.sh docker
tests/embedded-e2e.sh podman
tests/embedded-e2e.sh docker
```

The E2E suite needs `curl`, `jq`, and Bun. It owns a uniquely named session and
cleans it up; confirm no `io.vdesk.managed=true` test containers remain after a
failure.

Keep [README.md](README.md), [doc/](doc/), and
[skills/vdesk/SKILL.md](skills/vdesk/SKILL.md) aligned with observable behavior.
