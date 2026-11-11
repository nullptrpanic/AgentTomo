import { AgentModel, isFinalMessage, orderedRounds } from "./agent-model.mjs";
import { AgentChat } from "./agent-chat.mjs";
import { closeIcon, copyIcon, externalLinkIcon } from "./icons.mjs";
import { copyWithFeedback } from "./copy-text.mjs";
import { formatCommand } from "./event-description.mjs";
import { eventTime, orderByStart } from "./event-time.mjs";
import { ProcessTree } from "./process-tree.mjs";
import { activitySnapshot } from "./activity-model.mjs";

const el = (tag, name, text) => { const node = document.createElement(tag); node.className = name; if (text !== undefined) node.textContent = text; return node; };
const clipped = text => String(text || "").slice(0, 200);
const display = value => typeof value === "string" ? value : JSON.stringify(value, null, 2);
const chronological = entries => orderByStart(entries.map(([event, item]) => ({event:event || {},id:event?.sequence || 0,item}))).map(entry => entry.item);

// Keep existing elements in place: replacing a pressed button loses its click.
function reconcile(parent, nodes) {
  const retained = new Set(nodes);
  for (const child of [...parent.children]) if (!retained.has(child)) child.remove();
  for (const [index, node] of nodes.entries()) {
    if (parent.children[index] !== node) parent.insertBefore(node, parent.children[index] || null);
  }
}
function setText(node, value) { if (node.textContent !== value) node.textContent = value; }

export class AgentView {
  constructor({ root, chat, detail, load, onSelect, onRaw, reveal, filter }) {
    Object.assign(this, { root, chat, detail, load, onSelect, onRaw, reveal, filter });
    this.model = new AgentModel(); this.expanded = new Map(); this.control = null; this.ready = false; this.available = false;
    this.activity = new Map(); this.activitySequences = new Map(); this.branches = new Map();
    this.list = el("ol", "agent-task-roots");
    this.tree = new ProcessTree(20000);
    this.history = chat.querySelector(".agent-history");
    this.form = chat.querySelector("form"); this.prompt = chat.querySelector("textarea");
    this.send = chat.querySelector(".agent-send"); this.stop = chat.querySelector(".agent-stop");
    this.status = chat.querySelector(".agent-chat-status");
    this.chatHistory = new AgentChat({root:this.history,section:this.section.bind(this),
      onTool:id => { this.focus(id); this.showTool(this.model.tools.get(id)); },
      onMessage:message => { this.focus(`message-${message.id}`); this.showMessage(message); },
      onModel:(round,title) => { this.focus(round.id); this.showModel(round,title); },
      onTask:id => this.focus(`task-${id}`)});
    this.form.addEventListener("submit", event => { event.preventDefault(); this.submit(); });
    this.prompt.addEventListener("keydown", event => { if (event.key === "Enter" && !event.shiftKey && !event.isComposing) { event.preventDefault(); this.submit(); } });
    this.stop.addEventListener("click", () => this.cancel());
    this.dirty = true; this.selected = null;
    this.renderChat();
  }
  configure(message) {
    this.available = Boolean(message.agent_available);
    const metadata = message.agent || {};
    const name = typeof metadata.name === "string" && metadata.name.trim() ? metadata.name : "Agent";
    this.agentName = name;
    this.model.name = name;
    this.chatHistory.configure({name, avatar:typeof metadata.avatar === "string" ? metadata.avatar : ">_",
      authentication_hint:typeof metadata.authentication_hint === "string" ? metadata.authentication_hint : ""});
    this.prompt.setAttribute("aria-label", `Task for ${name}`);
    this.prompt.placeholder = `Ask ${name} to run a task in this sandbox…`;
    this.updateControls();
  }
  setReady(value, reason) {
    if (!value && this.ready) { this.model.end(); this.dirty = true; this.renderChat(); if (this.snapshot && !this.root.hidden) this.render(); }
    if (value) this.stopReason = null;
    else if (reason || this.ready) this.stopReason = reason || "Sandbox disconnected.";
    this.ready = value; this.updateControls();
  }
  setControl(control) { this.control = control; this.updateControls(); }
  updateControls() {
    const running = [...this.model.tasks.values()].find(task => task.state === "running");
    const busy = Boolean(running);
    this.prompt.disabled = !this.available || !this.control;
    this.send.disabled = !this.ready || this.prompt.disabled || busy;
    this.stop.disabled = !this.ready || !busy || !this.control || running.stopping;
    this.stop.textContent = running?.stopping ? "Stopping…" : "Stop";
    this.status.textContent = !this.ready && this.stopReason ? this.stopReason : !this.available ? `External ${this.agentName || "agent"} was not found on the server.` : !this.ready ? "Waiting for the sandbox" : busy ? "You can stop this task at any time" : "Enter to send · Shift+Enter for a new line";
    if (busy && !this.progressTimer) this.progressTimer = setInterval(() => this.updateProgress(), 1000);
    if (!busy && this.progressTimer) { clearInterval(this.progressTimer); this.progressTimer = null; }
    this.updateProgress();
  }
  updateProgress() {
    this.chatHistory.updateProgress();
    const task = [...this.model.tasks.values()].find(task => task.state === "running");
    if (task && this.ready) this.status.textContent = task.stopping ? "Stopping task…" : "You can stop this task at any time";
  }
  cancel() {
    const task = [...this.model.tasks.values()].find(task => task.state === "running");
    if (!task || task.stopping || !this.control) return;
    try {
      this.control.cancel(); task.stopping = true;
      this.dirty = true; this.renderChat(); this.updateControls();
    } catch (error) { this.status.textContent = error.message; }
  }
  submit() {
    const prompt = this.prompt.value;
    if (this.send.disabled || !prompt.trim()) return;
    if (new TextEncoder().encode(prompt).length > 8192) { this.status.textContent = "Prompt exceeds the 8192-byte limit."; return; }
    const id = Array.from(crypto.getRandomValues(new Uint8Array(16)), byte => byte.toString(16).padStart(2, "0")).join("");
    try {
      this.control.prompt(id, prompt);
      this.model.task(id).prompt = prompt;
      this.prompt.value = ""; this.dirty = true; this.renderChat(); this.updateControls(); this.reveal();
    } catch (error) { this.status.textContent = error.message; }
  }
  add(event) {
    this.model.add(event);
    const task = this.model.tasks.get(event.data?.task_id);
    if (["agent.task.finished", "agent.task.rejected"].includes(event.kind) && ["failed", "rejected"].includes(task?.state) && !this.prompt.value) this.prompt.value = task.prompt;
    if (!event.kind.startsWith("agent.") && event.data?.agent) this.tree.add(event);
    if (event.kind.startsWith("agent.")) { this.dirty = true; this.renderChat(); this.updateControls(); }
    if (this.selected && event.data?.agenttomo_toolcall_id === this.selected) {
      const tool = this.model.tools.get(this.selected);
      if (event.kind === "agent.tool.result" && this.abort && !this.abort.signal.aborted) {
        this.detail.querySelectorAll(".agent-content-section")[1]?.remove();
        this.section("Returned content", tool.output, this.abort.signal);
      } else this.showTool(tool);
    }
    if (event.kind === "agent.model.complete" && task?.rounds.some(round => round.id === this.selected && round.complete === event) && this.abort && !this.abort.signal.aborted) {
      this.detail.querySelectorAll(".agent-content-section")[1]?.remove();
      this.section("Model response", event, this.abort.signal);
    }
    if (event.kind === "agent.message" && event.data.payload?.status === "complete") {
      const task = this.model.tasks.get(event.data.task_id), message = task?.messages.get(event.data.item_id);
      if (message) this.load(event.data.payload).then(value => {
        message.text = display(value); this.dirty = true; this.renderChat();
        if (this.snapshot && !this.root.hidden) this.render(this.snapshot);
      }).catch(error => { message.captureError = error.message; });
    }
  }
  renderChat() {
    if (!this.dirty) return;
    this.dirty = false;
    this.chatHistory.render(this.model.tasks);
  }
  branch(id, category, title, meta, children, select, open = true) {
    let record = this.branches.get(id);
    if (!record) {
      const item = el("li", "agent-branch"); item.dataset.agentNode = id;
      const card = el("div", "agent-card");
      const toggle = el("button", "agent-fold"); toggle.type = "button";
      const content = el("button", "agent-node-content"); content.type = "button";
      const label = el("span", "agent-label"), name = el("strong", ""), metadata = el("span", "agent-node-meta");
      content.append(label, name, metadata);
      const inspect = el("button", "agent-icon agent-node-inspect"); inspect.type = "button";
      inspect.innerHTML = externalLinkIcon; inspect.title = "Inspect evidence"; inspect.setAttribute("aria-label", inspect.title);
      card.append(toggle, content, inspect);
      const list = el("ol", "agent-children"); item.append(card, list);
      record = {item,card,toggle,content,label,name,metadata,inspect,list};
      this.branches.set(id, record);
      const fold = () => {
        if (!record.hasChildren) return;
        const expanded = record.list.hidden;
        this.expanded.set(id, expanded);
        this.updateBranch(record, expanded);
      };
      toggle.addEventListener("click", fold);
      content.addEventListener("click", fold);
      inspect.addEventListener("click", () => record.select?.());
      card.addEventListener("animationend", event => { if (event.animationName === "agent-activity") card.classList.remove("has-activity"); });
    }
    const {item,card,toggle,content,label,name,metadata,inspect} = record;
    Object.assign(record, {children,select,hasChildren:typeof children === "function" || children.length > 0});
    card.dataset.category = category;
    setText(label, category); setText(name, title); name.title = title;
    setText(metadata, meta || "");
    content.title = record.hasChildren ? "Expand or collapse" : "";
    inspect.disabled = !select;
    inspect.title = select ? "Inspect evidence" : "Evidence not captured yet";
    inspect.setAttribute("aria-label", inspect.title);
    const previous = this.activity.get(id);
    const sequence = this.activitySequences.get(id) || 0;
    this.activity.set(id, {sequence:Math.max(previous?.sequence || 0, sequence),
      changedAt:previous && sequence > previous.sequence ? performance.now() : previous?.changedAt ?? null});
    this.paintActivity(card, id);
    const expanded = this.expanded.get(id) ?? (record.hasChildren && open);
    this.updateBranch(record, expanded);
    return item;
  }
  updateBranch(record, expanded) {
    const {list,toggle,content,children,hasChildren} = record;
    expanded = hasChildren && expanded;
    if (expanded || !hasChildren) {
      reconcile(list, typeof children === "function" ? children() : children);
      for (const branch of list.querySelectorAll(".agent-branch")) this.paintActivity(branch.querySelector(":scope > .agent-card"), branch.dataset.agentNode);
    }
    list.hidden = !expanded;
    toggle.disabled = content.disabled = !hasChildren;
    setText(toggle, expanded ? "⌄" : "›");
    for (const button of [toggle, content]) {
      if (hasChildren) button.setAttribute("aria-expanded", String(expanded));
      else button.removeAttribute("aria-expanded");
    }
    toggle.setAttribute("aria-label", expanded ? "Collapse" : "Expand");
  }
  paintActivity(card, id) {
    const activity = this.activity.get(id);
    const elapsed = activity?.changedAt == null ? Infinity : performance.now() - activity.changedAt;
    card.classList.toggle("has-activity", elapsed < 800);
    if (elapsed < 800) card.style.animationDelay = `${-elapsed}ms`;
  }
  render() {
    const snapshot = activitySnapshot(this.tree, this.filter);
    this.snapshot = snapshot;
    const follow = this.root.scrollHeight - this.root.scrollTop - this.root.clientHeight < 70;
    const bindings = this.model.bindings;
    const processes = [...snapshot.nodes.values()].filter(node => bindings.has(node.instanceId));
    const sequences = new Map(snapshot.visible.map(entry => [entry.id, entry.event.sequence || 0]));
    const taskActivity = new Map();
    this.activitySequences = new Map();
    for (const node of processes) {
      const binding = bindings.get(node.instanceId), sequence = sequences.get(node.activity) || 0;
      this.activitySequences.set(`pid-${node.instanceId}`, sequence);
      taskActivity.set(binding.task_id, Math.max(taskActivity.get(binding.task_id) || 0, sequence));
      if (binding.agenttomo_toolcall_id) this.activitySequences.set(binding.agenttomo_toolcall_id, Math.max(this.activitySequences.get(binding.agenttomo_toolcall_id) || 0, sequence));
    }
    const processById = new Map(processes.map(node => [node.id, node]));
    const resources = new Map();
    for (const entry of snapshot.behaviors) {
      if (!resources.has(entry.nodeId)) resources.set(entry.nodeId, []);
      resources.get(entry.nodeId).push(entry);
    }
    const resource = entry => {
      this.activitySequences.set(`resource-${entry.id}`, sequences.get(entry.activity ?? entry.id) || entry.event.sequence || 0);
      return this.branch(`resource-${entry.id}`, entry.event.category || entry.event.kind.split(".")[0], entry.title || clipped(entry.description), entry.type ? entry.description : entry.event.kind,
        (entry.children || []).map(resource), () => { this.closeDetail(); this.onSelect(entry.event); });
    };
    const process = node => {
      const binding = bindings.get(node.instanceId);
      const argv = [...node.events].reverse().find(entry => entry.event.kind === "process.exec")?.event.data.argv || [node.name || "Process"];
      const childProcesses = processes.filter(child => child.parentId === node.id && bindings.get(child.instanceId)?.agenttomo_toolcall_id === binding.agenttomo_toolcall_id);
      const children = () => {
        const entries = (resources.get(node.id) || []).map(entry => ({ time: entry.started ?? eventTime(entry.event), item: resource(entry) }));
        for (const child of childProcesses) entries.push({ time: child.events.length ? eventTime(child.events[0].event) : 0, item: process(child) });
        entries.sort((a, b) => (a.time || 0) - (b.time || 0));
        return entries.map(entry => entry.item);
      };
      const evidence = [...node.events].reverse().find(entry => entry.event.kind === "process.exec")?.event || node.events[0]?.event;
      const inspect = evidence ? () => { this.closeDetail(); this.onSelect(evidence); } : null;
      const branch = this.branch(`pid-${node.instanceId}`, "process", formatCommand(argv), `PID ${node.pid} · ${node.exit || node.state} · ${binding.association}`, resources.get(node.id)?.length || childProcesses.length ? children : [], inspect, false);
      if (binding.association.includes("inferred")) branch.classList.add("agent-inferred");
      return branch;
    };
    const roots = (task, toolId) => processes.filter(node => {
      const binding = bindings.get(node.instanceId), parent = processById.get(node.parentId);
      return binding.task_id === task.id && (binding.agenttomo_toolcall_id || null) === toolId && (!parent || bindings.get(parent.instanceId)?.agenttomo_toolcall_id !== binding.agenttomo_toolcall_id);
    }).map(node => [node.events[0]?.event, process(node)]);
    const tasks = [];
    for (const task of this.model.tasks.values()) {
      const rounds = orderedRounds(task).map((round, index) => {
        const children = [];
        let activity = round.activity || 0;
        for (const tool of round.tools.values()) {
          this.activitySequences.set(tool.id, Math.max(tool.activity || 0, this.activitySequences.get(tool.id) || 0));
          activity = Math.max(activity, this.activitySequences.get(tool.id));
          const outcome = tool.output?.data.payload?.status === "complete" ? "result captured" : tool.output ? "complete return unavailable" : task.state === "running" ? "waiting for model-visible result" : "model-visible result unavailable";
          children.push([tool.started, this.branch(tool.id, "tool", tool.name, `${tool.external || tool.id} · ${outcome}`, chronological(roots(task, tool.id)), () => this.showTool(tool))]);
        }
        for (const message of round.messages.values()) {
          if (isFinalMessage(task, message)) continue;
          this.activitySequences.set(`message-${message.id}`, message.activity || 0);
          children.push([message.started, this.branch(`message-${message.id}`, "message", clipped(message.text) || "Assistant response", message.complete ? "Model response" : "Streaming", [], () => this.showMessage(message))]);
        }
        const event = round.request || round.started || round.first;
        const state = round.complete ? round.complete.data.status || "complete" : task.state === "running" ? "streaming" : "ended";
        const title = `Model call ${index + 1}`;
        this.activitySequences.set(round.id, activity);
        return [event, this.branch(round.id, "model", title, `${round.request?.data.model || round.started?.data.model || "Unknown model"} · ${state}`, chronological(children), () => this.showModel(round, title))];
      });
      rounds.push(...roots(task, null));
      for (const error of task.errors) rounds.push([error, this.branch(`error-${error.sequence}`, "error", "Capture or execution error", error.data.error, [], () => this.onRaw(error))]);
      this.activitySequences.set(`task-${task.id}`, Math.max(task.activity || 0, taskActivity.get(task.id) || 0));
      const evidence = task.started || task.finished;
      tasks.push([task.started, this.branch(`task-${task.id}`, "task", task.prompt || "Observed task", task.state, chronological(rounds), evidence ? () => this.onRaw(evidence) : null)]);
    }
    const list = this.list;
    reconcile(list, chronological(tasks));
    for (const id of this.activity.keys()) {
      if (!this.activitySequences.has(id)) this.activity.delete(id);
    }
    if (this.tree.total > this.tree.limit) list.prepend(el("p", "agent-detail-note", `Showing the latest ${this.tree.limit.toLocaleString()} resource records. Complete events remain in the session journal.`));
    this.empty ||= el("p", "agent-welcome", "Send a task from Chat to build its trajectory.");
    reconcile(this.root, tasks.length ? [list] : [this.empty]);
    for (const [id, branch] of this.branches) if (!this.root.contains(branch.item)) this.branches.delete(id);
    if (follow) this.root.scrollTop = this.root.scrollHeight;
    return { tasks:this.model.tasks.size, tools:this.model.tools.size, processes:processes.length, resources:snapshot.behaviorCount };
  }
  focus(id) {
    for (const task of this.model.tasks.values()) {
      for (const round of task.rounds) {
        if (round.tools.has(id) || [...round.messages.keys()].some(key => `message-${key}` === id)) {
          this.expanded.set(`task-${task.id}`,true); this.expanded.set(round.id,true);
        }
      }
    }
    this.reveal();
    requestAnimationFrame(() => {
      const item = [...this.root.querySelectorAll("[data-agent-node]")].find(node => node.dataset.agentNode === id);
      if (!item) return;
      for (let parent = item.parentElement; parent && parent !== this.root; parent = parent.parentElement) {
        if (!parent.classList.contains("agent-children")) continue;
        const id = parent.parentElement.dataset.agentNode;
        this.expanded.set(id, true);
        this.updateBranch(this.branches.get(id), true);
      }
      item.querySelector(".agent-card")?.classList.add("agent-highlight"); item.scrollIntoView({ block: "nearest", behavior: "smooth" });
    });
  }
  closeDetail() { this.selected = null; this.abort?.abort(); this.detail.hidden = true; }
  heading(title, event) {
    this.abort?.abort(); this.abort = new AbortController(); this.detail.hidden = false; this.detail.replaceChildren();
    const heading = el("div", "agent-detail-heading"); heading.append(el("strong", "", title));
    const close = el("button", "agent-icon"); close.type = "button"; close.innerHTML = closeIcon; close.title = "Close detail"; close.setAttribute("aria-label", "Close detail"); close.addEventListener("click", () => this.closeDetail()); heading.append(close); this.detail.append(heading);
    if (event) this.onRaw(event);
    return this.abort.signal;
  }
  async section(title, event, signal, parent = this.detail) {
    const details = el("details", "agent-content-section"); details.open = true;
    const summary = el("summary", "", title); const copy = el("button", "agent-icon"); copy.type = "button"; copy.innerHTML = copyIcon; copy.title = `Copy ${title.toLowerCase()}`; copy.setAttribute("aria-label", copy.title);
    const body = el("pre", "agent-content", "Loading…"); summary.append(copy); details.append(summary, body); parent.append(details);
    let text = ""; copy.disabled = true;
    copy.addEventListener("click", async e => { e.preventDefault(); e.stopPropagation(); try { await copyWithFeedback(copy, text); } catch (error) { copy.title = error.message; } });
    if (title === "Model response" && (!event || !event.data.payload)) {
      body.textContent = event ? "The complete model response was not saved in this recording." : "No model response has been captured yet.";
      return;
    }
    if (!event) { body.textContent = title === "Request parameters" ? "Complete parameters have not appeared in captured model traffic." : "This result has not appeared in model traffic. Hidden internal tool returns are unavailable."; return; }
    try {
      const payload = event.data.payload;
      if (!payload || payload.status !== "complete") throw new Error(payload?.error || "Complete payload unavailable");
      text = display(await this.load(payload, signal));
      if (signal.aborted) return;
      body.textContent = text; copy.disabled = false;
    } catch (error) { if (!signal.aborted) body.textContent = error.message; }
  }
  showTool(tool) {
    if (!tool) return;
    this.selected = tool.id;
    const signal = this.heading(`${tool.name} · Tool call`, tool.arguments || tool.started);
    this.detail.append(el("p", "agent-detail-note", `${tool.id} · model call ${tool.external || "unavailable"}. Process associations marked inferred can be ambiguous during concurrency or yielded execution.`));
    this.section("Request parameters", tool.arguments, signal);
    this.section("Returned content", tool.output, signal);
  }
  showModel(round, title) {
    this.selected = round.id;
    const signal = this.heading(title, round.request || round.started || round.first);
    this.section("Model request", round.request, signal);
    this.section("Model response", round.complete, signal);
  }
  showContent(title, event) { this.selected = null; const signal = this.heading(title, event); this.section(title, event, signal); }
  showMessage(message) { this.showContent("Assistant message", message.event); }
}
