import { TraceView } from "./trace-view.mjs";

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
const trace = new TraceView({
  root: $("process-tree"), empty: $("empty-trace"), count: $("event-count"), summary: $("trace-summary"),
  onSelect(event) {
    $("selected-sequence").textContent = `#${event.sequence}`;
    $("event-json").textContent = JSON.stringify(event, null, 2);
    $("event-detail").open = true;
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
      terminal.focus();
      break;
    case "input_ack": sending = false; sendNext(); break;
    case "trace": trace.add(message.event); break;
    case "root_exit":
      stopInput();
      setStatus(`Shell exited (${message.code}) · collecting`, "starting");
      break;
    case "complete":
      stopInput();
      finalStatus = true;
      setStatus("Collection complete", "complete");
      break;
    case "error":
      stopInput();
      finalStatus = true;
      setStatus("Session error", "error");
      notice(message.message);
      break;
  }
};
socket.onerror = () => {
  stopInput();
  finalStatus = true;
  setStatus("Connection error", "error");
  notice("Cannot connect. Check the server and open its complete access URL.");
};
socket.onclose = () => {
  stopInput();
  if (!finalStatus) setStatus("Disconnected", "starting");
};
$("new-session").addEventListener("click", () => location.reload());
window.addEventListener("pagehide", () => socket.close());
