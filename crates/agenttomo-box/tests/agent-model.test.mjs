import assert from "node:assert/strict";
import { test } from "node:test";
import { AgentModel } from "../src/playground/assets/agent-model.mjs";
test("chat keeps first-observed message and tool order while later updates reuse their entries", () => {
  const model = new AgentModel(); let sequence = 0;
  const add = (kind,data={}) => model.add({kind,sequence:++sequence,data:{task_id:"t",request_id:1,...data}});
  add("agent.message.delta",{item_id:"before",delta:"Checking"});
  add("agent.tool.started",{agenttomo_toolcall_id:"tool",name:"shell"});
  add("agent.message.delta",{item_id:"after",delta:"Result"});
  add("agent.tool.call",{agenttomo_toolcall_id:"tool",payload:{status:"complete"}});
  add("agent.tool.result",{agenttomo_toolcall_id:"tool",request_id:2,preview:"done"});
  add("agent.message",{item_id:"before",preview:"Checking now"});
  assert.deepEqual(model.task("t").timeline,[{type:"message",id:"before"},{type:"tool",id:"tool"},{type:"message",id:"after"}]);
  assert.equal(model.task("t").messages.get("before").text,"Checking now");
  assert.equal(model.tools.get("tool").output.data.preview,"done");
});
test("a task exposes its phase while waiting for a model or tool", () => {
  const model = new AgentModel(); let sequence = 0;
  const add = (kind, data = {}) => model.add({kind,sequence:++sequence,data:{task_id:"t",...data}});
  add("agent.task.started", {prompt:"Test"});
  assert.equal(model.task("t").phase, "Connecting to model");
  add("agent.model.request", {request_id:1});
  assert.equal(model.task("t").phase, "Waiting for model");
  add("agent.tool.call", {request_id:1,agenttomo_toolcall_id:"tool",name:"exec"});
  assert.equal(model.task("t").phase, "Running exec");
  add("agent.message.delta", {request_id:2,item_id:"message",delta:"Hello"});
  assert.equal(model.task("t").phase, "Receiving reply");
});
test("live runtime errors are retained once and do not finish a retrying task", () => {
  const model = new AgentModel();
  model.add({kind:"agent.task.diagnostic",sequence:1,data:{task_id:"t",error:"Reconnecting 1/5. HTTP 401 Unauthorized"}});
  assert.equal(model.task("t").errors.length,1);
  assert.equal(model.task("t").state,"running");
  model.add({kind:"agent.task.finished",sequence:2,data:{task_id:"t",exit_code:1,error:"Reconnecting 1/5. HTTP 401 Unauthorized"}});
  assert.equal(model.task("t").errors.length,1);
  assert.equal(model.task("t").state,"failed");
});
test("one model tool has complete arguments and result while resources retain process binding", () => {
  const model = new AgentModel(); let sequence = 0;
  const add = (kind, data) => model.add({ kind, sequence: ++sequence, data: { task_id: "t", ...data } });
  add("agent.model.request", { request_id: 1 });
  add("agent.tool.started", { agenttomo_toolcall_id: "tool1", name: "exec" });
  add("agent.tool.call", { agenttomo_toolcall_id: "tool1", payload: { offset: 0, captured_bytes: 100000 } });
  add("process.exec", { pid: 35, process_instance_id: 5, agent: { task_id: "t", agenttomo_toolcall_id: "tool1", association: "inferred_active_call" } });
  add("agent.model.request", { request_id: 2 });
  add("agent.tool.result", { agenttomo_toolcall_id: "tool1", payload: { offset: 100000, captured_bytes: 200000 } });
  assert.equal(model.tools.size, 1);
  assert.equal(model.tools.get("tool1").arguments.data.payload.captured_bytes, 100000);
  assert.equal(model.tools.get("tool1").output.data.payload.captured_bytes, 200000);
  assert.equal(model.tasks.get("t").rounds[0].tools.size, 1);
  assert.equal(model.tasks.get("t").rounds[1].tools.size, 0);
  assert.equal(model.bindings.get(5).agenttomo_toolcall_id, "tool1");
});
test("out-of-order results and parallel tools keep separate stable nodes", () => {
  const model = new AgentModel();
  for (const [index, id] of ["a", "b"].entries()) model.add({ kind: "agent.tool.result", sequence: index, data: { task_id: "t", agenttomo_toolcall_id: id, preview: id } });
  model.add({ kind: "agent.tool.call", sequence: 3, data: { task_id: "t", agenttomo_toolcall_id: "a", name: "shell" } });
  assert.equal(model.tools.get("a").output.data.preview, "a");
  assert.equal(model.tools.get("a").name, "shell");
  assert.equal(model.tools.size, 2);
});
test("session completion marks unfinished tasks as interrupted", () => {
  const model = new AgentModel();
  model.task("running"); model.task("completed").state = "completed";
  model.end();
  assert.equal(model.task("running").state,"interrupted");
  assert.equal(model.task("completed").state,"completed");
});
test("overlapping model transports keep tools under their own model request", () => {
  const model = new AgentModel();
  for (const id of [1,2]) model.add({kind:"agent.model.request",sequence:id,data:{task_id:"t",request_id:id}});
  model.add({kind:"agent.tool.call",sequence:3,data:{task_id:"t",request_id:1,agenttomo_toolcall_id:"tool-a",name:"exec"}});
  assert.equal(model.tasks.get("t").rounds[0].tools.has("tool-a"),true);
  assert.equal(model.tasks.get("t").rounds[1].tools.size,0);
});
test("one WebSocket transport keeps successive response IDs separate", () => {
  const model = new AgentModel(); let sequence = 0;
  const add = (kind,data) => model.add({kind,sequence:++sequence,data:{task_id:"t",request_id:28,...data}});
  add("agent.model.request",{}); add("agent.model.started",{response_id:"a"});
  add("agent.model.request",{}); add("agent.model.started",{response_id:"b"});
  add("agent.tool.call",{response_id:"a",agenttomo_toolcall_id:"tool-a",name:"exec"});
  add("agent.tool.call",{response_id:"b",agenttomo_toolcall_id:"tool-b",name:"exec"});
  assert.deepEqual([...model.tasks.get("t").rounds[0].tools.keys()],["tool-a"]);
  assert.deepEqual([...model.tasks.get("t").rounds[1].tools.keys()],["tool-b"]);
});
