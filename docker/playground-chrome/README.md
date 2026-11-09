# Chrome Playground

This is the default Playground image. It provides an interactive Chrome window
alongside the terminal and process/traffic inspector. Select another local image
with `--image`.

Build from the repository root (local base image required):

```sh
docker build --pull=false -t agenttomo-playground:24.04 docker/playground
docker build --pull=false -t agenttomo-playground-chrome:24.04 docker/playground-chrome
cargo build -p agenttomo-box
```

The Chrome image installs the official Google Chrome stable amd64 Debian package,
Xvfb, x11vnc, Openbox, xauth, X11 utilities, NSS certificate tools, and CJK fonts.
It requires network access while building, not while starting a session. Chrome
is proprietary software; its package is downloaded from Google during the build.
The host Playground service can run as a non-root user. Container terminals run
as root, while the Chrome wrapper drops to nobody (UID/GID 65534) and retains
Chrome's normal sandbox. Rebuild this image when updating its helper scripts.

Start the Playground:

```sh
./target/debug/agenttomo-box playground \
  --listen 0.0.0.0:18889 \
  --capture-http
```

Use the printed token with `http://101.132.185.173:18889/`. Each page opens its own
container. The Browser tab shows Chrome; Terminal remains available and the
process/traffic inspector stays on the right. Chrome starts automatically in the
traced process tree. To reopen a closed window, run `google-chrome` in Terminal.
`Reconnect display` reconnects to the same desktop. Select
`--image agenttomo-playground:24.04` for a terminal session. `--capture-http`
enables HTTP content capture and HTTPS decryption.

Browser mode is detected from executable `agenttomo-browser-session`,
`agenttomo-desktop` and `agenttomo-chrome` helpers in `/usr/local/bin`.
Detection works with custom image names and the existing Chrome image.
All three absent selects terminal mode. A partial installation or failed Docker
probe produces an error. Detection uses a short-lived, read-only container with
networking disabled, then removes it. Sessions use the same inspected image ID.
Remove `--browser` from older startup commands.

## Ownership and transport

- The traced startup script launches Chrome as a child and then becomes Bash.
  Bash uses UID/GID 0 and `/root`; Chrome uses UID/GID 65534 and `HOME=/tmp`.
  The wrapper uses `setpriv` to drop the browser's root identity, including when
  `google-chrome` is invoked again from the terminal. No `--no-sandbox` is added.
  Chrome's descendants inherit the existing tracing and proxy configuration.
  Network requests are attributed to the actual network service PID; this is
  process attribution, not a mapping to browser tabs or individual renderers.
- Xvfb, Openbox and x11vnc run in the same container outside the traced tree.
  They share Chrome's unprivileged identity; the ready directory allows traversal
  by the root terminal, while the X11 cookie and logs remain private.
  X11 uses a cookie-authenticated filesystem socket and has TCP disabled.
  Only the display helpers have their FD limit capped at 4096: x11vnc otherwise
  scans enormous Docker FD limits during startup. The traced workload's limit
  is unaffected. Browser sessions use 256 MiB of shared memory.
- noVNC is bundled with the application. The server checks its existing token,
  Origin and active session before bridging WebSocket bytes through Docker exec
  to VNC on the container's loopback address. No VNC port is published. Buffers
  are bounded and use backpressure; display reconnect closes its previous relay.
- Closing the page, creating a new session, or stopping the server cancels
  bridges and removes the owned container. Shell exit alone still permits Chrome
  and other surviving descendants to remain traced, as with ordinary sessions.

The requested HTTP access mode was tested with real mouse/keyboard input and
display reconnection. noVNC 1.7 still logs its upstream warning about a non-secure
browser context on HTTP. This integration uses the basic RFB transport; features
requiring WebCrypto or a secure browser context are not enabled. The existing
HTTP token/terminal transport is not changed into an encrypted web service by
enabling the Browser tab.

## Chrome trust and protocol scope

With `--capture-http`, the existing OverlayFS supplies the task public CA at
`/etc/ssl/certs/agenttomo-task-ca.pem`. The Chrome wrapper imports it with
`certutil` into the disposable container user's NSS database before Chrome starts.
Modern Chrome uses `$HOME/.local/share/pki/nssdb`; an existing legacy
`$HOME/.pki/nssdb` takes precedence. The NSS database and browser profile remain
inside the temporary container, and are removed with it. Host trust is unchanged;
the signing private key remains only in collector memory.

The wrapper disables QUIC so requests use the current TCP/HTTP inspector. It
keeps Chrome's sandbox and certificate verification enabled. The upstream TLS
peer is still independently verified with the collector's original trust store.
Each session generates a fresh `AgentTomo Root CA` with ten-year validity.
Domain certificates last one day and use a run-local wildcard cache with a
one-hour TTL. Public trust arrives through the memory-only startup channel and
the target's temporary OverlayFS. HTTP/2-only services, pinned certificates, client certificates,
and QUIC remain unsupported. SSE events and WebSocket messages now have a live
Messages tab with sliding retention, including gzip/deflate SSE and
permessage-deflate WebSockets. Ordinary compressed HTTP bodies retain their
existing encoded representation. See [content inspection](../../docs/content-inspection-design.md)
for the separate stream limits and decoding boundaries.

When the outer network has no non-loopback unicast IPv6 routes, the diagnostic
namespace no longer invents a default IPv6 route. This preserves immediate
unreachable errors and lets Chrome use IPv4. IPv6 loopback remains available;
outer networks with IPv6 routes retain the existing IPv6 proxy path. This is a
startup availability check, not dynamic route synchronization or a guarantee
that every destination is reachable.

Chrome diagnostics are in `/tmp/agenttomo-chrome.log`, and display helper logs
in `/tmp/agenttomo-desktop/`, accessible from the terminal while the session lives.

## Verification

Run the regular checks from the repository root:

```sh
cargo test -p agenttomo-box
node --test crates/agenttomo-box/tests/*.test.mjs
cargo test -p agenttomo-box --test playground -- --ignored
```

The last command needs Docker access and both Playground images. It covers
ordinary terminal sessions, browser authentication and session ownership,
display reconnection, missing image helpers, and container cleanup.

`crates/agenttomo-box/tests/browser-smoke.mjs` is an additional opt-in GUI check.
It needs Playwright with Chromium and its system libraries, the built binary,
Docker access, and Internet access. Run it as the same non-root user that runs
the Playground:

```sh
node crates/agenttomo-box/tests/browser-smoke.mjs
```

When Playwright is installed outside the repository, set
`AGENTTOMO_PLAYWRIGHT_MODULE` to its absolute `index.mjs` path. Optionally set
`AGENTTOMO_TEST_BROWSER` to a Chromium executable and `AGENTTOMO_TEST_ARTIFACTS`
to a screenshot directory. The test opens a local HTTP fixture and
`https://www.baidu.com/` (override with `AGENTTOMO_HTTPS_TEST_URL`, requiring a
successful HTTP 200 response). It exercises remote keyboard input, Chrome PID
attribution, captured bodies, NSS trust without certificate bypass, display
reconnection, tab switching, and narrow layouts. Its server uses an ephemeral
loopback port and is stopped after the test.

`tests/stream-browser-smoke.mjs` in the same crate adds actual Chrome EventSource
and compressed WebSocket checks using a local fixture. It also needs the `ws`
npm package (set `AGENTTOMO_WS_MODULE` to an external `ws/wrapper.mjs` if needed).
It exercises both directions, live data before EOF, sliding-window expiry,
scroll pause/resume, copying, raw details and responsive layout. It uses the
same optional browser/artifact environment variables and cleans up its session.

References: [noVNC API](https://novnc.com/noVNC/docs/API.html),
[Chromium Linux certificate management](https://chromium.googlesource.com/chromium/src/+/HEAD/docs/linux/cert_management.md).
