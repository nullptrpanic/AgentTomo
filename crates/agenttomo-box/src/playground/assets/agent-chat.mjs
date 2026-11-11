// Stable conversation nodes. Trajectory projection and execution controls live in AgentView.
import { externalLinkIcon, copyIcon, userIcon } from "./icons.mjs";
import { copyWithFeedback } from "./copy-text.mjs";
import { markdownContent } from "./agent-markdown.mjs";
import { isFinalMessage, orderedRounds } from "./agent-model.mjs";

const el = (tag, name, text) => {
  const node = document.createElement(tag);
  node.className = name;
  if (text !== undefined) node.append(document.createTextNode(text));
  return node;
};

function text(node, value) {
  value = String(value ?? "");
  const current = node.firstChild;
  if (!current) { node.append(document.createTextNode(value)); return; }
  if (current.data === value) return;
  if (value.startsWith(current.data)) current.appendData(value.slice(current.length));
  else current.replaceData(0, current.length, value);
}

const time = event => (event?.data?.captured_unix_ns || event?.observed_unix_ns) / 1e6;
const duration = milliseconds => `${Math.max(0, milliseconds / 1000).toFixed(1)}s`;
const clock = new Intl.DateTimeFormat(undefined, {hour:"2-digit", minute:"2-digit"});

function timestamp(node, milliseconds) {
  node.hidden = !Number.isFinite(milliseconds);
  if (node.hidden) return;
  const date = new Date(milliseconds);
  node.dateTime = date.toISOString(); node.title = date.toLocaleString();
  text(node, clock.format(date));
}

export function taskProgress(task) {
  return task.stopping ? "Stopping task…" : task.phase;
}

function errorHint(error, agent) {
  if (/tls handshake|certificate|tls.*eof/i.test(error)) return "TLS connection failed. The connection closed before a secure session was established.";
  if (/\b401\b|unauthorized|not logged in|authentication|refresh.?token/i.test(error)) return agent.authentication_hint || "Agent authentication failed. Check its login on the server.";
  if (/\b403\b|forbidden|permission denied/i.test(error)) return "Access was denied. Check account permissions and the model configuration.";
  if (/timed? ?out|timeout|connection|connect error|network|dns/i.test(error)) return "The model connection failed. Check the server proxy and network connection.";
  return `${agent.name} reported an error. Expand the details for the original message.`;
}

export class AgentChat {
  constructor({root, section, onTool, onMessage, onTask, onModel}) {
    Object.assign(this, {root, section, onTool, onMessage, onTask, onModel});
    this.agent = {name:"Agent", avatar:">_"};
    this.records = new Map();
    this.follow = true;
    this.latest = root.parentElement.querySelector(".agent-latest");
    this.latest.addEventListener("click", () => { this.follow = true; this.scroll(); });
    root.addEventListener("scroll", () => {
      if (!root.clientHeight) return;
      this.follow = root.scrollHeight - root.scrollTop - root.clientHeight < 40;
      this.latest.hidden = this.follow;
    });
    new ResizeObserver(() => this.scroll()).observe(root);
  }
  configure(agent) { this.agent = agent; if (this.tasks) this.render(this.tasks); }
  render(tasks) {
    this.tasks = tasks;
    if (this.frame) return;
    this.frame = requestAnimationFrame(() => { this.frame = null; this.flush(); });
  }
  flush() {
    this.welcome?.remove();
    for (const task of this.tasks.values()) {
      let record = this.records.get(task.id);
      if (!record) { record = this.createTask(task); this.records.set(task.id, record); }
      text(record.prompt, task.prompt || "Observed task");
      text(record.agentName, this.agent.name);
      text(record.avatar, this.agent.avatar);
      record.reply.setAttribute("aria-label", `${this.agent.name} reply`);
      timestamp(record.user.timestamp, time(task.started) || task.queuedAt);
      const placements = new Map([[record.timeline, []], [record.answers, []]]);
      const groups = orderedRounds(task).map((round, index) => ({round, id:round.id, title:`Model call ${index + 1}`, entries:[]}));
      const owners = new Map();
      for (const group of groups) {
        for (const id of group.round.messages.keys()) if (!owners.has(`message-${id}`)) owners.set(`message-${id}`, group);
        for (const id of group.round.tools.keys()) if (!owners.has(`tool-${id}`)) owners.set(`tool-${id}`, group);
      }
      const unassigned = {id:"unassigned", title:"Unassigned events", entries:[]};
      for (const entry of task.timeline) {
        const key = `${entry.type}-${entry.id}`;
        (owners.get(key) || unassigned).entries.push(entry);
      }
      if (unassigned.entries.length) groups.push(unassigned);
      for (const group of groups) {
        if (!group.entries.length) continue;
        let response = record.responses.get(group.id);
        if (!response) {
          response = this.createResponse(task, group);
          record.responses.set(group.id, response);
        }
        text(response.title, group.title);
        text(response.label, this.agent.name);
        response.messages = group.entries.filter(entry => entry.type === "message").map(entry => task.messages.get(entry.id));
        response.copy.hidden = !response.messages.length;
        const primary = response.messages.some(message => isFinalMessage(task, message));
        const parent = primary ? record.answers : record.timeline;
        placements.get(parent).push(response.root);
        const nodes = [];
        for (const entry of group.entries) {
          const key = `${entry.type}-${entry.id}`;
          let node = record.entries.get(key);
          if (!node) {
            node = entry.type === "tool" ? this.createTool(task.tools.get(entry.id)) : this.createMessage();
            node.root.classList.add("agent-entry");
            node.root.dataset.entryType = entry.type;
            node.root.dataset.entryId = entry.id;
            record.entries.set(key, node);
          }
          if (entry.type === "tool") this.updateTool(node, task.tools.get(entry.id), task);
          else this.updateMessage(node, task.messages.get(entry.id));
          nodes.push(node.root);
        }
        placements.set(response.body, nodes);
        const started = response.messages[0]?.started || group.round?.started || group.round?.request || group.round?.first;
        timestamp(response.timestamp, time(started));
      }
      const primary = placements.get(record.answers);
      text(record.roundTitle, primary.length === 1 ? record.responses.get(primary[0].dataset.roundId).title.textContent : "");
      record.roundTitle.hidden = !record.roundTitle.textContent;
      // Keep response and entry nodes in place while streams add text/results.
      for (const [parent, nodes] of placements) {
        for (const [index, node] of nodes.entries()) {
          if (parent.children[index] !== node) parent.insertBefore(node, parent.children[index] || null);
        }
      }
      for (const [parent, nodes] of placements) {
        const retained = new Set(nodes);
        for (const child of [...parent.children]) if (!retained.has(child)) child.remove();
      }
      const error = task.errors.at(-1)?.data.error;
      record.error.hidden = !error;
      if (error) {
        const value = typeof error === "string" ? error : JSON.stringify(error, null, 2);
        text(record.hint, errorHint(value, this.agent)); text(record.errorBody, value);
      }
      record.pending.hidden = task.state !== "running" || Boolean(error);
      const expandable = !record.pending.hidden || Boolean(error) || placements.get(record.timeline).length > 0;
      record.summary.setAttribute("aria-disabled", String(!expandable));
      record.summary.tabIndex = expandable ? 0 : -1;
      if (!expandable) record.work.open = false;
      const label = {running:"Task in progress",completed:"Task completed",cancelled:"Task stopped",failed:"Task failed",rejected:"Task rejected",interrupted:"Task interrupted"}[task.state] || task.state;
      text(record.workTitle, task.state === "running" && task.stopping ? "Stopping task" : task.state === "running" && error ? "Retrying connection" : label);
      record.indicator.classList.toggle("agent-spinner", task.state === "running");
      record.work.dataset.state = task.state;
      record.reply.dataset.state = task.state;
      const count = task.tools.size;
      text(record.workCount, count ? `${count} tool ${count === 1 ? "call" : "calls"}` : "");
      if (task.state === "completed" && !record.completed) {
        record.work.open = false;
        record.completed = true;
      }
      if (task.state !== "running") {
        const elapsed = (time(task.finished) || task.endedAt || Date.now()) - (time(task.started) || task.queuedAt);
        text(record.outcome, duration(elapsed));
        record.outcome.dataset.state = task.state;
      } else {
        text(record.outcome, "Running");
        record.outcome.dataset.state = "running";
      }
    }
    if (!this.records.size) {
      this.welcome = el("p", "agent-welcome", `Send a task to ${this.agent.name}. Follow its responses and tool calls here, and inspect the evidence in the trajectory.`);
      this.root.append(this.welcome);
    }
    this.updateProgress();
    this.scroll();
  }
  inspect(callback) {
    const button = el("button", "agent-icon agent-inspect");
    button.type = "button"; button.title = "View in task trajectory";
    button.setAttribute("aria-label", button.title); button.innerHTML = externalLinkIcon;
    button.addEventListener("click", callback);
    return button;
  }
  createTask(task) {
    const root = el("section", "agent-chat-task"); root.dataset.taskId = task.id;
    const user = this.bubble("user", () => task.prompt, () => this.onTask(task.id));
    const prompt = user.body;
    const reply = el("article", "agent-reply");
    reply.setAttribute("aria-label", `${this.agent.name} reply`);
    const heading = el("header", "agent-reply-heading");
    const avatar = el("span", "agent-avatar", this.agent.avatar); avatar.setAttribute("aria-hidden", "true");
    const roundTitle = el("span", "agent-response-title");
    const agentName = el("strong", "", this.agent.name);
    heading.append(avatar, agentName, roundTitle);
    const work = el("details", "agent-work"); work.open = true;
    const summary = el("summary", "agent-work-summary");
    summary.addEventListener("click", event => {
      if (summary.getAttribute("aria-disabled") === "true") event.preventDefault();
    });
    const indicator = el("span", "agent-state-dot"); indicator.setAttribute("aria-hidden", "true");
    const workTitle = el("strong", "agent-work-title", "Task in progress");
    const workCount = el("span", "agent-work-count", "");
    const outcome = el("span", "agent-chat-outcome", "");
    summary.append(indicator, workTitle, workCount, outcome);
    const timeline = el("div", "agent-timeline");
    const answers = el("div", "agent-final-answers");
    const error = el("article", "agent-error"); error.hidden = true; error.setAttribute("role", "alert");
    const hint = el("p", "", "");
    const details = el("details", "agent-error-details");
    const errorBody = el("pre", "agent-content", "");
    details.append(el("summary", "", "Error details"), errorBody); error.append(hint, details);
    const pending = el("span", "agent-progress"); pending.setAttribute("role", "status");
    const progress = el("span", "agent-progress-text", ""); pending.append(progress);
    work.append(summary, pending, error, timeline);
    reply.append(heading, answers, work);
    root.append(user.root, reply); this.root.append(root);
    return {root,user,prompt,reply,agentName,avatar,roundTitle,indicator,work,summary,workTitle,workCount,timeline,answers,error,hint,errorBody,pending,progress,outcome,entries:new Map(),responses:new Map()};
  }
  bubble(role, content, inspect) {
    const root = el("article", `agent-chat-message agent-${role}`);
    const heading = el("div", "agent-message-heading");
    const timestamp = el("time", "agent-message-time");
    if (role === "user") {
      const avatar = el("span", "agent-avatar agent-avatar-user");
      avatar.setAttribute("aria-hidden", "true");
      avatar.innerHTML = userIcon;
      heading.append(avatar);
    }
    const label = el("span", "agent-label", role === "user" ? "You" : this.agent.name);
    heading.append(label);
    const body = el("div", "agent-message-text", "");
    const actions = el("footer", "agent-message-actions");
    const copy = el("button", "agent-icon agent-copy"); copy.type = "button";
    copy.title = role === "user" ? "Copy question" : "Copy answer";
    copy.setAttribute("aria-label", copy.title); copy.innerHTML = copyIcon;
    copy.addEventListener("click", async () => {
      try { await copyWithFeedback(copy, content()); }
      catch (error) { copy.title = error.message; }
    });
    actions.append(copy, this.inspect(inspect), timestamp);
    root.append(heading, body, actions);
    return {root,heading,label,body,timestamp,copy};
  }
  createResponse(task, group) {
    const response = this.bubble("answer", () => response.messages.map(message => message.text).join("\n\n"), () => {
      if (group.round) this.onModel(group.round, response.title.textContent);
      else if (response.messages.length) this.onMessage(response.messages.at(-1));
      else this.onTask(task.id);
    });
    response.root.classList.add("agent-response");
    response.root.dataset.roundId = group.id;
    response.title = el("span", "agent-response-title", group.title);
    response.heading.append(response.title);
    response.body.className = "agent-response-items";
    response.messages = [];
    return response;
  }
  createMessage() {
    const root = el("article", "agent-answer");
    const body = el("div", "agent-message-text", "");
    root.append(body);
    return {root,body};
  }
  updateMessage(record, message) {
    if (message.complete) {
      if (record.rendered !== message.text) {
        record.body.replaceChildren(markdownContent(message.text));
        record.body.classList.add("agent-markdown");
        record.rendered = message.text;
      }
    } else text(record.body, message.text);
  }
  createTool(tool) {
    const root = el("details", "agent-tool-card"); root.dataset.toolId = tool.id;
    const summary = el("summary", "agent-tool-summary");
    const name = el("strong", "agent-tool-name", "");
    const preview = el("code", "agent-tool-preview", "");
    const state = el("span", "agent-tool-state", "");
    const elapsed = el("span", "agent-tool-duration", "");
    elapsed.title = "Time from observed tool call to model-visible return";
    summary.append(name, preview, state, elapsed);
    const body = el("div", "agent-tool-body");
    const parameters = el("div", "agent-tool-parameters"), result = el("div", "agent-tool-result");
    const inspect = this.inspect(() => this.onTool(tool.id)); inspect.classList.add("agent-tool-inspect");
    inspect.title = "Inspect tool evidence"; inspect.setAttribute("aria-label", inspect.title);
    body.append(parameters, result, inspect); root.append(summary, body);
    const record = {root,name,preview,state,elapsed,tool,parts:[{root:parameters},{root:result}]};
    root.addEventListener("toggle", () => { if (root.open) this.contents(record, this.tasks.get(tool.taskId).tools.get(tool.id)); });
    return record;
  }
  updateTool(record, tool, task) {
    text(record.name, tool.name);
    const preview = String(tool.arguments?.data.preview || "Parameters are being captured").replace(/\s+/g, " ").slice(0, 180);
    text(record.preview, preview); record.preview.title = preview;
    let state;
    if (tool.output) state = tool.output.data.payload?.status === "complete" ? "Returned" : "Return capture unavailable";
    else if (task.state !== "running") state = "Return unavailable";
    else if (tool.arguments) state = "Awaiting result";
    else state = "Preparing parameters";
    text(record.state, state);
    record.root.dataset.state = tool.output ? "returned" : task.state !== "running" ? "unavailable" : "pending";
    if (record.root.open) this.contents(record, tool);
  }
  contents(record, tool) {
    for (const [index, event] of [tool.arguments, tool.output].entries()) {
      const part = record.parts[index];
      if (part.loaded && part.event === event) continue;
      part.controller?.abort(); part.controller = new AbortController();
      const open = part.root.firstElementChild?.open ?? true;
      part.root.replaceChildren(); part.event = event; part.loaded = true;
      this.section(index === 0 ? "Request parameters" : "Returned content", event, part.controller.signal, part.root).then(() => this.scroll());
      part.root.firstElementChild.open = open;
    }
  }
  updateProgress() {
    for (const task of this.tasks?.values() || []) {
      const record = this.records.get(task.id);
      if (!record) continue;
      if (task.state === "running") {
        text(record.progress, taskProgress(task));
        text(record.outcome, duration(Date.now() - (time(task.started) || task.queuedAt)));
      }
      for (const node of record.entries.values()) {
        if (!node.tool) continue;
        const end = time(node.tool.output) || time(task.finished) || task.endedAt || Date.now();
        text(node.elapsed, time(node.tool.started) ? duration(end - time(node.tool.started)) : "");
      }
    }
  }
  scroll() {
    if (this.follow && this.root.clientHeight) this.root.scrollTop = this.root.scrollHeight;
    this.latest.hidden = this.follow;
  }
}
