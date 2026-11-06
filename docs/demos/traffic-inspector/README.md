# Traffic inspector prototype

A standalone, dependency-free UI demo for reviewing AgentTomo's proposed HTTP,
SSE and WebSocket experience. All traffic and the terminal are simulated in the
browser. It does not connect to the collector, intercept traffic, run commands,
install a CA, or persist captured payloads.

Run from the repository root:

```bash
python3 -m http.server 18889 --bind 0.0.0.0 --directory docs/demos/traffic-inspector
```

Open `http://101.132.185.173:18889/` for this development server.

The prototype includes request filtering and host grouping, process-to-request
navigation, request/response headers and body inspection, simulated live SSE and
bidirectional WebSocket messages, and a byte-bounded sliding window. The small
1–8 KiB demo windows intentionally make eviction visible quickly; they are not
the proposed production defaults. Increasing the window does not restore evicted
messages. Pausing follow only affects the view; pausing simulation stops the
demo's message generator.

The real collector and Playground assets are unchanged by this prototype.

## File-open preview

Open `http://101.132.185.173:18889/#files` to start at the file preview. Each
process's `file.open` rows open an adjacent viewer with the path, PID, open
flags, result and sample content. Examples cover text with line numbers, JSON
formatting, binary hex/ASCII, a truncated certificate bundle, a failed open and
a successful write-only open without captured content. All previews use embedded
sample snapshots; the demo never reads the server paths shown in the interface.

The preview offers **copy path** and **copy content** (hex for binary). Content
copy uses the current formatted/raw preview without line numbers; a truncated
preview does not copy the rest of the file. Clipboard errors are shown and a
browser copy fallback supports plain HTTP. The former preview service on port
18889 was stopped at the user's request; this demo does not start a server itself.
