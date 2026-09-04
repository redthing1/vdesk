# Architecture

`vdesk` is one CLI and one OCI image with three clear responsibilities:

```text
host CLI ── lifecycle ──> Podman or Docker
host CLI ── desktop API ─> vdesk service ─> X11 desktop
browser  ── noVNC ──────> vdesk service ─> x11vnc
agent container ────────> vdesk service
```

The host owns container lifecycle. The service owns desktop state and serializes
operations. noVNC exposes the same display to a person. A sibling agent receives
only a desktop endpoint, a token, and a static client.

## Components

- `vdesk-protocol` defines strict wire types, limits, and errors.
- `vdesk` provides the CLI, Podman/Docker adapter, HTTP client, desktop service,
  X11 driver, process supervisor, and noVNC bridge.
- `container/Containerfile` builds the static client and the desktop images.

One binary serves the host, service, helper, and client roles. This keeps
installation and versioning simple while the protocol crate preserves a clean
compatibility boundary.

## Desktop runtime

The container starts Xvfb at fixed geometry, D-Bus, Xfce, the desktop service,
x11vnc, and noVNC. Screenshots are captured directly from X11 as lossless PNGs;
agent coordinates never depend on the resized or compressed browser view. Input
is injected through XTest.

The `minimal` image contains the runtime. The `default` image adds Chromium,
Mousepad, and Thunar. Additional applications belong in derived images.

## Sessions

Sessions are named containers discovered through labels, so there is no host
daemon or registry service.

| Operation | Result |
| --- | --- |
| `open` | Reuse a healthy session or create one and wait until ready. |
| `stop` | Stop the container while preserving its writable state. |
| `open` after `stop` | Resume the same container. |
| `reset` | Replace the container and rotate its credentials. |
| `delete` | Remove the container, network, and local descriptor. |

The optional workspace is a host directory mounted at `/workspace`. Downloads
live in the container and disappear on reset. Session credentials and endpoints
are stored outside the workspace with private permissions.

## Data plane

The authenticated HTTP API supplies observations, input batches, windows,
accessibility data, clipboard text, scoped files, application launch, and
managed processes. The CLI is the supported client and provides both concise
human output and stable JSON.

An observation carries its native geometry, runtime generation, observation ID,
input generation, timestamp, PNG metadata, and cursor state. Batches can require
the observed input generation. If human or other input changed the desktop, the
service rejects stale work before injecting it.

Every batch runs under one lock. Its actions execute in order, report individual
delivery results, and optionally capture an observation after each action or at
the end. Reusing a request ID returns the recorded result instead of repeating
input.

Files are confined to `/workspace` and `/downloads`. Processes use explicit
argument vectors, bounded output, and a managed registry; the API does not
evaluate shell strings.

See [protocol.md](protocol.md) for the wire semantics.

## Human view

`vdesk view` constructs an authenticated noVNC URL. The service bridges its
WebSocket to a private loopback-only x11vnc server, so raw VNC is not published.
Human and agent input intentionally share the display. Human events advance the
input generation and can be detected without imposing a takeover workflow.

## Containerized agents

`vdesk run` starts an agent on the session network and mounts two read-only
files: the static client and a descriptor containing only data-plane authority.
It does not mount the Podman or Docker socket, host display, session lifecycle
state, or workspace path.

## Security boundary

The recommended deployment uses a rootless engine. Desktop and sibling agent
containers run without added capabilities, with `no-new-privileges` and resource
limits. The service requires per-session credentials, host ports bind to
loopback, and file and process operations are bounded.

These controls limit authority; they do not turn a shared-kernel container into
a hostile multi-tenant VM. Workloads that require a stronger kernel boundary
should run the whole topology inside a VM or use a future VM-backed provider.
