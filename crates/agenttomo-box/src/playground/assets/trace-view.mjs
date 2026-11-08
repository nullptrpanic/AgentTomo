import { ProcessTree } from "./process-tree.mjs";
import { EventFilter, FilterMenu } from "./event-filter.mjs";
import { eventTime, orderByStart } from "./event-time.mjs";

function element(tag, className, text) {
  const node = document.createElement(tag);
  node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function setText(node, text) {
  if (node.textContent !== text) node.textContent = text;
}

function timestamp(event) {
  const ns = eventTime(event);
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
  constructor({ root, scroll, empty, count, summary, filters, onSelect }) {
    this.tree = new ProcessTree();
    this.filter = new EventFilter();
    this.filterMenu = filters ? new FilterMenu(filters, this.filter, () => this.schedule()) : null;
    this.root = root;
    this.scroll = scroll;
    this.empty = empty;
    this.count = count;
    this.summary = summary;
    this.onSelect = onSelect;
    this.nodes = new Map();
    this.events = new Map();
    this.scheduled = false;
    this.selected = null;
  }

  start(tid) {
    this.tree.start(tid);
    this.schedule();
  }

  add(event) {
    this.filter.observe(event.kind);
    this.tree.add(event);
    this.schedule();
  }

  schedule() {
    if (this.scheduled) return;
    this.scheduled = true;
    requestAnimationFrame(() => {
      this.scheduled = false;
      this.render();
    });
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
    const elapsed = element("span", "process-duration");
    elapsed.title = "Elapsed time from process creation (or first observed exec) to exit";
    const elapsedValue = element("span", "duration-value");
    elapsed.append(element("span", "duration-label", "Elapsed"), elapsedValue);
    const state = element("span", "process-state");
    identity.append(name, meta);
    summary.append(chevron, identity, elapsed, state);
    const body = element("div", "process-body");
    const omitted = element("p", "omitted-events");
    const events = element("ol", "process-behaviors");
    events.setAttribute("aria-label", "Process behaviors");
    const children = element("ul", "process-children");
    children.setAttribute("aria-label", "Child processes");
    const filtered = element("p", "filtered-events", "No matching events");
    body.append(omitted, filtered, events, children);
    details.append(summary, body);
    item.append(details);
    details.addEventListener("animationend", (event) => {
      if (event.animationName === "trace-activity") details.classList.remove("has-activity");
    });
    const view = { item, details, name, meta, elapsedValue, state, omitted, filtered, events, children, activity: 0 };
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
    const time = timestamp(event);
    const shortKind = String(event.kind).replace(/^process\./, "");
    const kind = element("span", "event-type", shortKind);
    if (["exec", "exit", "fork", "vfork", "clone"].includes(shortKind)) kind.classList.add(shortKind);
    if (shortKind.startsWith("file.")) kind.classList.add("file");
    if (shortKind.startsWith("memory.")) kind.classList.add("memory");
    if (shortKind.startsWith("network.")) kind.classList.add("network");
    const detail = element("span", "event-description");
    button.append(time, kind, detail);
    button.addEventListener("click", () => {
      this.selected?.setAttribute("aria-pressed", "false");
      this.selected = button;
      button.setAttribute("aria-pressed", "true");
      this.onSelect(event);
    });
    item.append(button);
    this.events.set(entry.id, item);
    return item;
  }

  render() {
    // Measure before changing the tree. Readers who scroll up keep their place;
    // returning to the bottom resumes following newly appended commands.
    const follow = this.scroll.scrollHeight - this.scroll.clientHeight - this.scroll.scrollTop <= 24;
    const { nodes, entries } = this.tree.processView();
    const visible = orderByStart(entries.filter(entry => this.filter.allows(entry.event.kind)));
    const counts = new Map();
    for (const entry of visible) {
      counts.set(entry.nodeId, (counts.get(entry.nodeId) ?? 0) + 1);
      for (let node = nodes.get(entry.nodeId); node; node = nodes.get(node.parentId)) node.activity = Math.max(node.activity, entry.id);
    }
    this.filterMenu?.update();
    // Reuse DOM nodes: new events must not reopen collapsed branches or replace
    // the focused summary/button. A late fork can move an existing subtree.
    for (const node of nodes.values()) {
      const view = this.nodes.get(node.id) || this.createNode(node);
      const name = node.name?.split("/").at(-1) || (node.hasExec ? "Unknown executable" : "Process");
      setText(view.name, name.length > 80 ? name.slice(0, 80) + "…" : name);
      view.name.title = node.name || "Executable was not captured";
      view.details.dataset.pid = node.pid ?? "unknown";
      view.details.dataset.state = node.state;
      const origin = node.root ? "root" : node.birth || "parent not observed";
      const count = counts.get(node.id) ?? 0;
      const events = count === node.events.length ? `${count}` : `${count}/${node.events.length}`;
      setText(view.meta, `PID ${node.pid ?? "—"} · ${origin} · ${events} events`);
      setText(view.state, node.exit || node.state);
      setText(view.elapsedValue, duration(node));
      view.state.dataset.state = node.state;
      if (node.state !== "running") view.details.classList.remove("has-activity");
      else if (node.activity > view.activity) view.details.classList.add("has-activity");
      view.activity = Math.max(view.activity, node.activity);
      view.omitted.hidden = node.omitted === 0;
      view.filtered.hidden = count > 0 || node.events.length === 0;
      setText(view.omitted, `${node.omitted} earlier events omitted from this view`);
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
    const cursors = new Map([...this.nodes].map(([id, view]) => [id, view.events.firstChild]));
    for (const entry of visible) {
      retained.add(entry.id);
      const item = this.events.get(entry.id) || this.createEvent(entry);
      const cursor = cursors.get(entry.nodeId);
      if (item !== cursor) this.nodes.get(entry.nodeId).events.insertBefore(item, cursor);
      cursors.set(entry.nodeId, item.nextSibling);
      const detail = item.querySelector(".event-description");
      setText(detail, entry.description.length > 240 ? entry.description.slice(0, 240) + "…" : entry.description);
      detail.title = entry.description;
    }
    for (const [id, item] of this.events) {
      if (!retained.has(id)) { item.remove(); this.events.delete(id); }
    }
    this.empty.hidden = nodes.size > 0;
    setText(this.count, visible.length.toLocaleString());
    setText(this.summary, `${nodes.size} process${nodes.size === 1 ? "" : "es"} · ${visible.length}/${entries.length} events${this.tree.total > this.tree.limit ? " · recent history" : ""}`);
    if (follow) this.scroll.scrollTop = this.scroll.scrollHeight;
  }
}
