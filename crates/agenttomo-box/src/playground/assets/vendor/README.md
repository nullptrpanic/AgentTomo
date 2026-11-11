# Browser, terminal and Markdown dependencies

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

## Markdown

Completed assistant messages use the unmodified npm ES modules:

- `marked` 18.2.0, MIT; `lib/marked.esm.js` is vendored as `marked.mjs`.
  Source: https://registry.npmjs.org/marked/-/marked-18.2.0.tgz
- `dompurify` 3.4.17, Apache-2.0 OR MPL-2.0; `dist/purify.es.mjs` is
  vendored as `dompurify.mjs`. Both license texts are included alongside it.
  Source: https://registry.npmjs.org/dompurify/-/dompurify-3.4.17.tgz

Refresh with `npm pack` as above and copy those files and licenses unchanged.
Both modules are embedded and served locally. `agent-markdown.mjs` sanitizes
parsed output to passive formatting and links; it does not allow scripts,
forms, embedded media, or model-controlled styles.
