# Terminal dependencies

Unmodified npm distribution files from the official npm registry:

- `@xterm/xterm` 6.0.0, MIT; https://github.com/xtermjs/xterm.js
- `@xterm/addon-fit` 0.11.0, MIT; https://github.com/xtermjs/xterm.js/tree/master/addons/addon-fit

License texts are included beside the distributions. These pinned files are
embedded into the Rust binary so builds and page loads need no Node.js or CDN.
Update using `npm pack <package>@<version> --registry=https://registry.npmjs.org`
and copy the corresponding `lib/*.js`, `css/xterm.css`, and `LICENSE` files
without modifying third-party code.
