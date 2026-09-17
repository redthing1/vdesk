---
name: vdesk
description: Operate a vdesk graphical Linux desktop with exact screenshots, structured input, files, processes, and optional human help. Use for GUI applications exposed through vdesk; prefer a semantic application tool when one exists.
---

# Use vdesk

Treat the desktop as persistent external state. Begin with:

```sh
vdesk capabilities
vdesk see --output /tmp/vdesk.png --include-cursor
```

Inspect the original PNG. Its integer coordinates start at `(0,0)` in the
top-left; never derive coordinates from a resized preview.

Use `click`, `move`, `drag`, `scroll`, `type`, `key`, or `wait`, then observe
again. Prefer short coherent batches. When retrying a batch, reuse the same
versioned JSON and `request_id`. After a stale-input error or
`interference_detected`, observe before acting again.

Use structured context when helpful:

- `vdesk windows` and `vdesk a11y` supplement vision.
- `vdesk launch PROGRAM ARGS...` starts a GUI application.
- `vdesk process` runs explicit argv and exposes status and bounded output; it
  does not imply a shell.
- `vdesk import LOCAL workspace/RELATIVE` and `vdesk export` move bytes between
  client and desktop. Treat downloads as untrusted.

Unicode typing uses a verified clipboard paste and leaves the text on the
desktop clipboard. ASCII typing is direct.

A person can use `vdesk view` on a managed desktop without a handoff step. For
an embedded desktop, the host passes the environment's ordinary exec vector,
for example `vdesk view -- mim exec -i work -- vdesk rfb-stdio`.

From a sibling agent container, use the mounted client and descriptor. Inside
an embedded sandbox, run the same CLI normally; it discovers the local runtime.
Check `capabilities` before using files or managed processes because embedded
runtimes expose them only when scoped roots were configured. Do not seek an
engine socket, raw VNC, host display, or a new shared host path.

Action delivery is not task success. Verify the resulting pixels, structured
state, process result, or exported file.
