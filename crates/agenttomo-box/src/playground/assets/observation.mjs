import { TraceView } from "./trace-view.mjs";
import { FilePreview } from "./file-preview.mjs";
import { TrafficView } from "./traffic-view.mjs";
import { ProcessTree } from "./process-tree.mjs";
import { EventFilter, FilterMenu } from "./event-filter.mjs";
import { activitySnapshot } from "./activity-model.mjs";
import { BehaviorGraph } from "./behavior-graph.mjs";
import { AgentView } from "./agent-view.mjs";

// Shared by the workspace page and independent read-only monitor pages.
export function createObservation(token) {
  const $ = id => document.getElementById(id);
  let sessionId = null, activeView = "process", scheduled = false;
  const tree = new ProcessTree(), filter = new EventFilter();
  const filters = new FilterMenu($("event-filter"), filter, schedule);
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
  async function loadAgent(preview, signal) {
    if (!Number.isSafeInteger(preview.offset) || preview.offset < 0 || !Number.isSafeInteger(preview.captured_bytes) || preview.captured_bytes < 0 || preview.captured_bytes > 33554432) throw new Error("Invalid semantic payload reference");
    const target = new URL(`/api/sessions/${sessionId}/agent-payload`, location.href);
    target.searchParams.set("token", token); target.searchParams.set("offset", preview.offset); target.searchParams.set("length", preview.captured_bytes);
    const response = await fetch(target, { signal, cache: "no-store" });
    if (!response.ok) throw new Error(`Complete content unavailable (HTTP ${response.status})`);
    return response.json();
  }
  const agent = new AgentView({ root:$("agent-trajectory"), chat:$("agent-chat"), detail:$("agent-detail"), load:loadAgent, onSelect:selectEvent,
    onRaw:event => { files.close(); selectRaw(event); $("event-detail").open = false; },
    reveal:() => observationView("agent"), filter });

  function selectRaw(event) {
    $("selected-sequence").textContent = event.sequence === undefined ? `message #${event.data.id}` : `#${event.sequence}`;
    $("event-json").textContent = JSON.stringify(event, null, 2);
  }
  function observationView(name) {
    activeView = name;
    for (const button of document.querySelectorAll("[data-observation]")) button.setAttribute("aria-pressed", String(button.dataset.observation === name));
    $("traffic").hidden = name !== "traffic";
    $("trace-scroll").hidden = name !== "process";
    $("graph-scroll").hidden = name !== "graph";
    $("agent-trajectory").hidden = name !== "agent";
    document.querySelector(".trace-tools").hidden = name === "traffic";
    document.querySelector(".trace-description").hidden = name === "traffic";
    $("view-description").textContent = name === "agent" ? "Task → model calls → tools → processes → resource activity. Dashed edges are inferred." : name === "graph" ? "Expand a process to reveal event cards and child processes." : "Expand a process to inspect its behavior and descendants.";
    if (name !== "agent") agent.closeDetail();
    if (name === "traffic") files.close();
    schedule();
  }
  for (const button of document.querySelectorAll("[data-observation]")) button.addEventListener("click", () => observationView(button.dataset.observation));
  function selectEvent(event, connectionId = null) {
    selectRaw(event);
    if (event.kind === "file.open") {
      agent.closeDetail();
      $("event-detail").open = false;
      files.show(event);
    } else if (event.kind.startsWith("network.")) {
      observationView("traffic");
      traffic.select(connectionId === null ? event : { data: { connection_id: connectionId } });
      $("event-detail").open = traffic.selected === null;
    } else {
      files.close();
      $("event-detail").open = true;
    }
  }
  function inspectEvidence(event) { selectRaw(event); files.close(); $("event-detail").open = true; }
  const trace = new TraceView({
    root: $("process-tree"), scroll: $("trace-scroll"), empty: $("empty-trace"), onSelect: selectEvent, onInspect: inspectEvidence,
  });
  const graph = new BehaviorGraph({ root: $("behavior-graph"), scroll: $("graph-scroll"), empty: $("empty-graph"), onSelect: selectEvent,
    onInspect: inspectEvidence,
  });

  function schedule() {
    if (scheduled) return;
    scheduled = true;
    requestAnimationFrame(() => {
      scheduled = false;
      const snapshot = activitySnapshot(tree, filter);
      filters.update();
      if (activeView === "process") trace.render(snapshot);
      if (activeView === "graph") graph.render(snapshot);
      if (activeView === "agent") {
        const summary = agent.render();
        $("event-count").textContent = (summary.tools + summary.resources).toLocaleString();
        $("trace-summary").textContent = `${summary.tasks} tasks · ${summary.tools} tools · ${summary.processes} processes · ${summary.resources} resource cards`;
        return;
      }
      $("event-count").textContent = snapshot.behaviorCount.toLocaleString();
      $("trace-summary").textContent = `${snapshot.nodes.size} process${snapshot.nodes.size === 1 ? "" : "es"} · ${snapshot.behaviorCount}/${snapshot.totalBehaviorCount} behaviors${tree.total > tree.limit ? " · recent history" : ""}`;
    });
  }

  return {
    session(message) {
      sessionId = message.id;
      agent.configure(message);
      traffic.setEnabled(Boolean(message.http_capture));
      $("capture-description").textContent = message.http_capture ? "Process lifecycle · files · TCP / UDP · HTTP/HTTPS content available in Traffic." : "Process lifecycle · files · TCP / UDP · HTTP Host / TLS SNI. HTTPS stays encrypted.";
    },
    start(pid) { tree.start(pid); agent.setReady(true); schedule(); },
    stop(reason) { agent.setReady(false, reason); },
    control(control) { agent.setControl(control); },
    view:observationView,
    add(event) { agent.add(event); if (!event.kind.startsWith("agent.")) { filter.observe(event.kind); tree.add(event); traffic.add(event); } schedule(); },
  };
}
