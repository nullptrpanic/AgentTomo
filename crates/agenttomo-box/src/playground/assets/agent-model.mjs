// Agent semantics are projected separately from process and resource records.
import { orderByStart } from "./event-time.mjs";

export function orderedRounds(task) {
  return orderByStart(task.rounds.map(round => ({
    event: round.request || round.started || round.first,
    id: round.first?.sequence || 0,
    round,
  }))).map(entry => entry.round);
}

export function isFinalMessage(task, message) {
  if (message.kind === "final") return true;
  if (message.kind === "intermediate") return false;
  const last = task.timeline.at(-1);
  return task.state === "completed" && last?.type === "message" && last.id === message.id;
}

function messageKind(data) {
  if (data.message_kind !== undefined) {
    return ["final", "intermediate"].includes(data.message_kind) ? data.message_kind : "unknown";
  }
  // Legacy journals predate message_kind. Convert their phase once at ingress,
  // preserving the old rule for any explicit non-final phase.
  return data.phase ? data.phase === "final_answer" ? "final" : "intermediate" : "unknown";
}

export class AgentModel {
  constructor() { this.name = "Agent"; this.tasks = new Map(); this.tools = new Map(); this.bindings = new Map(); }
  task(id) {
    if (!this.tasks.has(id)) this.tasks.set(id, { id, prompt: "", state: "running", phase: `Starting ${this.name}`, queuedAt: Date.now(), activity: 0, timeline: [], rounds: [], messages: new Map(), tools: new Map(), errors: [], current: null });
    return this.tasks.get(id);
  }
  add(event) {
    const d = event.data || {};
    if (d.agent && Number.isSafeInteger(d.process_instance_id)) this.bindings.set(d.process_instance_id, d.agent);
    if (!event.kind.startsWith("agent.") || !d.task_id) return;
    const task = this.task(d.task_id);
    task.activity = Math.max(task.activity, event.sequence || 0);
    if (event.kind === "agent.task.started") { task.prompt = d.prompt; task.started = event; task.state = "running"; task.phase = "Connecting to model"; }
    if (event.kind === "agent.task.finished") { task.state = d.cancelled ? "cancelled" : d.exit_code === 0 ? "completed" : "failed"; task.finished = event; }
    if (event.kind === "agent.task.rejected") { task.state = d.cancelled ? "cancelled" : "rejected"; task.finished = event; }
    if (d.error && !d.cancelled && ["agent.task.diagnostic", "agent.task.finished", "agent.task.rejected", "agent.capture.error"].includes(event.kind) && task.errors.at(-1)?.data.error !== d.error) {
      task.errors.push(event);
    }
    if (event.kind === "agent.task.diagnostic") task.phase = /reconnect|retry/i.test(d.error || "") ? "Retrying connection" : `${this.name} reported a problem`;
    if (event.kind === "agent.model.request") {
      const round = task.rounds.findLast(round => !round.request && round.requestId === d.request_id) || this.newRound(task,event);
      round.request = event; task.current = round;
      round.activity = Math.max(round.activity || 0, event.sequence || 0);
      task.phase = "Waiting for model";
    }
    if (event.kind === "agent.model.started") {
      const round = this.round(task,event);
      round.started = event; round.responseId = d.response_id;
      task.phase = "Receiving model response";
    }
    if (event.kind === "agent.model.complete") this.round(task,event).complete = event;
    if (event.kind.startsWith("agent.tool.")) {
      const id = d.agenttomo_toolcall_id;
      if (!id) return;
      let tool = this.tools.get(id);
      if (!tool) {
        tool = { id, taskId: task.id, name: d.name || "Tool call", external: d.call_id, started: event };
        this.tools.set(id, tool); task.tools.set(id, tool);
        task.timeline.push({type:"tool",id});
      }
      if (d.name) tool.name = d.name;
      tool.activity = Math.max(tool.activity || 0, event.sequence || 0);
      task.phase = event.kind === "agent.tool.result" ? "Waiting for model" : `Running ${tool.name}`;
      if (event.kind === "agent.tool.call") tool.arguments = event;
      if (event.kind === "agent.tool.result") tool.output = event;
      if (event.kind !== "agent.tool.result") {
        this.round(task,event).tools.set(id, tool);
      }
    }
    if (event.kind === "agent.message.delta" || event.kind === "agent.message") {
      const id = d.item_id || `message-${d.request_id}`;
      task.phase = "Receiving reply";
      let message = task.messages.get(id);
      if (!message) {
        message = { id, text: "", kind: messageKind(d), event, started: event };
        task.timeline.push({type:"message",id});
      }
      if (event.kind === "agent.message.delta") message.text += d.delta || "";
      else { message.text = d.preview ?? message.text; message.kind = messageKind(d); message.event = event; message.complete = true; }
      task.messages.set(id, message);
      message.activity = Math.max(message.activity || 0, event.sequence || 0);
      this.round(task,event).messages.set(id, message);
    }
  }
  round(task, event) {
    const d = event.data || {};
    const byResponse = d.response_id && task.rounds.findLast(round => round.responseId === d.response_id);
    const byRequest = d.request_id !== undefined && task.rounds.findLast(round => round.requestId === d.request_id && (!d.response_id || !round.responseId || round.responseId === d.response_id));
    const round = byResponse || byRequest || (d.request_id === undefined ? task.current : null) || this.newRound(task,event);
    round.activity = Math.max(round.activity || 0, event.sequence || 0);
    return round;
  }
  newRound(task, event) {
    const round = { id:`round-${event.sequence}`, first:event, requestId:event.data?.request_id, responseId:event.data?.response_id, tools:new Map(), messages:new Map() };
    task.rounds.push(round); task.current = round; return round;
  }
  end() {
    for (const task of this.tasks.values()) if (task.state === "running") {
      task.state = "interrupted"; task.endedAt = Date.now();
    }
  }
}
