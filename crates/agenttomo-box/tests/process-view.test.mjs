import test from "node:test";
import assert from "node:assert/strict";
import { LegacyProcessTree as ProcessTree } from "../src/playground/assets/legacy-process-tree.mjs";

function fixture(limit) {
  const tree = new ProcessTree(limit);
  let sequence = 0;
  const add = (kind, data) => tree.add({ sequence: ++sequence, monotonic_ns: sequence * 1e6, kind, data });
  add("process.exec", { pid: 35, tid: 35, task_id: 1, argv: ["curl"] });
  add("process.clone", { pid: 35, parent_tid: 35, child_pid: 35, child_tid: 36, task_id: 1, child_task_id: 2 });
  return { tree, add };
}

test("process view merges thread behaviors in sequence and hides thread lifecycle", () => {
  const { tree, add } = fixture();
  add("file.open", { pid: 35, tid: 36, task_id: 2, path: "/etc/resolv.conf" });
  add("network.upstream", { pid: 35, tid: 35, task_id: 1, destination: "example:443" });
  add("process.exit", { pid: 35, tid: 36, task_id: 2, exit_code: 0 });
  const view = tree.processView();
  assert.equal(view.nodes.size, 1);
  const process = [...view.nodes.values()][0];
  assert.equal(process.pid, 35);
  assert.equal(process.state, "running");
  assert.deepEqual(process.events.map(e => e.event.kind), ["process.exec", "file.open", "network.upstream"]);
  assert.deepEqual(view.entries.map(e => e.event.sequence), [1, 3, 4]);
  assert.equal(view.entries[1].event.data.tid, 36, "raw evidence retains its thread identity");
});

test("a process remains running until all known threads exit", () => {
  const { tree, add } = fixture();
  add("process.exit", { pid: 35, tid: 35, task_id: 1, exit_code: 0 });
  let process = [...tree.processView().nodes.values()][0];
  assert.equal(process.state, "running");
  assert.equal(process.events.some(e => e.event.kind === "process.exit"), false);
  add("process.exit", { pid: 35, tid: 36, task_id: 2, exit_code: 0 });
  process = [...tree.processView().nodes.values()][0];
  assert.equal(process.state, "exited");
  assert.equal(process.endedNs, 4e6);
  assert.equal(process.events.filter(e => e.event.kind === "process.exit").length, 1);
});

test("a subprocess forked by a worker thread belongs directly to the process", () => {
  const { tree, add } = fixture();
  add("process.fork", { pid: 35, parent_tid: 36, child_pid: 40, child_tid: 40, task_id: 2, child_task_id: 3 });
  add("process.exec", { pid: 40, tid: 40, task_id: 3, argv: ["worker"] });
  const view = tree.processView();
  const nodes = [...view.nodes.values()];
  assert.equal(nodes.length, 2);
  assert.equal(nodes.find(n => n.pid === 40).parentId, nodes.find(n => n.pid === 35).id);
  assert.equal(view.entries.find(e => e.event.kind === "process.fork").description, "PID 35 → PID 40");
});

test("PID reuse and late thread proxy records stay in their original process lifetime", () => {
  const { tree, add } = fixture();
  add("process.exit", { pid: 35, tid: 36, task_id: 2, exit_code: 0 });
  add("process.exit", { pid: 35, tid: 35, task_id: 1, exit_code: 0 });
  add("process.exec", { pid: 35, tid: 35, task_id: 3, argv: ["next"] });
  add("network.transfer", { pid: 35, tid: 36, task_id: 2, bytes_up: 10, bytes_down: 20 });
  const view = tree.processView();
  const nodes = [...view.nodes.values()];
  assert.equal(nodes.length, 2);
  assert.equal(nodes.find(n => n.name === "curl").events.at(-1).event.kind, "network.transfer");
  assert.equal(nodes.find(n => n.name === "curl").state, "exited");
  assert.equal(nodes.find(n => n.name === "next").state, "running");
});

test("nonleader exec updates one process and keeps its name after thread history is pruned", () => {
  const { tree, add } = fixture(4);
  add("process.exit", { pid: 35, tid: 35, task_id: 1, exit_code: 0 });
  add("process.exec", { pid: 35, tid: 35, former_tid: 36, task_id: 2, argv: ["replacement"] });
  let process = [...tree.processView().nodes.values()][0];
  assert.equal(tree.processView().nodes.size, 1);
  assert.equal(process.name, "replacement");
  assert.equal(process.state, "running");
  add("process.fork", { pid: 35, parent_tid: 35, child_pid: 40, child_tid: 40, task_id: 2, child_task_id: 3 });
  add("process.exit", { pid: 35, tid: 35, task_id: 2, exit_code: 7 });
  for (let i = 0; i < 6; i++) add("file.open", { pid: 40, tid: 40, task_id: 3, path: "/a" });
  process = [...tree.processView().nodes.values()].find(n => n.pid === 35);
  assert.equal(process.name, "replacement");
  assert.equal(process.exit, "exit 7");
  assert.equal(process.endedNs, 6e6);
});

test("pruning a finished thread retains the final process exit while descendants remain", () => {
  const { tree, add } = fixture(3);
  add("process.fork", { pid: 35, parent_tid: 35, child_pid: 40, child_tid: 40, task_id: 1, child_task_id: 3 });
  add("process.exit", { pid: 35, tid: 35, task_id: 1, exit_code: 0 });
  add("process.exit", { pid: 35, tid: 36, task_id: 2, exit_code: 0 });
  for (let i = 0; i < 4; i++) add("file.open", { pid: 40, tid: 40, task_id: 3, path: "/a" });
  assert.equal(tree.byTask.has(2), false, "finished threads must be reclaimable");
  const process = [...tree.processView().nodes.values()].find(n => n.pid === 35);
  assert.equal(process.state, "exited");
  assert.equal(process.endedNs, 5e6);
  assert.equal(process.exit, "exit 0");
});
