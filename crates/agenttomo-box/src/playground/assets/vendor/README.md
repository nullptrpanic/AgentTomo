# Browser and terminal dependencies

Unmodified npm distribution files from the official npm registry:

- `@xterm/xterm` 6.0.0, MIT; https://github.com/xtermjs/xterm.js
- `@xterm/addon-fit` 0.11.0, MIT; https://github.com/xtermjs/xterm.js/tree/master/addons/addon-fit

License texts are included beside the distributions. These pinned files are
embedded into the Rust binary so builds and page loads need no Node.js or CDN.
Update using `npm pack <package>@<version> --registry=https://registry.npmjs.org`
and copy the corresponding `lib/*.js`, `css/xterm.css`, and `LICENSE` files
without modifying third-party code.

## noVNC

`novnc.mjs` is the unmodified noVNC 1.7.0 implementation bundled as an ES
module using esbuild 0.28.2 (build tool only). noVNC is principally MPL-2.0;
see `novnc-LICENSE.txt` and `novnc-NOTICES.txt` for
individual files and bundled dependencies. Source code for this exact version:
https://registry.npmjs.org/@novnc/novnc/-/novnc-1.7.0.tgz
and https://github.com/novnc/noVNC/tree/v1.7.0 .

Rebuild from the published npm package with:

```sh
esbuild node_modules/@novnc/novnc/core/rfb.js --bundle --format=esm \
  --target=es2022 --legal-comments=linked --outfile=novnc.mjs
```

The application serves the bundle locally; browser sessions do not use a CDN.
