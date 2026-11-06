import { TraceView } from "./trace-view.mjs";
import { FilePreview } from "./file-preview.mjs";
import { TrafficView } from "./traffic-view.mjs";
import { BrowserView } from "./browser-view.mjs";

const $ = (id) => document.getElementById(id);
const terminal = new Terminal({
  cursorBlink: true,
  fontFamily: '"SFMono-Regular", Consolas, "Liberation Mono", monospace',
  fontSize: 13,
  lineHeight: 1.3,
  scrollback: 4000,
  theme: {
    background: "#101820", foreground: "#d3dee8", cursor: "#9be9c2",
    selectionBackground: "#456353", black: "#1a2531", red: "#ef9b8a",
    green: "#9be9c2", yellow: "#e7cb8d", blue: "#88b9ed", magenta: "#c4a7dc",
    cyan: "#8ddbd8", white: "#d3dee8", brightBlack: "#6d8299",
  },
});
const fit = new FitAddon.FitAddon();
terminal.loadAddon(fit);
terminal.open($("terminal"));

const url = new URL("/ws", location.href);
url.protocol = location.protocol === "https:" ? "wss:" : "ws:";
url.searchParams.set("token", new URLSearchParams(location.search).get("token") || "");
const socket = new WebSocket(url);
socket.binaryType = "arraybuffer";
let acceptingInput = false;
let finalStatus = false;
let sending = false;
let queuedBytes = 0;
const queuedInput = [];
const encoder = new TextEncoder();
let sessionId = null;
let browserEnabled = false;
const browser = new BrowserView($("browser-screen"), $("browser-status"), $("browser-reconnect"));
function workspaceView(name) {
  for (const button of document.querySelectorAll("[data-workspace]")) button.setAttribute("aria-pressed", String(button.dataset.workspace === name));
  const visible = name === "browser";
  $("terminal").hidden = visible;
  $("browser").hidden = !visible;
  $("terminal-title").textContent = visible ? "Container browser" : "Container terminal";
  $("workspace-hint").textContent = visible ? "Mouse and keyboard control Chrome" : "Ctrl-C to interrupt · Ctrl-D to exit";
  browser.show(visible);
  if (!visible) { resize(); terminal.focus(); }
}
for (const button of document.querySelectorAll("[data-workspace]")) button.addEventListener("click", () => workspaceView(button.dataset.workspace));
async function loadPayload(kind, preview, signal) {
  if (!Number.isSafeInteger(preview.offset) || preview.offset < 0 || !Number.isSafeInteger(preview.captured_bytes) || preview.captured_bytes < 0 || preview.captured_bytes > 1048576) throw new Error("Invalid snapshot reference");
  const target = new URL(`/api/sessions/${sessionId}/${kind}`, location.href);
  target.searchParams.set("token", url.searchParams.get("token"));
  target.searchParams.set("offset", preview.offset);
  target.searchParams.set("length", preview.captured_bytes);
  const response = await fetch(target, { signal, cache: "no-store" });
  if (!response.ok) throw new Error(`Saved content is unavailable (HTTP ${response.status})`);
  const bytes = new Uint8Array(await response.arrayBuffer());
  if (bytes.length !== preview.captured_bytes) throw new Error("Snapshot length does not match the event");
  return bytes;
}
const files = new FilePreview($("file-preview"), (event, signal) => loadPayload("file-preview", event.data.preview, signal));
async function loadStream(requestId, messageId, signal) {
  const target = new URL(`/api/sessions/${sessionId}/stream-messages`, location.href);
  target.searchParams.set("token", url.searchParams.get("token"));
  target.searchParams.set("request_id", requestId);
  if (messageId !== null) target.searchParams.set("message_id", messageId);
  const response = await fetch(target, { signal, cache: "no-store" });
  if (!response.ok) { const error = new Error(`HTTP ${response.status}`); error.status = response.status; throw error; }
  return messageId === null ? response.json() : new Uint8Array(await response.arrayBuffer());
}
const traffic = new TrafficView($("traffic"), (body, signal) => loadPayload("http-body", body, signal), selectRaw, loadStream);

function selectRaw(event) {
  $("selected-sequence").textContent = event.sequence === undefined ? `message #${event.data.id}` : `#${event.sequence}`;
  $("event-json").textContent = JSON.stringify(event, null, 2);
}
function observationView(name) {
  for (const button of document.querySelectorAll("[data-observation]")) button.setAttribute("aria-pressed", String(button.dataset.observation === name));
  $("traffic").hidden = name !== "traffic";
  $("trace-scroll").hidden = name !== "process";
  document.querySelector(".trace-tools").hidden = name !== "process";
  document.querySelector(".trace-description").hidden = name !== "process";
  if (name === "traffic") files.close();
}
for (const button of document.querySelectorAll("[data-observation]")) button.addEventListener("click", () => observationView(button.dataset.observation));
const trace = new TraceView({
  root: $("process-tree"), scroll: $("trace-scroll"), empty: $("empty-trace"), count: $("event-count"), summary: $("trace-summary"), filters: $("event-filter"),
  onSelect(event) {
    selectRaw(event);
    if (event.kind === "file.open") {
      $("event-detail").open = false;
      files.show(event);
    } else if (event.kind.startsWith("network.http.") || event.kind.startsWith("network.stream.")) {
      observationView("traffic");
      traffic.select(event);
    } else {
      files.close();
      $("event-detail").open = true;
    }
  },
});

function setStatus(label, state) {
  $("status").textContent = label;
  $("status").dataset.state = state;
}

function notice(message) {
  $("notice").hidden = false;
  $("notice").textContent = message;
}

function stopInput() {
  acceptingInput = false;
  queuedInput.length = 0;
  queuedBytes = 0;
  sending = false;
  terminal.options.disableStdin = true;
}

function sendNext() {
  if (sending || !acceptingInput || socket.readyState !== WebSocket.OPEN || !queuedInput.length) return;
  const chunk = queuedInput.shift();
  queuedBytes -= chunk.length;
  sending = true;
  socket.send(chunk);
}

function input(bytes) {
  if (!acceptingInput) return;
  if (queuedBytes + bytes.length > 1024 * 1024) {
    notice("Input queue is full. Wait for the command to read input before pasting again.");
    return;
  }
  for (let offset = 0; offset < bytes.length; offset += 8192) {
    const chunk = bytes.slice(offset, offset + 8192);
    queuedInput.push(chunk);
    queuedBytes += chunk.length;
  }
  sendNext();
}

terminal.onData((data) => input(encoder.encode(data)));
terminal.onBinary((data) => input(Uint8Array.from(data, (char) => char.charCodeAt(0) & 255)));

let resizeFrame;
function resize() {
  cancelAnimationFrame(resizeFrame);
  resizeFrame = requestAnimationFrame(() => {
    if ($("terminal").hidden) return;
    fit.fit();
    if (acceptingInput && socket.readyState === WebSocket.OPEN) {
      socket.send(JSON.stringify({ type: "resize", rows: terminal.rows, cols: terminal.cols }));
    }
  });
}
new ResizeObserver(resize).observe($("terminal"));
document.fonts.ready.then(resize);

socket.onmessage = ({ data }) => {
  if (data instanceof ArrayBuffer) {
    terminal.write(new Uint8Array(data));
    return;
  }
  const message = JSON.parse(data);
  switch (message.type) {
    case "session":
      sessionId = message.id;
      browserEnabled = Boolean(message.browser);
      $("browser-tab").hidden = !browserEnabled;
      if (browserEnabled) workspaceView("browser");
      traffic.setEnabled(Boolean(message.http_capture));
      $("capture-description").textContent = message.http_capture ? "Process lifecycle · files · TCP / UDP · HTTP/HTTPS content available in Traffic." : "Process lifecycle · files · TCP / UDP · HTTP Host / TLS SNI. HTTPS stays encrypted.";
      $("image").textContent = message.image;
      $("session-id").textContent = `session ${message.id.slice(0, 8)}`;
      setStatus("Starting container", "starting");
      break;
    case "ready":
      trace.start(message.pid);
      acceptingInput = true;
      terminal.options.disableStdin = false;
      setStatus("Connected", "live");
      resize();
      if (browserEnabled) browser.start(sessionId, url.searchParams.get("token"));
      if (!$("terminal").hidden) terminal.focus();
      break;
    case "input_ack": sending = false; sendNext(); break;
    case "trace": trace.add(message.event); traffic.add(message.event); break;
    case "root_exit":
      stopInput();
      setStatus(`Shell exited (${message.code}) · collecting`, "starting");
      break;
    case "complete":
      stopInput();
      browser.stop();
      finalStatus = true;
      setStatus("Collection complete", "complete");
      break;
    case "error":
      stopInput();
      browser.stop();
      finalStatus = true;
      setStatus("Session error", "error");
      notice(message.message);
      break;
  }
};
socket.onerror = () => {
  stopInput();
  browser.stop();
  finalStatus = true;
  setStatus("Connection error", "error");
  notice("Cannot connect. Check the server and open its complete access URL.");
};
socket.onclose = () => {
  stopInput();
  browser.stop();
  if (!finalStatus) setStatus("Disconnected", "starting");
};
$("new-session").addEventListener("click", () => location.reload());
window.addEventListener("pagehide", () => { browser.stop(); socket.close(); });
