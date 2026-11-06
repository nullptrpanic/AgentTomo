// Compatibility reader for schema v1 task records only.
import { describeBehavior, formatCommand } from "./event-description.mjs";
// Project trace semantics, independent of DOM and transport. PID is the observed
// thread-group ID; clone records alone cannot identify a separate process.
const isTid = (value) => Number.isInteger(value) && value > 0;
const creationKinds = new Set(["process.fork", "process.vfork", "process.clone"]);
const proxyKinds = new Set(["network.upstream", "network.transfer", "network.protocol", "network.udp.transfer", "network.udp.error"]);
const connectionKinds = new Set(["network.connect", "network.upstream", "network.transfer"]);

export function taskIdentity(pid, tid) {
  return `PID ${isTid(pid) ? pid : "—"}${isTid(tid) && tid !== pid ? ` · TID ${tid}` : ""}`;
}


export class LegacyProcessTree {
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
        id: `task-${++this.nextId}`, tid, pid: null, taskId, parentId: null, children: new Set(),
        name: tid === null ? "Unattributed events" : null, state: tid === null || !live ? "unknown" : "running",
        birth: null, root: false, hasExec: false, events: [], omitted: 0, exit: null,
        startedNs: null, endedNs: null, activity: 0,
        image: null, end: null, retiredImage: null, retiredEnd: null,
      };
      this.nodes.set(node.id, node);
    }
    if (taskId !== null) { node.taskId = taskId; this.byTask.set(taskId, node); }
    if (live && node.state === "unknown" && tid !== null) node.state = "running";
    if (tid === null || (live && node.state === "running")) {
      this.active.set(tid, node);
      this.latest.set(tid, node);
    }
    return node;
  }

  add(event) {
    const data = event.data || {};
    const time = Number.isFinite(event.monotonic_ns) && event.monotonic_ns >= 0 ? event.monotonic_ns : null;
    let node;
    let description;
    if (creationKinds.has(event.kind) && isTid(data.parent_tid) && isTid(data.child_tid)) {
      node = this.task(data.parent_tid, data.task_id);
      // waitpid can report a child exec/exit before its parent's creation stop.
      const observed = this.latest.get(data.child_tid);
      const child = isTid(data.child_task_id) ? this.task(data.child_tid, data.child_task_id) : observed && !observed.birth && !observed.root ? observed : this.task(data.child_tid);
      if (child !== node) {
        if (isTid(data.child_pid)) child.pid = data.child_pid;
        child.parentId = node.id;
        child.birth = event.kind.replace("process.", "");
        child.startedNs ??= time;
        if (!child.hasExec) child.name ??= node.name;
        node.children.add(child.id);
      }
      description = `${taskIdentity(data.pid, data.parent_tid)} → ${taskIdentity(data.child_pid, data.child_tid)}`;
    } else {
      if (event.kind === "process.exec" && isTid(data.former_tid) && data.former_tid !== data.tid && isTid(data.tid)) {
        node = this.task(data.former_tid, data.task_id);
        const previous = this.active.get(data.tid);
        if (previous && previous !== node) {
          previous.state = "replaced";
          previous.exit = "replaced by exec";
          previous.endedNs = time;
        }
        this.active.delete(data.former_tid);
        if (this.latest.get(data.former_tid) === node) this.latest.delete(data.former_tid);
        node.tid = data.tid;
        this.active.set(data.tid, node);
        this.latest.set(data.tid, node);
      } else {
        node = this.task(data.tid, data.task_id, !proxyKinds.has(event.kind) && !["tcp_proxy", "udp_proxy"].includes(event.source));
      }
      if (event.kind === "process.exec") {
        node.startedNs ??= time;
        const previous = node.name;
        const argv = Array.isArray(data.argv) ? data.argv : [];
        node.hasExec = true;
        node.name = argv[0] || null;
        node.image = { id: this.total + 1, name: node.name };
        description = `${previous || "exec"} → ${argv.length ? formatCommand(argv) : "arguments unavailable"}`;
        if (data.former_tid && data.former_tid !== data.tid) description += ` (TID ${data.former_tid} → ${data.tid})`;
      } else if (event.kind === "process.exit") {
        node.state = "exited";
        node.endedNs = time;
        node.exit = data.signal ? `signal ${data.signal}` : `exit ${data.exit_code ?? "?"}`;
        node.end = { id: this.total + 1, time, exit: node.exit };
        description = node.exit;
        if (this.active.get(node.tid) === node) this.active.delete(node.tid);
      } else {
        const name = connectionKinds.has(event.kind) && isTid(data.connection_id)
          ? this.entries.findLast(entry => entry.event.kind === "network.protocol" && entry.event.data.connection_id === data.connection_id)?.event.data.domain
          : null;
        description = describeBehavior(event.kind, data, name);
      }
    }
    if (event.kind === "network.protocol" && isTid(data.connection_id)) {
      // A name arrives after request bytes, usually after connect. Enrich only
      // retained rows for this connection; raw event evidence stays untouched.
      for (const entry of this.entries) {
        if (connectionKinds.has(entry.event.kind) && entry.event.data.connection_id === data.connection_id) {
          entry.description = describeBehavior(entry.event.kind, entry.event.data, data.domain);
        }
      }
    }
    if (isTid(data.pid)) node.pid = data.pid;
    const entry = { id: ++this.total, event, description, nodeId: node.id };
    node.events.push(entry);
    // A collapsed ancestor signals activity in a hidden running descendant too.
    for (let owner = node; owner; owner = this.nodes.get(owner.parentId)) owner.activity = entry.id;
    this.entries.push(entry);
    while (this.entries.length > this.limit) {
      const oldest = this.entries.shift();
      const owner = this.nodes.get(oldest.nodeId);
      owner.events.shift();
      owner.omitted += 1;
      this.prune(owner);
    }
  }

  processOwner(node) {
    // A creation relationship proves shared lifetime; PID equality alone does
    // not, since a later process can reuse an exited process's numeric PID.
    let parent;
    while (isTid(node.pid) && (parent = this.nodes.get(node.parentId)) && parent.pid === node.pid) node = parent;
    return node;
  }

  processView() {
    const nodes = new Map();
    const owners = new Map();
    for (const task of this.nodes.values()) {
      const owner = this.processOwner(task);
      owners.set(task.id, owner.id);
      if (!nodes.has(owner.id)) nodes.set(owner.id, { ...owner, events: [], children: new Set(), members: [] });
      nodes.get(owner.id).members.push(task);
    }
    for (const process of nodes.values()) {
      process.parentId = owners.get(process.parentId) ?? null;
      if (process.parentId) nodes.get(process.parentId).children.add(process.id);
      const members = process.members;
      const image = members.flatMap(n => [n.image, n.retiredImage]).filter(Boolean).reduce(newest, null);
      const end = members.flatMap(n => [n.end, n.retiredEnd]).filter(Boolean).reduce(newest, null);
      if (image) { process.name = image.name; process.hasExec = true; }
      process.state = members.some(n => n.state === "running") ? "running"
        : members.some(n => n.state === "unknown") ? "unknown" : "exited";
      process.exit = process.state === "exited" ? end?.exit ?? process.exit : null;
      process.endedNs = process.state === "exited" ? end?.time ?? null : null;
      process.end = end;
      process.omitted = members.reduce((sum, n) => sum + n.omitted, 0);
      process.activity = 0;
    }
    const entries = [];
    for (const entry of this.entries) {
      const { kind, data } = entry.event;
      const process = nodes.get(owners.get(entry.nodeId));
      if (creationKinds.has(kind) && isTid(data.pid) && data.pid === data.child_pid) continue;
      if (kind === "process.exit" && (process.state !== "exited" || process.end?.id !== entry.id)) continue;
      let description = entry.description;
      if (creationKinds.has(kind)) description = `${taskIdentity(data.pid)} → ${taskIdentity(data.child_pid)}`;
      if (kind === "process.exec") description = description.replace(/ \(TID \d+ → \d+\)$/, "");
      const projected = { ...entry, nodeId: process.id, description };
      process.events.push(projected);
      entries.push(projected);
    }
    return { nodes, entries };
  }

  prune(node) {
    while (node && node.events.length === 0 && node.children.size === 0 && node.state !== "running") {
      const owner = this.processOwner(node);
      if (owner !== node) {
        // Retain only lifecycle summaries, not unbounded finished thread nodes.
        owner.retiredImage = [owner.retiredImage, node.image, node.retiredImage].reduce(newest, null);
        owner.retiredEnd = [owner.retiredEnd, node.end, node.retiredEnd].reduce(newest, null);
        owner.omitted += node.omitted;
      }
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

function newest(first, second) {
  return (second?.id ?? 0) > (first?.id ?? 0) ? second : first;
}
