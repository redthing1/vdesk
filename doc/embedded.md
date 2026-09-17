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

## Image integration

Build the distribution-neutral artifact image:

```sh
podman build -f container/Containerfile --target artifacts \
  -t localhost/vdesk:artifacts .
```

Copy it into the existing image without changing that image's lifecycle:

```Dockerfile
FROM localhost/vdesk:artifacts AS vdesk
FROM your-existing-image

# Install this distribution's Xvfb, Xfce, x11vnc, xdotool, xclip, D-Bus,
# AT-SPI/Python bindings, and basic fonts here.
COPY --from=vdesk / /
```

Only package names are distribution-specific. The runtime requires the
executables `Xvfb`, `xfce4-session`, `x11vnc`, `xdotool`, `xclip`, and
`dbus-daemon`; the managed Containerfile is a tested Debian example. Fedora's
equivalents work with the same artifacts and runtime command.

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
