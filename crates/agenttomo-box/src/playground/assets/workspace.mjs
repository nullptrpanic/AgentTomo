import { BrowserView } from "./browser-view.mjs";
import { resizePanels } from "./panel-resize.mjs";

// Only this entry point may create and own a Docker session.
export function startWorkspace({ token, onMessage, notice, onView }) {
  const $ = (id) => document.getElementById(id);
  resizePanels(document.querySelector("main"), $("panel-resizer"));
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
  url.searchParams.set("token", token);
  const socket = new WebSocket(url);
  socket.binaryType = "arraybuffer";
  let acceptingInput = false;
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
    const chat = name === "chat";
    const samples = name === "samples";
    $("terminal").hidden = visible || chat || samples;
    $("security-samples").hidden = !samples;
    $("browser").hidden = !visible;
    $("agent-chat").hidden = !chat;
    $("terminal-title").textContent = samples ? "Security tests" : chat ? "Agent conversation" : visible ? "Container browser" : "Container terminal";
    $("workspace-hint").textContent = samples ? "Real Agent · Disposable sample files" : chat ? "Enter to send · Shift-Enter for a new line" : visible ? "Mouse and keyboard control Chrome" : "Ctrl-C to interrupt · Ctrl-D to exit";
    browser.show(visible);
    if (!visible && !chat && !samples) { resize(); terminal.focus(); }
    onView?.(name);
  }
  for (const button of document.querySelectorAll("[data-workspace]")) button.addEventListener("click", () => workspaceView(button.dataset.workspace));
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
    onMessage(message);
    switch (message.type) {
      case "session":
        sessionId = message.id;
        browserEnabled = Boolean(message.browser);
        $("browser-tab").hidden = !browserEnabled;
        if (browserEnabled) workspaceView("browser");
        $("image").textContent = message.image;
        $("session-id").textContent = `session ${message.id.slice(0, 8)}`;
        break;
      case "ready":
        acceptingInput = true;
        terminal.options.disableStdin = false;
        resize();
        if (browserEnabled) browser.start(sessionId, url.searchParams.get("token"));
        if (!$("terminal").hidden) terminal.focus();
        break;
      case "input_ack": sending = false; sendNext(); break;
      case "root_exit":
        stopInput();
        break;
      case "complete":
        stopInput();
        browser.stop();
        break;
      case "error":
        stopInput();
        browser.stop();
        break;
    }
  };
  socket.onerror = () => {
    stopInput();
    browser.stop();
    onMessage({ type: "error", message: "Cannot connect. Check the server and open its complete access URL." });
  };
  socket.onclose = () => {
    stopInput();
    browser.stop();
    onMessage({ type: "disconnected" });
  };
  window.addEventListener("pagehide", () => { browser.stop(); socket.close(); });
  return {
    prompt(task_id, prompt) {
      if (!acceptingInput || socket.readyState !== WebSocket.OPEN) throw new Error("Sandbox is not connected");
      socket.send(JSON.stringify({ type:"agent_prompt", task_id, prompt }));
    },
    sample(task_id, sample_id) {
      if (!acceptingInput || socket.readyState !== WebSocket.OPEN) throw new Error("Sandbox is not connected");
      socket.send(JSON.stringify({type:"sample_run",task_id,sample_id}));
    },
    cancel() { if (socket.readyState === WebSocket.OPEN) socket.send(JSON.stringify({ type:"agent_cancel" })); },
  };
}
