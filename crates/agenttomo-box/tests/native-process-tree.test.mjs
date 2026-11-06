import test from "node:test";
import assert from "node:assert/strict";
import { ProcessTree } from "../src/playground/assets/process-tree.mjs";

function feed(tree) {
  let sequence = 0;
  return (kind, data, source = "ptrace") => tree.add({ schema_version: 2, source, sequence: ++sequence, monotonic_ns: sequence * 1e6, kind, data });
}

test("native process records build lineage and behavior ownership using only PID", () => {
  const tree = new ProcessTree();
  const add = feed(tree);
  tree.start(35);
  add("process.exec", {pid:35,process_instance_id:1,argv:["curl"]});
  add("file.open", {pid:35,process_instance_id:1,path:"/etc/resolv.conf"});
  add("process.fork", {pid:35,parent_pid:35,process_instance_id:1,child_pid:39,child_process_instance_id:2});
  add("process.exec", {pid:39,process_instance_id:2,argv:["worker"]});
  add("network.upstream", {pid:39,process_instance_id:2,destination:"example:443",status:"connected"}, "tcp_proxy");
  const view = tree.processView();
  const nodes = [...view.nodes.values()];
  assert.equal(nodes.length, 2);
  const root = nodes.find(n=>n.pid===35), child=nodes.find(n=>n.pid===39);
  assert.equal(child.parentId, root.id);
  assert.equal(root.name,"curl");
  assert.equal(child.events.at(-1).event.kind,"network.upstream");
  assert.equal(view.entries.find(e=>e.event.kind==='process.fork').description,"PID 35 → PID 39");
});

test("late proxy records keep their PID lifetime when a later process reuses it", () => {
  const tree = new ProcessTree();
  const add = feed(tree);
  add("process.exec",{pid:35,process_instance_id:1,argv:["first"]});
  add("process.exit",{pid:35,process_instance_id:1,exit_code:0});
  add("process.exec",{pid:35,process_instance_id:2,argv:["second"]});
  add("network.transfer",{pid:35,process_instance_id:1,bytes_up:12,bytes_down:34},"tcp_proxy");
  const nodes=[...tree.processView().nodes.values()];
  assert.equal(nodes.length,2);
  assert.equal(nodes.find(n=>n.name==='first').state,'exited');
  assert.equal(nodes.find(n=>n.name==='first').events.at(-1).event.kind,'network.transfer');
  assert.equal(nodes.find(n=>n.name==='second').state,'running');
});

test("exec preserves the process identity and duration without thread concepts", () => {
  const tree = new ProcessTree();
  const add = feed(tree);
  add("process.exec",{pid:35,process_instance_id:1,argv:["before"]});
  add("process.exec",{pid:35,process_instance_id:1,argv:["after"]});
  add("process.exit",{pid:35,process_instance_id:1,exit_code:7});
  const nodes=[...tree.processView().nodes.values()];
  assert.equal(nodes.length,1);
  assert.equal(nodes[0].name,'after');
  assert.equal(nodes[0].startedNs,1e6);
  assert.equal(nodes[0].endedNs,3e6);
  assert.equal(nodes[0].exit,'exit 7');
  assert.equal(nodes[0].events[1].description,'before → after');
});
