import assert from "node:assert/strict";
import { test } from "node:test";
import { AgentModel, capturedTime, elapsedTime, isFinalMessage } from "../src/playground/assets/agent-model.mjs";

test("live and interrupted durations stay on the server timeline when browser clocks differ", t => {
  let local = 10_000_000;
  t.mock.method(Date, "now", () => local);
  for (const skew of [-3_600_000, 3_600_000]) {
    const model = new AgentModel();
    const remote = local + skew;
    const add = (kind, at, fields = {}) => model.add({kind,
      observed_unix_ns:at * 1e6, data:{task_id:"t", captured_unix_ns:at * 1e6, ...fields}});
    add("agent.task.started", remote);
    const task = model.task("t");
    local += 300;
    add("agent.tool.call", remote + 300, {agenttomo_toolcall_id:"tool"});
    const tool = task.tools.get("tool");
    local += 900;
    assert.equal(elapsedTime(task, task.started), 1200);
    assert.equal(elapsedTime(task, tool.started), 900);
    model.end();
    local += 50_000;
    assert.equal(elapsedTime(task, tool.started), 900);
  }
});

test("completion keeps the streamed answer while its full payload is unavailable", () => {
  const model = new AgentModel();
  const prefix = "文".repeat(4096), answer = prefix + " — the ending";
  const add = (kind,data) => model.add({kind,data:{task_id:"t",item_id:"m",...data}});
  add("agent.message.delta", {delta:answer});
  add("agent.message", {preview:prefix, text_bytes:new TextEncoder().encode(answer).length, payload:{status:"unavailable"}});
  assert.equal(model.task("t").messages.get("m").text,answer);
  add("agent.message", {preview:"文",text_bytes:3});
  assert.equal(model.task("t").messages.get("m").text,"文","a shorter authoritative answer wins");
});

test("a retry diagnostic becomes historical when a later attempt makes progress", () => {
  const model = new AgentModel();
  const add = (kind,data={}) => model.add({kind,data:{task_id:"t",request_id:1,...data}});
  const task = model.task("t");
  add("agent.task.diagnostic",{error:"Reconnecting 1/5"});
  assert.equal(task.activeDiagnostic?.data.error,"Reconnecting 1/5");
  add("agent.model.request");
  assert.equal(task.phase,"Waiting for model");
  assert.equal(task.activeDiagnostic,null);
  assert.equal(task.errors.length,1);
  add("agent.task.diagnostic",{error:"Runtime stopped responding"});
  assert.equal(task.phase,"Agent reported a problem");
  add("agent.tool.call",{agenttomo_toolcall_id:"tool",name:"shell"});
  assert.equal(task.phase,"Running shell");
  assert.equal(task.activeDiagnostic,null);
});

test("legacy previews preserve longer deltas and a skipped delta keeps its gap visible", () => {
  const model = new AgentModel(), prefix = "🌤".repeat(4096);
  const add = (kind,data) => model.add({kind,data:{task_id:"t",item_id:"m",...data}});
  add("agent.message.delta",{delta:prefix + " ending"});
  add("agent.message",{preview:prefix,payload:{status:"unavailable"}});
  assert.equal(model.task("t").messages.get("m").text,prefix + " ending");

  add("agent.message.delta",{item_id:"gap",delta:prefix,delta_truncated:true});
  add("agent.message.delta",{item_id:"gap",delta:"tail after the gap"});
  add("agent.message",{item_id:"gap",preview:prefix,text_bytes:65536,payload:{status:"unavailable",error:"budget exhausted"}});
  const incomplete = model.task("t").messages.get("gap");
  assert.equal(incomplete.text,prefix);
  assert.equal(incomplete.partial,true);
  assert.equal(incomplete.captureError,"budget exhausted");
});

test("normalized message kinds override protocol fields and survive streaming completion", () => {
  const model = new AgentModel(); let sequence = 0;
  const add = (kind,data={}) => model.add({kind,sequence:++sequence,data:{task_id:"t",request_id:1,...data}});
  add("agent.message.delta",{item_id:"answer",delta:"Answer"});
  const message = model.task("t").messages.get("answer");
  assert.equal(isFinalMessage(model.task("t"),message),false);
  add("agent.message",{item_id:"answer",message_kind:"final",phase:"commentary",preview:"Full answer"});
  assert.equal(model.task("t").messages.get("answer"),message,"completion updates the existing entry");
  assert.equal(isFinalMessage(model.task("t"),message),true);
  assert.equal(message.event.data.phase,"commentary","raw protocol evidence is retained");
  add("agent.message",{item_id:"progress",message_kind:"intermediate",phase:"final_answer",preview:"Progress"});
  add("agent.task.finished",{exit_code:0});
  assert.equal(isFinalMessage(model.task("t"),model.task("t").messages.get("progress")),false);
});

test("unknown message semantics use the completed-task fallback without guessing during execution", () => {
  const model = new AgentModel(); let sequence = 0;
  const add = (kind,data={}) => model.add({kind,sequence:++sequence,data:{task_id:"t",request_id:1,...data}});
  add("agent.message",{item_id:"answer",message_kind:"unknown",phase:"final_answer",preview:"Reply"});
  const task = model.task("t"), message = task.messages.get("answer");
  assert.equal(isFinalMessage(task,message),false,"unknown does not read raw phase");
  add("agent.task.finished",{exit_code:0});
  assert.equal(isFinalMessage(task,message),true);
  add("agent.tool.call",{agenttomo_toolcall_id:"later-tool",name:"shell"});
  assert.equal(isFinalMessage(task,message),false,"a later tool means this was not the last answer");
  add("agent.message",{item_id:"later",message_kind:"unknown",preview:"Later reply"});
  add("agent.task.finished",{cancelled:true});
  assert.equal(isFinalMessage(task,task.messages.get("later")),false,"interruption cannot imply a final answer");
});

test("legacy message events keep their final-answer and fallback behavior", () => {
  for (const [data,running,completed] of [
    [{phase:"final_answer"},true,true],
    [{phase:"commentary"},false,false],
    [{phase:"other_legacy_phase"},false,false],
    [{},false,true],
  ]) {
    const model = new AgentModel();
    model.add({kind:"agent.message",sequence:1,data:{task_id:"t",item_id:"m",preview:"Reply",...data}});
    const task=model.task("t"), message=task.messages.get("m");
    assert.equal(isFinalMessage(task,message),running);
    model.add({kind:"agent.task.finished",sequence:2,data:{task_id:"t",exit_code:0}});
    assert.equal(isFinalMessage(task,message),completed);
  }
});
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
test("null timestamps stay unknown and late started events do not revive finished tasks", () => {
  assert.equal(Number.isNaN(capturedTime({ observed_unix_ns: null, data: { captured_unix_ns: null } })), true);
  const model = new AgentModel();
  model.add({ kind: "agent.task.finished", sequence: 2, observed_unix_ns: null, data: { task_id: "t", exit_code: 0 } });
  model.add({ kind: "agent.task.started", sequence: 1, observed_unix_ns: null, data: { task_id: "t", prompt: "Late start" } });
  const task = model.task("t");
  assert.equal(task.state, "completed");
  assert.equal(task.prompt, "Late start");
  assert.equal(model.runningTasks.size, 0);
  assert.equal(task.clockOffset, undefined);
});
