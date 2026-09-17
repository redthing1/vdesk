# Architecture

`vdesk` is one runtime, one CLI, and one versioned protocol with two deployment
shapes:

```text
                         common runtime
                supervisor / service / X11
                           /       \
                          /         \
        managed desktop OCI       existing sandbox or VM
        host vdesk owns it         outer tool owns it
```

In managed mode the trusted host CLI owns a Podman or Docker container. In
embedded mode the runtime is an ordinary foreground process inside an existing
container, mimchine, or VM. The runtime contains no engine or mim dependency.
In both modes the service owns desktop state, serializes operations, and exposes
the same display to the agent and person.

## Components

- `vdesk-protocol` defines strict wire types, limits, and errors.
- `vdesk` provides the CLI, Podman/Docker adapter, HTTP client, desktop service,
  X11 driver, process supervisor, and noVNC bridge.
- `container/Containerfile` builds neutral runtime artifacts and the managed
  desktop images.

One binary serves the host, service, helper, and client roles. This keeps
installation and versioning simple while the protocol crate preserves a clean
compatibility boundary.

## Desktop runtime

`vdesk serve` starts an X11 server at fixed geometry, D-Bus, Xfce, the desktop
service, and a loopback-only x11vnc process. It prefers Xvnc and falls back to
Xvfb when Xvnc is absent. It stays in the foreground, owns and reaps its
children, and shuts them down on TERM or INT. Screenshots are captured directly
from X11 as lossless PNGs; agent coordinates never depend on the resized or
compressed browser view. Input is injected through XTest.

The managed `minimal` image contains the runtime. `default` adds Chromium,
Mousepad, and Thunar. The distribution-neutral `artifacts` stage contains only
the static binary and AT-SPI helper for copying into existing images. Package
selection stays at that image's packaging boundary. Additional applications
belong in derived images.

`vdesk serve --local` generates local credentials, binds the API to a random
loopback port, and atomically publishes a data-only descriptor in a private
current-user runtime directory. Ordinary CLI commands discover and validate
that descriptor. The optional workspace and download roots are absent unless
the caller explicitly supplies both, so capabilities reflect the real sandbox
rather than inventing path conventions.

Xvnc 1.14 or newer can expose DRI3 through a delegated DRM render node. The
managed `open --gpu` path delegates one render node and requires DRI3 before it
reports readiness. Embedded mode simply consumes devices visible in its outer
sandbox; the runtime contains no Podman, Docker, or mim logic. Xvfb and Xvnc
without a render node remain the software baseline.

Managed mode rejects NVIDIA because its standard CDI profile injects devices
beyond the render-node-only boundary. Embedded mode does not reinterpret the
outer sandbox's device policy, so an owner may choose NVIDIA CDI there without
adding provider logic to vdesk.

Only the outer lifecycle owner handles devices. Vdesk never asks for the host
display, a KMS/card node, or an engine socket. x11vnc remains the viewer adapter
because its input hook is the small, proven way to count human input; Xvnc's
otherwise unused native RFB listener is confined to a private Unix socket.

## Sessions

Private local descriptors index known sessions. Their containers and networks
carry matching ownership labels that are verified before reuse or removal, so
there is no host daemon or registry service and corrupt state cannot authorize
deleting an unrelated resource.

| Operation | Result |
| --- | --- |
| `open` | Reuse a healthy session or create one and wait until ready. |
| `stop` | Stop the container while preserving its writable state. |
| `open` after `stop` | Resume the same container. |
| `open` after external removal | Recreate from the descriptor and rotate credentials. |
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

Managed `vdesk view` constructs an authenticated noVNC URL served by the
desktop service. For an embedded runtime, `vdesk view -- EXEC ARGV...` runs that
trusted argv vector and bridges noVNC to the guest's fixed `vdesk rfb-stdio`
command. The command authenticates the local descriptor and connects only to
the fixed private RFB endpoint; it is not a generic proxy.

Both paths use the same embedded noVNC client and viewer bridge. Raw VNC is
never published. Human and agent input intentionally share the display. Human
events advance the input generation and can be detected without imposing a
takeover workflow.

## Containerized agents

`vdesk run` starts an agent on the session network and mounts two read-only
files: the static client and a descriptor containing only data-plane authority.
It does not mount the Podman or Docker socket, host display, session lifecycle
state, or workspace path.

An embedded agent needs even less translation: it runs the same CLI beside the
runtime and discovers the private local descriptor. The outer sandbox already
owns its user, files, network, and lifecycle. Host viewing uses only that outer
tool's existing interactive exec transport.

## Security boundary

The recommended deployment uses a rootless engine. Desktop and sibling agent
containers run without added capabilities, with `no-new-privileges` and resource
limits. The service requires per-session credentials, host ports bind to
loopback, and file and process operations are bounded.

These controls limit authority; they do not turn a shared-kernel container into
a hostile multi-tenant VM. Workloads that require a stronger kernel boundary
should run the runtime inside a VM. Embedded mode inherits its outer boundary
and requests no privilege, host display, engine socket, port publication, or
extra mount.

See [embedded.md](embedded.md) for the concrete embedded workflow.
