import { eventTime } from "./event-time.mjs";
import { EventEvidence } from "./event-evidence.mjs";
import { formatCommand } from "./event-description.mjs";

function element(tag, className, text) {
  const node = document.createElement(tag);
  node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}
function text(node, value) { if (node.textContent !== value) node.textContent = value; }
const short = value => String(value).length > 240 ? String(value).slice(0, 240) + "…" : String(value);

function place(views, destination) {
  let cursor = destination.firstChild;
  for (const { item } of views) {
    if (item !== cursor) destination.insertBefore(item, cursor);
    cursor = item.nextSibling;
  }
  while (cursor) { const next = cursor.nextSibling; cursor.remove(); cursor = next; }
}
const chronological = (a, b) => (a.started ?? Infinity) - (b.started ?? Infinity) || (a.order ?? 0) - (b.order ?? 0);

function cardTitle(entry) {
  if (entry.title) return entry.title;
  const { kind, data: d = {} } = entry.event;
  if (kind === "file.open") return d.path?.split("/").filter(Boolean).at(-1) || d.path || "Path unavailable";
  if (kind === "memory.mmap") return d.anonymous ? `Anonymous · ${d.length ?? "?"} B` : d.resolved_path || `File mapping · FD ${d.fd ?? "?"}`;
  if (kind === "network.http.request") return `${d.method || "HTTP"} ${d.url || `request #${d.request_id}`}`;
  if (kind === "network.http.response") return `${d.status_code ?? "HTTP response"} · request #${d.request_id}`;
  if (kind === "network.protocol") return d.domain || d.destination || "Protocol detected";
  if (kind.startsWith("network.")) return d.domain || d.peer || d.destination || (d.request_id ? `Request #${d.request_id}` : "Network activity");
  return entry.description;
}

// A rendering of the shared process projection, not a second event collector.
// Nested lists own the hierarchy; CSS draws each branch without measuring cards.
export class BehaviorGraph {
  constructor({ root, scroll, empty, onSelect, onInspect }) {
    this.root = root;
    this.scroll = scroll;
    this.empty = empty;
    this.onSelect = onSelect;
    this.onInspect = onInspect;
    this.roots = root.querySelector(".graph-roots");
    this.nodes = new Map();
    this.events = new Map();
    this.selected = null;
    this.snapshot = null;
    this.latest = null;
  }

  createProcess(node) {
    const item = element("li", "graph-branch graph-tree-item");
    item.dataset.nodeId = node.id;
    const layout = element("div", "graph-layout");
    const button = element("button", "graph-card graph-process-card");
    button.type = "button";
    button.dataset.category = "process";
    const identity = element("span", "graph-card-kind");
    const name = element("span", "graph-process-name");
    const chevron = element("span", "graph-chevron", "›");
    chevron.setAttribute("aria-hidden", "true");
    const meta = element("span", "graph-card-meta");
    const command = element("span", "graph-card-description");
    const state = element("span", "graph-card-meta graph-process-state");
    button.append(identity, name, command, meta, state, chevron);
    const body = element("div", "graph-body graph-subtree");
    body.id = `graph-body-${node.id}`;
    button.setAttribute("aria-controls", body.id);
    const omitted = element("p", "graph-note");
    const filtered = element("p", "graph-note", "No matching events");
    const children = element("ol", "graph-children graph-tree-children");
    children.setAttribute("aria-label", "Process behaviors and child processes in start order");
    const evidence = new EventEvidence("Process details", this.onInspect);
    const context = element("div", "graph-context");
    context.append(evidence.details, omitted, filtered);
    body.append(context, children);
    layout.append(button, body);
    item.append(layout);
    const view = { item, button, identity, name, command, meta, state, body, omitted, filtered, children, evidence, expanded: node.root, activity: 0 };
    button.addEventListener("click", () => {
      view.expanded = !view.expanded;
      this.render(this.snapshot, false);
    });
    button.addEventListener("animationend", () => button.classList.remove("has-activity"));
    this.nodes.set(node.id, view);
    return view;
  }

  createEvent(entry) {
    const item = element("li", "graph-event-item graph-tree-item");
    const button = element("button", "graph-card graph-event-card");
    button.type = "button";
    const category = entry.event.category || String(entry.event.kind).split(".")[0];
    button.dataset.category = ["file", "network", "memory", "process"].includes(category) ? category : "other";
    const heading = element("span", "graph-event-heading");
    const kind = element("span", "graph-card-kind");
    const time = element("time", "graph-card-time");
    heading.append(kind, time);
    const title = element("span", "graph-event-title");
    const description = element("span", "graph-card-description");
    button.append(heading, title, description);
    const view = { item, button, kind, time, title, description, entry, owner: entry.nodeId, expanded: entry.type !== "udp", activity: 0 };
    if (entry.type === "tcp" || entry.type === "udp") {
      item.classList.add("graph-network-flow");
      view.body = element("div", "graph-flow-body graph-subtree");
      view.body.id = `graph-flow-${entry.id}`;
      button.setAttribute("aria-controls", view.body.id);
      view.evidence = new EventEvidence("Raw events", this.onInspect);
      const context = element("div", "graph-context");
      context.append(view.evidence.details);
      view.children = element("ol", "graph-http-requests graph-tree-children");
      view.children.setAttribute("aria-label", "HTTP exchanges in request start order");
      view.body.append(context, view.children);
      const chevron = element("span", "graph-chevron", "›");
      chevron.setAttribute("aria-hidden", "true");
      button.append(chevron);
    }
    button.addEventListener("click", () => {
      if (view.children) {
        view.expanded = !view.expanded;
        if (view.expanded && !view.entry.children.length) view.evidence.details.open = true;
        this.render(this.snapshot, false);
        return;
      }
      this.events.get(this.selected)?.button.setAttribute("aria-pressed", "false");
      this.selected = entry.id;
      button.setAttribute("aria-pressed", "true");
      this.onSelect(view.entry.event);
    });
    button.addEventListener("animationend", () => button.classList.remove("has-activity"));
    item.append(button);
    if (view.body) item.append(view.body);
    this.events.set(entry.id, view);
    return view;
  }

  renderCards(cards) {
    return cards.map(entry => {
      const view = this.events.get(entry.id) || this.createEvent(entry);
      view.entry = entry;
      if (!view.children) view.button.dataset.sequence = entry.event.sequence;
      view.button.dataset.kind = entry.type ? `network.${entry.type}` : entry.event.kind;
      text(view.kind, entry.label || entry.event.kind.replace(/^process\./, ""));
      const ns = entry.started ?? eventTime(entry.event);
      view.started = ns;
      view.order = entry.firstId ?? entry.id;
      if (view.timeNs !== ns) {
        view.timeNs = ns;
        const date = new Date(ns === null ? NaN : ns / 1e6);
        text(view.time, Number.isFinite(date.getTime()) ? date.toLocaleTimeString(undefined, { hour12: false, hour: "2-digit", minute: "2-digit", second: "2-digit", fractionalSecondDigits: 3 }) : "—");
        if (Number.isFinite(date.getTime())) { view.time.dateTime = date.toISOString(); view.time.title = `Start: ${date.toISOString()}`; }
      }
      const title = cardTitle(entry);
      text(view.title, short(title));
      view.title.title = title;
      text(view.description, short(entry.description));
      view.description.title = entry.description;
      view.button.setAttribute("aria-pressed", String(this.selected === entry.id));
      const activity = entry.activity ?? entry.id;
      if (this.latest !== null && activity > this.latest && activity > view.activity) view.button.classList.add("has-activity");
      view.activity = activity;
      if (entry.type === "http") view.button.dataset.requestId = entry.requestId;
      if (view.children) {
        if (entry.type === "tcp") view.item.dataset.connectionId = entry.connectionId;
        else view.item.dataset.flowId = entry.flowId;
        view.button.setAttribute("aria-expanded", String(view.expanded));
        view.button.setAttribute("aria-label", `${title}, ${view.expanded ? "collapse" : "expand"} ${entry.type === "tcp" ? "details and HTTP exchanges" : "packet details"}`);
        view.body.hidden = !view.expanded;
        view.evidence.update(entry.records);
        place(this.renderCards(entry.children), view.children);
      }
      return view;
    });
  }

  render(snapshot, followUpdates = true) {
    const follow = followUpdates && this.scroll.scrollHeight - this.scroll.clientHeight - this.scroll.scrollTop <= 24;
    this.snapshot = snapshot;
    const { nodes, entries, behaviorCounts: counts, totalBehaviorCounts, behaviors: cards, processRecords } = snapshot;
    const byOwner = new Map();
    const retained = new Set();
    for (const card of cards) {
      if (!byOwner.has(card.nodeId)) byOwner.set(card.nodeId, []);
      byOwner.get(card.nodeId).push(card);
      retained.add(card.id);
      for (const child of card.children || []) retained.add(child.id);
    }
    const visibleOwners = new Set();
    for (const node of nodes.values()) {
      const view = this.nodes.get(node.id) || this.createProcess(node);
      // Preserve the first known start even after its raw record leaves the window.
      const birth = nodes.get(node.parentId)?.events.find(entry =>
        ["process.fork", "process.vfork", "process.clone"].includes(entry.event.kind)
        && (node.instanceId != null ? entry.event.data.child_process_instance_id === node.instanceId : entry.event.data.child_pid === node.pid));
      const first = birth || node.events[0];
      if (first) { view.started ??= eventTime(first.event); view.order ??= first.id; }
      view.button.dataset.pid = node.pid ?? "unknown";
      view.button.dataset.instance = node.instanceId ?? node.id;
      view.button.dataset.state = node.state;
      view.button.setAttribute("aria-expanded", String(view.expanded));
      view.body.hidden = !view.expanded;
      const name = node.name?.split("/").at(-1) || (node.hasExec ? "Unknown executable" : "Process");
      const count = counts.get(node.id) ?? 0;
      text(view.identity, `PID ${node.pid ?? "—"} · ${node.root ? "root" : node.birth || "parent unknown"}`);
      text(view.name, short(name));
      view.name.title = node.name || "Executable was not captured";
      const argv = node.events.findLast(entry => entry.event.kind === "process.exec")?.event.data.argv;
      const command = Array.isArray(argv) && argv.length > 1 ? formatCommand(argv) : "";
      view.command.hidden = !command;
      text(view.command, short(command));
      view.command.title = command;
      const total = totalBehaviorCounts.get(node.id) ?? 0;
      text(view.meta, `${count === total ? count : `${count}/${total}`} behaviors · ${node.children.size} child processes`);
      text(view.state, node.exit || node.state);
      view.button.setAttribute("aria-label", `${name}, PID ${node.pid ?? "unknown"}, ${view.expanded ? "collapse" : "expand"} events and child processes`);
      if (node.state !== "running") view.button.classList.remove("has-activity");
      else if (this.latest !== null && node.activity > view.activity) view.button.classList.add("has-activity");
      view.activity = Math.max(view.activity, node.activity);
      view.omitted.hidden = node.omitted === 0;
      text(view.omitted, `${node.omitted} earlier records omitted from this view`);
      view.filtered.hidden = count > 0 || processRecords.has(node.id) || node.events.length === 0;
      view.evidence.update(processRecords.get(node.id) || []);
    }
    for (const node of nodes.values()) {
      let expanded = this.nodes.get(node.id).expanded;
      for (let parent = nodes.get(node.parentId); parent; parent = nodes.get(parent.parentId)) {
        expanded &&= this.nodes.get(parent.id).expanded;
      }
      if (expanded) visibleOwners.add(node.id);
    }
    for (const [id, view] of this.nodes) {
      if (!nodes.has(id)) { view.item.remove(); this.nodes.delete(id); }
    }
    const roots = [];
    for (const node of nodes.values()) {
      const view = this.nodes.get(node.id);
      const children = [...node.children].map(id => this.nodes.get(id)).filter(Boolean);
      const behaviors = visibleOwners.has(node.id) ? this.renderCards(byOwner.get(node.id) || []) : [];
      place([...children, ...behaviors].sort(chronological), view.children);
      if (!nodes.has(node.parentId)) roots.push(view);
    }
    place(roots.sort(chronological), this.roots);
    // Collapsing a process detaches its cards, but keeps bounded view state so
    // expanding an ancestor does not reopen a previously collapsed TCP branch.
    for (const [id, view] of this.events) {
      if (!retained.has(id)) { view.item.remove(); this.events.delete(id); }
    }
    this.latest = entries.reduce((max, entry) => Math.max(max, entry.id), this.latest ?? 0);
    this.empty.hidden = nodes.size > 0;
    if (follow) this.scroll.scrollTop = this.scroll.scrollHeight;
  }
}
