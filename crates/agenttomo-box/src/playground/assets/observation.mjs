import { TraceView } from "./trace-view.mjs";
import { FilePreview } from "./file-preview.mjs";
import { TrafficView } from "./traffic-view.mjs";

// Shared by the workspace page and independent read-only monitor pages.
export function createObservation(token) {
  const $ = id => document.getElementById(id);
  let sessionId = null;
  async function loadPayload(kind, preview, signal, encoding = "") {
    if (!Number.isSafeInteger(preview.offset) || preview.offset < 0 || !Number.isSafeInteger(preview.captured_bytes) || preview.captured_bytes < 0 || preview.captured_bytes > 1048576) throw new Error("Invalid snapshot reference");
    const target = new URL(`/api/sessions/${sessionId}/${kind}`, location.href);
    target.searchParams.set("token", token);
    target.searchParams.set("offset", preview.offset);
    target.searchParams.set("length", preview.captured_bytes);
    if (encoding) target.searchParams.set("decode", encoding);
    const response = await fetch(target, { signal, cache: "no-store" });
    if (!response.ok) throw new Error(`Saved content is unavailable (HTTP ${response.status})`);
    const bytes = new Uint8Array(await response.arrayBuffer());
    const decoding = response.headers.get("x-agenttomo-decoding") || "identity";
    const decoded = ["complete", "incomplete", "output_limit"].includes(decoding);
    if (bytes.length > 1048576 || !decoded && bytes.length !== preview.captured_bytes) throw new Error("Snapshot length does not match the event");
    return { bytes, decoding };
  }
  const files = new FilePreview($("file-preview"), async (event, signal) => (await loadPayload("file-preview", event.data.preview, signal)).bytes);
  async function loadStream(requestId, messageId, signal) {
    const target = new URL(`/api/sessions/${sessionId}/stream-messages`, location.href);
    target.searchParams.set("token", token);
    target.searchParams.set("request_id", requestId);
    if (messageId !== null) target.searchParams.set("message_id", messageId);
    const response = await fetch(target, { signal, cache: "no-store" });
    if (!response.ok) { const error = new Error(`HTTP ${response.status}`); error.status = response.status; throw error; }
    return messageId === null ? response.json() : new Uint8Array(await response.arrayBuffer());
  }
  const traffic = new TrafficView($("traffic"), (body, signal, encoding) => loadPayload("http-body", body, signal, encoding), selectRaw, loadStream);

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
      } else if (event.kind.startsWith("network.")) {
        observationView("traffic");
        traffic.select(event);
        $("event-detail").open = traffic.selected === null;
      } else {
        files.close();
        $("event-detail").open = true;
      }
    },
  });

  return {
    session(message) {
      sessionId = message.id;
      traffic.setEnabled(Boolean(message.http_capture));
      $("capture-description").textContent = message.http_capture ? "Process lifecycle · files · TCP / UDP · HTTP/HTTPS content available in Traffic." : "Process lifecycle · files · TCP / UDP · HTTP Host / TLS SNI. HTTPS stays encrypted.";
    },
    start(pid) { trace.start(pid); },
    add(event) { trace.add(event); traffic.add(event); },
  };
}
