# Embedded runtime

Embedded mode runs vdesk inside an existing container or VM. The outer tool
owns that environment; vdesk owns only its desktop processes and local API.

```text
host viewer -- existing exec/SSH --> sandbox --> private RFB
                                      |
                                local vdesk CLI
```

There is no nested engine, second desktop container, published port, host
display, shared descriptor mount, or provider-specific code.

## Install

Vdesk installs as one static binary plus the sandbox's native desktop packages.
For Fedora:

```sh
dnf install -y --setopt=install_weak_deps=False \
  at-spi2-core dbus-daemon dbus-x11 dejavu-sans-fonts liberation-fonts \
  mesa-dri-drivers python3-pyatspi tigervnc-x11-server x11vnc xclip xdotool \
  xfce4-panel xfce4-session xfce4-settings xfce4-terminal \
  xfconf xfdesktop xfwm4
install -m 0755 vdesk /usr/local/bin/vdesk
```

These commands work while building an image or inside an existing mutable
mimchine. They do not change its user, home, workdir, entrypoint, or lifecycle.

To build the static binary from this repository without installing Rust:

```sh
podman build -f container/Containerfile --target artifacts \
  --output type=local,dest=dist .
```

The result is `dist/usr/local/bin/vdesk`. To bake it into an image:

```Dockerfile
FROM your-existing-image

# Install the distribution's desktop packages here.
COPY dist/ /
```

## Start and use

Run the supervisor as the sandbox's ordinary user:

```sh
vdesk serve --local
```

It remains in the foreground, starts and reaps the desktop processes, and
publishes a private current-user descriptor. Other processes in the same
sandbox discover it automatically:

```sh
vdesk capabilities
vdesk see --output /tmp/desktop.png
vdesk launch xfce4-terminal
```

An existing service manager or entrypoint can own the process. For mimchine,
the recommended convenience is an executable `/usr/local/bin/mimchine-start`:

```sh
#!/bin/sh
exec vdesk serve --local
```

That hook is optional. It uses mim's ordinary lifecycle contract; vdesk has no
mim dependency and can also be started by `mim exec`, another init system, or
the image entrypoint.

## GPU access

The outer sandbox owns GPU policy. Delegate a render device there and start the
same runtime normally:

```sh
mim create work --image IMAGE --gpu
mim exec work -- vdesk capabilities
```

Vdesk uses a visible render node automatically and reports
`hardware_acceleration: true` when X11 exposes DRI3. No visible render node
means ordinary software rendering. The image must supply the matching Mesa or
NVIDIA userspace driver. An outer owner may use NVIDIA CDI if its broader device
scope is acceptable; vdesk neither requests nor interprets that policy. It does
not add a second container.

File and managed-process APIs are disabled unless both roots are explicit:

```sh
vdesk serve --local \
  --workspace-root /work/project \
  --downloads-root /work/downloads
```

`vdesk capabilities` reports what is available. Screenshots, input, windows,
clipboard, launch, accessibility, and viewing do not require those roots.

## Human view

Pass any trusted, stdin-preserving execution vector after `--`:

```sh
vdesk view -- mim exec -i work -- vdesk rfb-stdio
vdesk view -- podman exec -i work vdesk rfb-stdio
vdesk view -- ssh work.example vdesk rfb-stdio
```

Arguments are executed directly, never through a shell. The guest command can
reach only its authenticated local runtime and fixed loopback RFB endpoint.
The host serves its built-in noVNC client on a random loopback port.

Embedded vdesk inherits the isolation of its outer container or VM. It requests
no privilege, engine socket, host network, host display, or extra mount, and it
works in a networkless sandbox through the existing exec transport.
