import test from "node:test";
import assert from "node:assert/strict";
import { ProcessTree } from "../src/playground/assets/process-tree.mjs";
import { EventFilter } from "../src/playground/assets/event-filter.mjs";
import { activitySnapshot } from "../src/playground/assets/activity-model.mjs";

function fixture(limit = 1000) {
  const tree = new ProcessTree(limit), filter = new EventFilter();
  let sequence = 0;
  const add = (kind, data) => tree.add({ schema_version: 2, sequence: ++sequence, monotonic_ns: sequence * 1e6, observed_unix_ns: sequence * 1e6, kind, data });
  tree.start(10);
  add("process.exec", { pid: 10, process_instance_id: 1, argv: ["bash"] });
  add("process.fork", { pid: 10, process_instance_id: 1, child_pid: 20, child_process_instance_id: 2 });
  add("process.exec", { pid: 20, process_instance_id: 2, argv: ["curl"] });
  return { tree, filter, add };
}

test("shared filtering retains process lineage and marks only matching descendant activity", () => {
  const { tree, filter, add } = fixture();
  add("file.open", { pid: 20, process_instance_id: 2, path: "/tmp/a" });
  add("network.upstream", { pid: 20, process_instance_id: 2, destination: "192.0.2.1:443" });
  filter.set("*", false);
  filter.set("file.*", true);
  const view = activitySnapshot(tree, filter);
  const parent = [...view.nodes.values()].find(n => n.pid === 10);
  const child = [...view.nodes.values()].find(n => n.pid === 20);
  assert.equal(child.parentId, parent.id);
  assert.deepEqual(view.visible.map(e => e.event.sequence), [4]);
  assert.equal(view.counts.get(child.id), 1);
  assert.equal(parent.activity, 4);
  assert.equal(child.activity, 4);
  assert.equal(child.name, "curl");
  filter.set("*", false);
  const empty = activitySnapshot(tree, filter);
  assert.equal(empty.nodes.size, 2);
  assert.equal(empty.visible.length, 0);
  assert.equal([...empty.nodes.values()].every(n => n.activity === 0), true);
});

test("late transfer cards keep their start and exited PID lifetime after reuse", () => {
  const { tree, filter, add } = fixture();
  add("process.exit", { pid: 20, process_instance_id: 2, exit_code: 0 });
  add("process.exec", { pid: 20, process_instance_id: 3, argv: ["next"] });
  add("network.transfer", { pid: 20, process_instance_id: 2, started_unix_ns: 3500000, bytes_up: 2, bytes_down: 8 });
  const view = activitySnapshot(tree, filter);
  assert.deepEqual(view.visible.map(e => e.event.sequence), [1, 2, 3, 6, 4, 5]);
  const transfer = view.visible.find(e => e.event.kind === "network.transfer");
  assert.equal(view.nodes.get(transfer.nodeId).name, "curl");
  assert.equal(view.nodes.get(transfer.nodeId).state, "exited");
  assert.equal(view.nodes.size, 3);
  assert.deepEqual(tree.entries.map(e => e.event.sequence), [1, 2, 3, 4, 5, 6]);
});

test("graph and list snapshots use the same bounded evidence window and necessary ancestors", () => {
  const { tree, filter, add } = fixture(3);
  add("process.exit", { pid: 10, process_instance_id: 1, exit_code: 0 });
  for (let i = 0; i < 5; i++) add("file.open", { pid: 20, process_instance_id: 2, path: `/tmp/${i}` });
  const view = activitySnapshot(tree, filter);
  assert.equal(view.entries.length, 3);
  assert.equal(view.visible.length, 3);
  const parent = [...view.nodes.values()].find(n => n.pid === 10);
  assert.equal(parent.omitted, 3);
  assert.equal(parent.state, "exited");
  assert.equal(view.nodes.size, 2);
  assert.deepEqual(view.visible.map(e => e.event.data.path), ["/tmp/2", "/tmp/3", "/tmp/4"]);
});

test("shared projection folds lifecycle into its process and preserves distinct calls to the same file", () => {
  const { tree, filter, add } = fixture();
  add("file.open", { pid: 20, process_instance_id: 2, path: "/tmp/a", result: 3 });
  add("file.open", { pid: 20, process_instance_id: 2, path: "/tmp/a", result: 4 });
  add("memory.mmap", { pid: 20, process_instance_id: 2, length: 4096 });
  add("process.exit", { pid: 20, process_instance_id: 2, exit_code: 0 });
  const snapshot = activitySnapshot(tree, filter), graph = snapshot;
  assert.deepEqual(graph.behaviors.map(card => card.event.kind), ["file.open", "file.open", "memory.mmap"]);
  const child = [...graph.nodes.values()].find(node => node.pid === 20);
  assert.deepEqual(graph.processRecords.get(child.id).map(entry => entry.event.kind), ["process.exec", "process.exit"]);
  assert.equal(child.state, "exited");
  assert.equal(snapshot.visible.length, 7, "raw list and evidence are unchanged");
  filter.set("process.*", false);
  const filtered = activitySnapshot(tree, filter);
  assert.equal(filtered.processRecords.size, 0);
  assert.equal(filtered.nodes.size, 2, "process context remains for visible file calls");
});

test("every tracking view receives the same grouped behaviors and logical counts", () => {
  const { tree, filter, add } = fixture();
  const network = (kind, data = {}) => add(kind, { pid: 20, process_instance_id: 2, connection_id: 1, ...data });
  network("network.upstream", { status: "connected" });
  network("network.http.request", { request_id: 1, started_unix_ns: 5e6, method: "GET", url: "/a" });
  network("network.http.request", { request_id: 2, started_unix_ns: 6e6, method: "GET", url: "/b" });
  const before = activitySnapshot(tree, filter);
  assert.equal(before.behaviors?.length, 1, "shared projection must contain one TCP parent");
  assert.equal(before.behaviorCount, 3, "TCP + two HTTP exchanges, not raw stages");
  network("network.http.response", { request_id: 1, started_unix_ns: 5e6, status_code: 200 });
  network("network.http.response.body", { request_id: 1, started_unix_ns: 5e6, body: { observed_bytes: 10, complete: true } });
  network("network.transfer", { started_unix_ns: 4e6, status: "closed", bytes_down: 100 });
  add("process.exit", { pid: 20, process_instance_id: 2, exit_code: 0 });
  const after = activitySnapshot(tree, filter), [flow] = after.behaviors;
  assert.equal(after.behaviorCount, 3, "updates must not inflate the visible count");
  assert.equal(after.behaviorCounts.get(flow.nodeId), 3);
  assert.equal(flow.id, before.behaviors[0].id);
  assert.deepEqual(flow.children.map(child => child.requestId), [1, 2]);
  assert.match(flow.children[0].description, /200/);
  assert.equal(after.entries.length, 10, "raw evidence is retained separately");
  filter.set("network.*", false);
  const hidden = activitySnapshot(tree, filter);
  assert.equal(hidden.behaviorCount, 0);
  assert.equal(hidden.totalBehaviorCount, 3);
  assert.equal(hidden.nodes.size, 2);
});
