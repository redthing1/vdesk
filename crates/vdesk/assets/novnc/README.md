# noVNC client

`rfb.js` is the minified browser bundle of noVNC 1.7.0's `core/rfb.js`.
It is served only by vdesk's fixed RFB bridge. The surrounding HTML and
JavaScript are vdesk code.

Source: <https://github.com/novnc/noVNC/tree/v1.7.0>

Bundle SHA-256: `8a7ecc725c07063e4a613c5aab2da92548efd57bede31e07e56bc59e4e73f353`

Rebuild from an unpacked noVNC 1.7.0 tree:

```sh
bun build core/rfb.js --outfile rfb.js --minify \
  --banner='/* noVNC 1.7.0 | MPL-2.0 and compatible licenses | source and notices: README.md */'
```

The noVNC core is MPL-2.0. Incorporated portions retain their compatible
licenses; the applicable texts and upstream author list are included here.
