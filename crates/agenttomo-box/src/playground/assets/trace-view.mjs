import { eventTime } from "./event-time.mjs";
import { EventEvidence } from "./event-evidence.mjs";
import { formatCommand } from "./event-description.mjs";

function element(tag, className, text) {
  const node = document.createElement(tag);
  node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function setText(node, text) {
  if (node.textContent !== text) node.textContent = text;
}

function timestamp(event, ns = eventTime(event)) {
  const time = element("time", "event-time", "—");
  if (!Number.isFinite(ns)) return time;
  const date = new Date(ns / 1e6);
  if (!Number.isFinite(date.getTime())) return time;
  const pad = (value, length = 2) => String(value).padStart(length, "0");
  time.dateTime = date.toISOString();
  time.title = date.toISOString() + " (UTC); displayed in your local time zone";
  if (Number.isFinite(event.data?.started_unix_ns) && event.data.started_unix_ns >= 0) {
    time.title += `\nStart: ${event.data.start_source || "operation"}`;
    for (const [label, value] of [["Completed", event.data.completed_unix_ns], ["Recorded", event.recorded_unix_ns ?? event.observed_unix_ns]]) {
      const stamp = new Date(value / 1e6);
      if (Number.isFinite(value) && Number.isFinite(stamp.getTime())) time.title += `\n${label}: ${stamp.toISOString()}`;
    }
  }
  time.textContent = `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}\n${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}.${pad(date.getMilliseconds(), 3)}`;
  return time;
}

function duration(node) {
  if (node.state === "running") return "Running";
  if (node.startedNs === null || node.endedNs === null || node.endedNs < node.startedNs) return "—";
  const ms = (node.endedNs - node.startedNs) / 1e6;
  return ms < 1000 ? `${ms.toFixed(1)} ms` : `${(ms / 1000).toFixed(3)} s`;
}

export class TraceView {
  constructor({ root, scroll, empty, onSelect, onInspect }) {
    this.root = root;
    this.scroll = scroll;
    this.empty = empty;
    this.onSelect = onSelect;
    this.onInspect = onInspect;
    this.nodes = new Map();
    this.events = new Map();
    this.selected = null;
  }

  createNode(node) {
    const item = element("li", "process-item");
    item.dataset.nodeId = node.id;
    const details = element("details", "process-node");
    details.open = false;
    const summary = element("summary", "process-heading");
    const chevron = element("span", "process-chevron", "›");
    chevron.setAttribute("aria-hidden", "true");
    const identity = element("span", "process-identity");
    const name = element("span", "process-name");
    const meta = element("span", "process-meta");
    const command = element("span", "process-command");
    const elapsed = element("span", "process-duration");
    elapsed.title = "Elapsed time from process creation (or first observed exec) to exit";
    const elapsedValue = element("span", "duration-value");
    elapsed.append(element("span", "duration-label", "Elapsed"), elapsedValue);
    const state = element("span", "process-state");
    identity.append(name, command, meta);
    summary.append(chevron, identity, elapsed, state);
    const body = element("div", "process-body");
    const omitted = element("p", "omitted-events");
    const events = element("ol", "process-behaviors");
    events.setAttribute("aria-label", "Process behaviors");
    const children = element("ul", "process-children");
    children.setAttribute("aria-label", "Child processes");
    const filtered = element("p", "filtered-events", "No matching events");
    const evidence = new EventEvidence("Process details", this.onInspect);
    body.append(evidence.details, omitted, filtered, events, children);
    details.append(summary, body);
    item.append(details);
    details.addEventListener("animationend", (event) => {
      if (event.animationName === "trace-activity") details.classList.remove("has-activity");
    });
    const view = { item, details, name, command, meta, elapsedValue, state, omitted, filtered, events, children, evidence, activity: 0 };
    this.nodes.set(node.id, view);
    return view;
  }

  createEvent(entry) {
    const event = entry.event;
    const item = element("li", "behavior-item");
    const button = element("button", "event-row");
    button.type = "button";
    button.dataset.category = event.category || String(event.kind).split(".")[0];
    button.setAttribute("aria-pressed", "false");
    const ns = entry.started ?? eventTime(event);
    const time = timestamp(event, ns);
    const shortKind = entry.label || String(event.kind).replace(/^process\./, "");
    const kind = element("span", "event-type", shortKind);
    if (["exec", "exit", "fork", "vfork", "clone"].includes(shortKind)) kind.classList.add(shortKind);
    if (shortKind.startsWith("file.")) kind.classList.add("file");
    if (shortKind.startsWith("memory.")) kind.classList.add("memory");
    if (event.kind.startsWith("network.")) kind.classList.add("network");
    const detail = element("span", "event-description");
    button.append(time, kind, detail);
    const view = { item, button, time, timeNs: ns, kind, detail, entry, expanded: true };
    if (entry.type === "tcp" || entry.type === "udp") {
      item.classList.add("trace-network-flow");
      view.evidence = new EventEvidence("Raw events", this.onInspect, () => this.updateExpansion(view));
      view.evidence.details.id = `trace-evidence-${entry.id}`;
      view.children = element("ol", "flow-behaviors");
      view.children.id = `trace-requests-${entry.id}`;
      view.children.setAttribute("aria-label", "HTTP exchanges in request start order");
      if (entry.type === "tcp") {
        view.toggle = element("button", "flow-toggle", "›");
        view.toggle.type = "button";
        view.toggle.title = "Expand or collapse HTTP exchanges";
        view.toggle.setAttribute("aria-label", view.toggle.title);
        view.toggle.setAttribute("aria-controls", view.children.id);
        view.toggle.addEventListener("click", () => { view.expanded = !view.expanded; this.updateExpansion(view); });
      } else button.setAttribute("aria-controls", view.evidence.details.id);
    }
    button.addEventListener("click", () => {
      if (view.entry.type === "udp") { view.evidence.details.open = !view.evidence.details.open; return; }
      this.selected?.setAttribute("aria-pressed", "false");
      this.selected = button;
      button.setAttribute("aria-pressed", "true");
      this.onSelect(view.entry.event, view.entry.type === "tcp" ? view.entry.connectionId : null);
    });
    item.append(button);
    if (view.toggle) item.append(view.toggle);
    if (view.evidence) item.append(view.evidence.details, view.children);
    this.events.set(entry.id, view);
    return view;
  }

  updateExpansion(view) {
    if (!view.children) return;
    view.children.hidden = !view.expanded || !view.entry.children.length;
    if (view.toggle) {
      view.toggle.hidden = !view.entry.children.length;
      view.toggle.setAttribute("aria-expanded", String(view.expanded));
    } else view.button.setAttribute("aria-expanded", String(view.evidence.details.open));
  }

  renderBehaviors(behaviors, destination, retained) {
    let cursor = destination.firstChild;
    for (const entry of behaviors) {
      retained.add(entry.id);
      const view = this.events.get(entry.id) || this.createEvent(entry);
      view.entry = entry;
      if (view.item !== cursor) destination.insertBefore(view.item, cursor);
      cursor = view.item.nextSibling;
      view.button.dataset.kind = entry.type ? `network.${entry.type}` : entry.event.kind;
      if (!view.children) view.button.dataset.sequence = entry.event.sequence;
      if (entry.type === "http") view.button.dataset.requestId = entry.requestId;
      setText(view.kind, entry.label || entry.event.kind.replace(/^process\./, ""));
      const ns = entry.started ?? eventTime(entry.event);
      if (view.timeNs !== ns) {
        const time = timestamp(entry.event, ns);
        view.time.replaceWith(time);
        view.time = time; view.timeNs = ns;
      }
      const description = entry.title ? `${entry.title}\n${entry.description}` : entry.description;
      setText(view.detail, description.length > 320 ? description.slice(0, 320) + "…" : description);
      view.detail.title = description;
      if (view.children) {
        if (entry.type === "tcp") {
          view.item.dataset.connectionId = entry.connectionId;
          view.button.title = "Inspect this connection in Traffic";
        } else view.item.dataset.flowId = entry.flowId;
        view.evidence.update(entry.records);
        this.updateExpansion(view);
        this.renderBehaviors(entry.children, view.children, retained);
      }
    }
  }

  render({ nodes, behaviors, behaviorCounts: counts, totalBehaviorCounts, processRecords }) {
    // Measure before changing the tree. Readers who scroll up keep their place;
    // returning to the bottom resumes following newly appended commands.
    const follow = this.scroll.scrollHeight - this.scroll.clientHeight - this.scroll.scrollTop <= 24;
    // Reuse DOM nodes: new events must not reopen collapsed branches or replace
    // the focused summary/button. A late fork can move an existing subtree.
    for (const node of nodes.values()) {
      const view = this.nodes.get(node.id) || this.createNode(node);
      const name = node.name?.split("/").at(-1) || (node.hasExec ? "Unknown executable" : "Process");
      setText(view.name, name.length > 80 ? name.slice(0, 80) + "…" : name);
      view.name.title = node.name || "Executable was not captured";
      view.details.dataset.pid = node.pid ?? "unknown";
      view.details.dataset.instance = node.instanceId ?? node.id;
      view.details.dataset.state = node.state;
      const origin = node.root ? "root" : node.birth || "parent not observed";
      const count = counts.get(node.id) ?? 0;
      const total = totalBehaviorCounts.get(node.id) ?? 0;
      const events = count === total ? `${count}` : `${count}/${total}`;
      setText(view.meta, `PID ${node.pid ?? "—"} · ${origin} · ${events} behaviors`);
      const argv = node.events.findLast(entry => entry.event.kind === "process.exec")?.event.data.argv;
      const command = Array.isArray(argv) && argv.length > 1 ? formatCommand(argv) : "";
      view.command.hidden = !command;
      setText(view.command, command.length > 240 ? command.slice(0, 240) + "…" : command);
      view.command.title = command;
      setText(view.state, node.exit || node.state);
      setText(view.elapsedValue, duration(node));
      view.state.dataset.state = node.state;
      if (node.state !== "running") view.details.classList.remove("has-activity");
      else if (node.activity > view.activity) view.details.classList.add("has-activity");
      view.activity = Math.max(view.activity, node.activity);
      view.omitted.hidden = node.omitted === 0;
      view.filtered.hidden = count > 0 || processRecords.has(node.id) || node.events.length === 0;
      setText(view.omitted, `${node.omitted} earlier records omitted from this view`);
      view.evidence.update(processRecords.get(node.id) || []);
    }
    for (const node of nodes.values()) {
      const view = this.nodes.get(node.id);
      const destination = node.parentId ? this.nodes.get(node.parentId).children : this.root;
      if (view.item.parentNode !== destination) destination.append(view.item);
    }
    for (const [id, view] of this.nodes) {
      if (!nodes.has(id)) { view.item.remove(); this.nodes.delete(id); }
    }
    const retained = new Set();
    const byOwner = new Map();
    for (const entry of behaviors) {
      if (!byOwner.has(entry.nodeId)) byOwner.set(entry.nodeId, []);
      byOwner.get(entry.nodeId).push(entry);
    }
    for (const [id, view] of this.nodes) this.renderBehaviors(byOwner.get(id) || [], view.events, retained);
    for (const [id, view] of this.events) {
      if (!retained.has(id)) { view.item.remove(); this.events.delete(id); }
    }
    this.empty.hidden = nodes.size > 0;
    if (follow) this.scroll.scrollTop = this.scroll.scrollHeight;
  }
}
