import test from "node:test";
import assert from "node:assert/strict";
import { ProcessTree } from "../src/playground/assets/process-tree.mjs";

function feed(tree) {
  let sequence = 0;
  return (kind, data) => tree.add({ sequence: ++sequence, monotonic_ns: sequence * 1e6, kind, data });
}

test("fork builds lineage while repeated exec stays in the same node", () => {
  const tree = new ProcessTree();
  const event = feed(tree);
  tree.start(10);
  event("process.exec", { tid: 10, argv: ["/bin/bash"] });
  event("process.fork", { parent_tid: 10, child_tid: 11 });
  const child = tree.active.get(11);
  event("process.exec", { tid: 11, argv: ["/bin/sh", "-c", "exec /bin/echo hello"] });
  event("process.exec", { tid: 11, argv: ["/bin/echo", "hello"] });
  event("process.exit", { tid: 11, exit_code: 0 });
  assert.equal(tree.nodes.size, 2);
  assert.equal(tree.roots.length, 1);
  assert.equal(child.parentId, tree.roots[0].id);
  assert.equal(child.name, "/bin/echo");
  assert.equal(child.state, "exited");
  assert.equal(child.events.length, 3);
  assert.match(child.events[1].description, /\/bin\/sh → \/bin\/echo hello/);
});

test("a child observed first is attached when its creation event arrives", () => {
  const tree = new ProcessTree();
  const event = feed(tree);
  event("process.exec", { tid: 21, argv: ["worker"] });
  const child = tree.active.get(21);
  event("process.fork", { parent_tid: 20, child_tid: 21 });
  assert.equal(tree.nodes.size, 2);
  assert.equal(tree.roots[0].tid, 20);
  assert.equal(child.parentId, tree.roots[0].id);
  assert.equal(child.name, "worker");
});

test("late ready messages and unreadable exec do not invent a new root or image", () => {
  const tree = new ProcessTree();
  const event = feed(tree);
  event("process.exec", { tid: 10, argv: ["parent"] });
  event("process.exec", { tid: 11, capture: "unreadable" });
  event("process.fork", { parent_tid: 10, child_tid: 11 });
  assert.equal(tree.active.get(11).name, null);
  event("process.exit", { tid: 10, exit_code: 0 });
  tree.start(10);
  assert.equal(tree.nodes.size, 2);
  assert.equal(tree.roots[0].state, "exited");
  assert.equal(tree.roots[0].root, true);
});

test("reusing an exited TID creates a separate task history", () => {
  const tree = new ProcessTree();
  const event = feed(tree);
  event("process.fork", { parent_tid: 10, child_tid: 11 });
  event("process.exec", { tid: 11, argv: ["first"] });
  const first = tree.active.get(11);
  event("process.exit", { tid: 11, exit_code: 3 });
  event("process.fork", { parent_tid: 10, child_tid: 11 });
  event("process.exec", { tid: 11, argv: ["second"] });
  const second = tree.active.get(11);
  assert.notEqual(first.id, second.id);
  assert.equal(first.name, "first");
  assert.equal(second.name, "second");
  assert.equal(tree.roots[0].children.size, 2);
});

test("nonleader exec follows former_tid without merging the old leader history", () => {
  const tree = new ProcessTree();
  const event = feed(tree);
  event("process.exec", { tid: 10, argv: ["leader"] });
  const leader = tree.active.get(10);
  event("process.clone", { parent_tid: 10, child_tid: 11 });
  const thread = tree.active.get(11);
  event("process.exit", { tid: 10, exit_code: 0 });
  event("process.exec", { tid: 10, former_tid: 11, argv: ["replacement"] });
  assert.equal(tree.active.get(10), thread);
  assert.equal(tree.active.has(11), false);
  assert.equal(thread.parentId, leader.id);
  assert.equal(thread.tid, 10);
  assert.equal(leader.state, "exited");
  assert.equal(leader.name, "leader");
  event("process.exit", { tid: 10, exit_code: 2 });
  assert.equal(thread.state, "exited");
  assert.equal(leader.events.at(-1).event.data.exit_code, 0);
});

test("behaviors use their event TID; missing ownership stays unattributed", () => {
  const tree = new ProcessTree();
  const event = feed(tree);
  event("process.exec", { tid: 10, argv: ["worker"] });
  event("file.open", { tid: 10, path: "/tmp/input", result: 3 });
  event("network.connect", { tid: 10, destination: "127.0.0.1:8080" });
  event("file.open", { path: "/tmp/unknown" });
  assert.equal(tree.active.get(10).events.length, 3);
  assert.equal(tree.roots.find(node => node.tid === null).events.length, 1);
});

test("delayed proxy records retain their task after exit and TID reuse", () => {
  const tree = new ProcessTree();
  const event = feed(tree);
  tree.start(10);
  event("process.exec", { tid: 10, task_id: 1, argv: ["parent"] });
  event("process.fork", { parent_tid: 10, child_tid: 11, task_id: 1, child_task_id: 2 });
  const first = tree.active.get(11);
  event("process.exec", { tid: 11, task_id: 2, argv: ["curl"] });
  event("network.connect", { tid: 11, task_id: 2, connection_id: 1, destination: "127.0.0.1:80", redirected: true, result: 0 });
  event("process.exit", { tid: 11, task_id: 2, exit_code: 0 });
  event("process.fork", { parent_tid: 10, child_tid: 11, task_id: 1, child_task_id: 3 });
  const second = tree.active.get(11);
  event("network.transfer", { tid: 11, task_id: 2, connection_id: 1, destination: "127.0.0.1:80", status: "scope_finished", bytes_up: 42, bytes_down: null });
  assert.equal(first.events.at(-1).event.kind, "network.transfer");
  assert.equal(first.state, "exited");
  assert.equal(tree.active.get(11), second);
  assert.notEqual(first, second);
  assert.equal(tree.nodes.size, 3);
  assert.match(first.events.at(-1).description, /42/);
});

test("proxy records can precede process records without inventing a live reused TID", () => {
  const tree = new ProcessTree();
  const event = feed(tree);
  event("network.upstream", { tid: 11, task_id: 2, destination: "127.0.0.1:80", status: "connected" });
  const early = tree.roots[0];
  assert.equal(early.state, "unknown");
  event("process.fork", { parent_tid: 10, child_tid: 11, task_id: 1, child_task_id: 2 });
  event("process.exec", { tid: 11, task_id: 2, argv: ["curl"] });
  assert.equal(tree.active.get(11), early);
  assert.equal(early.state, "running");
  assert.equal(tree.nodes.size, 2);
  assert.equal(early.parentId, tree.roots[0].id);
});

test("pruned tasks receive late proxy records without attaching to a reused TID", () => {
  const tree = new ProcessTree(2);
  const event = feed(tree);
  event("process.exec", { tid: 11, task_id: 1, argv: ["first"] });
  event("process.exit", { tid: 11, task_id: 1, exit_code: 0 });
  event("process.exec", { tid: 11, task_id: 2, argv: ["second"] });
  event("file.open", { tid: 11, task_id: 2, path: "/input", result: 3 });
  const second = tree.active.get(11);
  assert.equal(tree.nodes.size, 1);
  event("network.transfer", { tid: 11, task_id: 1, status: "closed", bytes_up: 3, bytes_down: 4 });
  assert.equal(tree.active.get(11), second);
  assert.equal(second.events.some(entry => entry.event.kind === "network.transfer"), false);
  assert.equal(tree.roots.find(node => node !== second).state, "unknown");
});

test("file and TCP behavior descriptions show the observed outcomes", () => {
  const tree = new ProcessTree();
  const event = feed(tree);
  event("file.open", { tid: 1, path: "/missing", syscall: "openat", result: -2, errno: 2 });
  event("network.connect", { tid: 1, destination: "example:443", redirected: true, result_source: "proxy_endpoint", result: 0 });
  event("network.upstream", { tid: 1, destination: "example:443", status: "failed", error: "Connection refused" });
  assert.match(tree.entries[0].description, /\/missing.*errno 2/);
  assert.match(tree.entries[1].description, /proxy/);
  assert.match(tree.entries[2].description, /Connection refused/);
});

test("event retention preserves necessary ancestors and reclaims finished leaves", () => {
  const tree = new ProcessTree(3);
  const event = feed(tree);
  event("process.exec", { tid: 10, argv: ["root"] });
  event("process.fork", { parent_tid: 10, child_tid: 11 });
  event("process.exit", { tid: 10, exit_code: 0 });
  const parent = tree.roots[0];
  event("process.exec", { tid: 11, argv: ["child"] });
  event("file.open", { tid: 11, path: "/a" });
  event("file.open", { tid: 11, path: "/b" });
  assert.equal(parent.events.length, 0);
  assert.equal(parent.omitted, 3);
  assert.equal(tree.roots[0], parent);
  event("process.exit", { tid: 11, exit_code: 0 });
  event("process.exec", { tid: 20, argv: ["other"] });
  event("file.open", { tid: 20, path: "/a" });
  event("file.open", { tid: 20, path: "/b" });
  assert.equal(tree.nodes.size, 1);
  assert.equal(tree.roots[0].tid, 20);
  assert.equal(tree.entries.length, 3);
});
