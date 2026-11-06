// Native schema v2: PID owns behavior; an instance distinguishes PID reuse.
import { LegacyProcessTree } from "./legacy-process-tree.mjs";
import { describeBehavior, formatCommand } from "./event-description.mjs";

const valid = value => Number.isInteger(value) && value > 0;
const creationKinds = new Set(["process.fork", "process.vfork", "process.clone"]);
const connectionKinds = new Set(["network.upstream", "network.transfer"]);
const identity = pid => `PID ${valid(pid) ? pid : "—"}`;

export class ProcessTree {
  constructor(limit = 1000) {
    this.limit = limit;
    this.nodes = new Map();
    this.active = new Map();
    this.latest = new Map();
    this.byInstance = new Map();
    this.entries = [];
    this.count = 0;
    this.nextId = 0;
    this.rootPid = null;
    this.legacy = null;
  }

  get total() { return this.legacy?.total ?? this.count; }

  start(pid) {
    this.rootPid = pid;
    if (this.legacy) this.legacy.start(pid);
    else if (valid(pid)) (this.latest.get(pid) || this.process(pid)).root = true;
  }

  process(pid, instanceId = null, live = true) {
    if (!valid(pid)) pid = null;
    if (!valid(instanceId)) instanceId = null;
    let node = instanceId === null ? this.active.get(pid) : this.byInstance.get(instanceId);
    if (!node && instanceId !== null && this.active.get(pid)?.instanceId === null) node = this.active.get(pid);
    if (!node) {
      node = {
        id: `process-${++this.nextId}`, pid, instanceId, parentId: null, children: new Set(),
        name: pid === null ? "Unattributed events" : null, state: pid === null || !live ? "unknown" : "running",
        root: false, birth: null, hasExec: false, events: [], omitted: 0, exit: null,
        startedNs: null, endedNs: null, activity: 0,
      };
      this.nodes.set(node.id, node);
    }
    if (instanceId !== null) { node.instanceId = instanceId; this.byInstance.set(instanceId, node); }
    if (live && node.state === "unknown" && pid !== null) node.state = "running";
    if (pid === null || (live && node.state === "running")) {
      this.active.set(pid, node);
      this.latest.set(pid, node);
    }
    return node;
  }

  add(event) {
    const data = event.data || {};
    if (this.legacy || event.schema_version === 1 || Object.hasOwn(data, "tid") || Object.hasOwn(data, "parent_tid")) {
      if (!this.legacy) { this.legacy = new LegacyProcessTree(this.limit); this.legacy.start(this.rootPid); }
      this.legacy.add(event);
      return;
    }
    const time = Number.isFinite(event.monotonic_ns) && event.monotonic_ns >= 0 ? event.monotonic_ns : null;
    const node = this.process(data.pid, data.process_instance_id, !String(event.kind).startsWith("network."));
    let description;
    if (creationKinds.has(event.kind) && valid(data.child_pid)) {
      const child = this.process(data.child_pid, data.child_process_instance_id);
      if (child !== node) {
        child.parentId = node.id;
        child.birth = event.kind.replace("process.", "");
        child.startedNs ??= time;
        if (!child.hasExec) child.name ??= node.name;
        node.children.add(child.id);
      }
      description = `${identity(data.pid)} → ${identity(data.child_pid)}`;
    } else if (event.kind === "process.exec") {
      node.startedNs ??= time;
      const previous = node.name;
      const argv = Array.isArray(data.argv) ? data.argv : [];
      node.hasExec = true;
      node.name = argv[0] || null;
      description = `${previous || "exec"} → ${argv.length ? formatCommand(argv) : "arguments unavailable"}`;
    } else if (event.kind === "process.exit") {
      node.state = "exited";
      node.endedNs = time;
      node.exit = data.signal ? `signal ${data.signal}` : `exit ${data.exit_code ?? "?"}`;
      description = node.exit;
      if (this.active.get(node.pid) === node) this.active.delete(node.pid);
    } else {
      const domain = connectionKinds.has(event.kind) && valid(data.connection_id)
        ? this.entries.findLast(entry => entry.event.kind === "network.protocol" && entry.event.data.connection_id === data.connection_id)?.event.data.domain : null;
      description = describeBehavior(event.kind, data, domain);
    }
    if (event.kind === "network.protocol" && valid(data.connection_id)) {
      for (const entry of this.entries) {
        if (connectionKinds.has(entry.event.kind) && entry.event.data.connection_id === data.connection_id)
          entry.description = describeBehavior(entry.event.kind, entry.event.data, data.domain);
      }
    }
    const entry = { id: ++this.count, event, description, nodeId: node.id };
    node.events.push(entry);
    this.entries.push(entry);
    while (this.entries.length > this.limit) {
      const first = this.entries.shift();
      const owner = this.nodes.get(first.nodeId);
      owner.events.shift();
      owner.omitted++;
      this.prune(owner);
    }
  }

  processView() {
    if (this.legacy) return this.legacy.processView();
    return { nodes: new Map([...this.nodes].map(([id, node]) => [id, { ...node, activity: 0 }])), entries: this.entries };
  }

  prune(node) {
    while (node && node.events.length === 0 && node.children.size === 0 && node.state !== "running") {
      this.nodes.delete(node.id);
      if (this.byInstance.get(node.instanceId) === node) this.byInstance.delete(node.instanceId);
      if (this.latest.get(node.pid) === node) this.latest.delete(node.pid);
      if (this.active.get(node.pid) === node) this.active.delete(node.pid);
      const parent = this.nodes.get(node.parentId);
      parent?.children.delete(node.id);
      node = parent;
    }
  }
}
