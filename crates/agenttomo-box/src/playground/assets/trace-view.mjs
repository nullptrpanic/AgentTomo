import { ProcessTree } from "./process-tree.mjs";

function element(tag, className, text) {
  const node = document.createElement(tag);
  node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function setText(node, text) {
  if (node.textContent !== text) node.textContent = text;
}

export class TraceView {
  constructor({ root, empty, count, summary, onSelect }) {
    this.tree = new ProcessTree();
    this.root = root;
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
    details.open = true;
    const summary = element("summary", "process-heading");
    const chevron = element("span", "process-chevron", "›");
    chevron.setAttribute("aria-hidden", "true");
    const identity = element("span", "process-identity");
    const name = element("span", "process-name");
    const meta = element("span", "process-meta");
    const state = element("span", "process-state");
    identity.append(name, meta);
    summary.append(chevron, identity, state);
    const body = element("div", "process-body");
    const omitted = element("p", "omitted-events");
    const events = element("ol", "process-behaviors");
    events.setAttribute("aria-label", "Task behaviors");
    const children = element("ul", "process-children");
    children.setAttribute("aria-label", "Child tasks");
    body.append(omitted, events, children);
    details.append(summary, body);
    item.append(details);
    const view = { item, details, name, meta, state, omitted, events, children };
    this.nodes.set(node.id, view);
    return view;
  }

  createEvent(entry) {
    const event = entry.event;
    const item = element("li", "behavior-item");
    const button = element("button", "event-row");
    button.type = "button";
    button.setAttribute("aria-pressed", "false");
    const time = element("span", "event-time", `+${(Number(event.monotonic_ns) / 1e9).toFixed(3)}s`);
    const shortKind = String(event.kind).replace(/^process\./, "");
    const kind = element("span", "event-type", shortKind);
    if (["exec", "exit", "fork", "vfork", "clone"].includes(shortKind)) kind.classList.add(shortKind);
    if (shortKind.startsWith("file.")) kind.classList.add("file");
    if (shortKind.startsWith("network.")) kind.classList.add("network");
    const description = entry.description;
    const detail = element("span", "event-description", description.length > 240 ? description.slice(0, 240) + "…" : description);
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
    const nodes = this.tree.nodes;
    // Reuse DOM nodes: new events must not reopen collapsed branches or replace
    // the focused summary/button. A late fork can move an existing subtree.
    for (const node of nodes.values()) {
      const view = this.nodes.get(node.id) || this.createNode(node);
      const name = node.name?.split("/").at(-1) || (node.hasExec ? "Unknown executable" : "Task");
      setText(view.name, name.length > 80 ? name.slice(0, 80) + "…" : name);
      view.name.title = node.name || "Executable was not captured";
      view.details.dataset.tid = node.tid ?? "unknown";
      view.details.dataset.state = node.state;
      const origin = node.root ? "root" : node.birth === "clone" ? "clone task" : node.birth || "parent not observed";
      setText(view.meta, node.tid === null ? "No TID recorded" : `TID ${node.tid} · ${origin} · ${node.events.length} events`);
      setText(view.state, node.exit || node.state);
      view.state.dataset.state = node.state;
      view.omitted.hidden = node.omitted === 0;
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
    for (const entry of this.tree.entries) {
      retained.add(entry.id);
      if (!this.events.has(entry.id)) this.nodes.get(entry.nodeId).events.append(this.createEvent(entry));
    }
    for (const [id, item] of this.events) {
      if (!retained.has(id)) { item.remove(); this.events.delete(id); }
    }
    this.empty.hidden = nodes.size > 0;
    setText(this.count, this.tree.total.toLocaleString());
    setText(this.summary, this.tree.total > this.tree.limit ? "Latest 1,000 behaviors · lineage retained" : `${nodes.size} task${nodes.size === 1 ? "" : "s"} · ${this.tree.total} events`);
  }
}
