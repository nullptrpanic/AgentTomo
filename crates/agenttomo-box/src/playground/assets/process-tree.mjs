// Project trace semantics, independent of DOM and transport. TIDs are task IDs;
// clone records alone cannot tell us whether the child is a separate process.
const isTid = (value) => Number.isInteger(value) && value > 0;
const creationKinds = new Set(["process.fork", "process.vfork", "process.clone"]);
const command = (argv) => argv.map(arg => /\s/.test(arg) ? JSON.stringify(arg) : arg).join(" ");
const proxyKinds = new Set(["network.upstream", "network.transfer"]);

function behavior(kind, data) {
  const result = data.restart ? "interrupted; restart pending" : data.errno ? `errno ${data.errno}` : data.result ?? "result unavailable";
  if (kind === "file.open") return `${data.syscall || "open"} ${data.path ?? "[unreadable path]"} → ${typeof result === "number" && result >= 0 ? `FD ${result}` : result}`;
  if (kind === "network.connect") return `${data.transport || "socket"} ${data.destination ?? "[address unavailable]"} · ${data.redirected ? "local proxy" : "direct"} → ${result}${data.skip_reason ? ` · ${data.skip_reason}` : ""}`;
  if (kind === "network.upstream") return `${data.destination} · upstream ${data.status}${data.error ? `: ${data.error}` : ""}`;
  if (kind === "network.transfer") return `${data.destination ?? "connection"} · ${data.status} · ↑ ${data.bytes_up ?? "?"} B · ↓ ${data.bytes_down ?? "?"} B${data.error ? ` · ${data.error}` : ""}`;
  return JSON.stringify(data);
}

export class ProcessTree {
  constructor(limit = 1000) {
    this.limit = limit;
    this.nodes = new Map();
    this.active = new Map();
    this.latest = new Map();
    this.byTask = new Map();
    this.entries = [];
    this.total = 0;
    this.nextId = 0;
  }

  get roots() {
    return [...this.nodes.values()].filter(node => node.parentId === null);
  }

  start(tid) {
    if (isTid(tid)) (this.roots.find(node => node.tid === tid) || this.latest.get(tid) || this.task(tid)).root = true;
  }

  task(tid, taskId = null, live = true) {
    if (!isTid(tid)) tid = null;
    if (!isTid(taskId)) taskId = null;
    let node = taskId === null ? this.active.get(tid) : this.byTask.get(taskId);
    // A ready message may introduce the root before its first stable ID arrives.
    if (!node && taskId !== null && this.active.get(tid)?.taskId === null) node = this.active.get(tid);
    if (!node) {
      node = {
        id: `task-${++this.nextId}`, tid, taskId, parentId: null, children: new Set(),
        name: tid === null ? "Unattributed events" : null, state: tid === null || !live ? "unknown" : "running",
        birth: null, root: false, hasExec: false, events: [], omitted: 0, exit: null,
      };
      this.nodes.set(node.id, node);
    }
    if (taskId !== null) { node.taskId = taskId; this.byTask.set(taskId, node); }
    if (live && node.state === "unknown" && tid !== null) node.state = "running";
    if (live && (node.state === "running" || tid === null)) {
      this.active.set(tid, node);
      this.latest.set(tid, node);
    }
    return node;
  }

  add(event) {
    const data = event.data || {};
    let node;
    let description;
    if (creationKinds.has(event.kind) && isTid(data.parent_tid) && isTid(data.child_tid)) {
      node = this.task(data.parent_tid, data.task_id);
      // waitpid can report a child exec/exit before its parent's creation stop.
      const observed = this.latest.get(data.child_tid);
      const child = isTid(data.child_task_id) ? this.task(data.child_tid, data.child_task_id) : observed && !observed.birth && !observed.root ? observed : this.task(data.child_tid);
      if (child !== node) {
        child.parentId = node.id;
        child.birth = event.kind.replace("process.", "");
        if (!child.hasExec) child.name ??= node.name;
        node.children.add(child.id);
      }
      description = `TID ${data.parent_tid} → TID ${data.child_tid}`;
    } else {
      if (event.kind === "process.exec" && isTid(data.former_tid) && data.former_tid !== data.tid && isTid(data.tid)) {
        node = this.task(data.former_tid, data.task_id);
        const previous = this.active.get(data.tid);
        if (previous && previous !== node) {
          previous.state = "replaced";
          previous.exit = "replaced by exec";
        }
        this.active.delete(data.former_tid);
        if (this.latest.get(data.former_tid) === node) this.latest.delete(data.former_tid);
        node.tid = data.tid;
        this.active.set(data.tid, node);
        this.latest.set(data.tid, node);
      } else {
        node = this.task(data.tid, data.task_id, !proxyKinds.has(event.kind));
      }
      if (event.kind === "process.exec") {
        const previous = node.name;
        const argv = Array.isArray(data.argv) ? data.argv : [];
        node.hasExec = true;
        node.name = argv[0] || null;
        description = `${previous || "exec"} → ${argv.length ? command(argv) : "arguments unavailable"}`;
        if (data.former_tid && data.former_tid !== data.tid) description += ` (TID ${data.former_tid} → ${data.tid})`;
      } else if (event.kind === "process.exit") {
        node.state = "exited";
        node.exit = data.signal ? `signal ${data.signal}` : `exit ${data.exit_code ?? "?"}`;
        description = node.exit;
        if (this.active.get(node.tid) === node) this.active.delete(node.tid);
      } else {
        description = behavior(event.kind, data);
      }
    }
    const entry = { id: ++this.total, event, description, nodeId: node.id };
    node.events.push(entry);
    this.entries.push(entry);
    while (this.entries.length > this.limit) {
      const oldest = this.entries.shift();
      const owner = this.nodes.get(oldest.nodeId);
      owner.events.shift();
      owner.omitted += 1;
      this.prune(owner);
    }
  }

  prune(node) {
    while (node && node.events.length === 0 && node.children.size === 0 && node.state !== "running") {
      this.nodes.delete(node.id);
      if (this.byTask.get(node.taskId) === node) this.byTask.delete(node.taskId);
      if (this.latest.get(node.tid) === node) this.latest.delete(node.tid);
      if (this.active.get(node.tid) === node) this.active.delete(node.tid);
      const parent = this.nodes.get(node.parentId);
      parent?.children.delete(node.id);
      node = parent;
    }
  }
}
