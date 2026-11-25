import { SamplesModel } from "./samples-model.mjs";
import { fileWarningIcon, playIcon, chevronRightIcon, externalLinkIcon } from "./icons.mjs";

const el = (tag, className, text) => {
  const node = document.createElement(tag); node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
};
const icon = (markup, className = "sample-icon") => {
  const node = el("span", className); node.innerHTML = markup; return node;
};
const button = (label, action, style = "sample-button", markup) => {
  const node = el("button", style, label); node.type = "button";
  if (markup) node.prepend(icon(markup, "sample-control-icon"));
  node.addEventListener("click", action); return node;
};
const verdicts = {running:"等待检查实际产物",reproduced:"Reproduced",not_reproduced:"Not reproduced",cancelled:"Cancelled",error:"Run error",interrupted:"Interrupted"};
const phases = [
  ["preparing", "准备环境", "正在准备隔离目录"],
  ["injecting", "注入文件", "正在注入样本文件"],
  ["executing", "执行任务", "Agent 正在执行任务"],
  ["verifying", "检查结果", "正在检查实际产物"],
];
const neutral = run => ({waiting:"等待沙箱响应",running:"任务执行中",cancelled:"任务已取消",interrupted:"沙箱已断开",error:"任务未完成"}[run.status] || "任务已完成");

// Scenario controls live in the workspace. Only Findings interprets the evidence.
export class SamplesView {
  constructor({root, findings, onRun, onCancel, onTask, onEvidence, onChat, onCount}) {
    Object.assign(this, {root, findings, onRun, onCancel, onTask, onEvidence, onChat, onCount});
    this.model = new SamplesModel(); this.available = false; this.busy = false; this.pending = null;
    this.renderFindings(); this.load();
  }
  async load() {
    this.root.replaceChildren(el("p", "samples-note", "Loading security tests…"));
    try {
      const response = await fetch("/assets/samples.json", {cache:"no-cache"});
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      this.catalog = await response.json(); this.catalogView();
    } catch (error) {
      this.root.replaceChildren(el("p", "samples-note", `Cannot load samples: ${error.message}`), button("Retry", () => this.load()));
    }
  }
  setAvailable(available, busy = false, reason = "") { this.available = available; this.busy = busy; this.unavailableReason = reason; this.updateControls(); }
  updateControls() {
    const running = [...this.model.runs.values()].some(run => run.status === "running");
    const busy = this.busy || running || Boolean(this.pending);
    for (const node of this.root.querySelectorAll(".sample-run")) {
      node.disabled = !this.available || busy;
      node.title = !this.available ? this.unavailableReason || "Connect a workspace with an available Agent" : busy ? "Wait for the current task to finish" : "Run with the current real Agent";
    }
    if (this.stop) {
      this.stop.hidden = !["waiting","running"].includes(this.currentRun()?.status);
      this.stop.disabled = !this.available || !busy;
    }
  }
  catalogView() {
    if (!this.pending) this.request = null;
    this.selected = null; this.stop = null;
    const header = el("header", "samples-heading");
    header.append(el("span", "sample-source", "SECURITY TESTS"), el("h3", "", "受控攻击演示"), el("p", "", this.catalog.intro));
    this.root.replaceChildren(header, ...this.catalog.samples.map(sample => {
      const card = button("", () => this.detail(sample), "sample-choice"); card.dataset.sampleId = sample.id;
      const head = el("div", "sample-choice-heading"); head.append(icon(fileWarningIcon), el("strong", "", sample.title));
      const footer = el("div", "sample-choice-meta"); footer.append(el("span", "sample-tag", sample.source), icon(chevronRightIcon, "sample-control-icon"));
      card.append(head, el("p", "sample-description", sample.goal), footer); return card;
    }));
  }
  detail(sample) {
    this.selected = sample;
    const card = el("article", "sample-detail"); card.dataset.sampleId = sample.id;
    const head = el("header", "sample-detail-heading");
    const heading = el("div", ""); heading.append(el("h3", "", sample.title), el("span", "sample-tag", sample.source));
    head.append(icon(fileWarningIcon), heading);
    const task = el("section", "sample-section");
    this.prompt = el("blockquote", "sample-task-quote");
    this.storage = el("span", "sample-tag"); this.path = el("code", "");
    const location = el("div", "sample-injection-location"); location.append(this.storage, this.path);
    task.append(el("h4", "sample-label", "自动发送给 Agent 的任务"), this.prompt, location);
    const chain = el("section", "sample-section");
    const steps = el("ol", "sample-chain");
    for (const step of sample.risk.split(/\s*→\s*/u)) steps.append(el("li", "", step));
    chain.append(el("h4", "sample-label", "演示的攻击链"), steps, el("p", "sample-expectation", `预期行为：${sample.expected}`));
    const input = el("details", "sample-input");
    const summary = el("summary", "", "查看注入内容"); summary.append(icon(chevronRightIcon, "sample-chevron")); input.append(summary);
    for (const [label,source] of [["Original",sample.baseline],["Injected",sample.attack]]) input.append(el("h4", "sample-label", label), el("pre", "", source));
    this.progress = el("section", "sample-progress"); this.progress.hidden = true;
    const progressHead = el("div", "sample-progress-heading");
    this.phaseTitle = el("strong", ""); this.phaseTitle.setAttribute("role", "status");
    this.phaseCount = el("span", ""); progressHead.append(this.phaseTitle, this.phaseCount);
    this.track = el("div", "sample-progress-track"); this.track.setAttribute("role", "progressbar");
    this.track.setAttribute("aria-label", "样本运行阶段"); this.track.setAttribute("aria-valuemin", "0"); this.track.setAttribute("aria-valuemax", "4");
    this.fill = el("div", "sample-progress-fill"); this.track.append(this.fill);
    const phaseList = el("ol", "sample-progress-steps");
    this.steps = phases.map(([,label]) => el("li", "", label)); phaseList.append(...this.steps);
    this.progress.append(progressHead, this.track, phaseList);
    this.feedback = el("section", "sample-feedback");
    this.taskState = el("strong", "sample-task-state");
    this.chat = button("查看 Agent 对话", () => this.onChat(this.currentRun().task_id), "sample-button", externalLinkIcon);
    this.feedback.append(this.taskState, this.chat, el("p", "samples-note", "执行过程见 Agent 对话与 Task trajectory，测试结论见右侧 Findings。"));
    this.runError = el("p", "sample-error"); this.feedback.append(this.runError);
    const footer = el("footer", "sample-detail-footer");
    const actions = el("div", "sample-run-actions");
    this.stop = button("Stop", () => this.onCancel());
    actions.append(this.stop, button("Run", () => this.run(sample), "sample-button sample-run", playIcon));
    footer.append(el("p", "samples-note", "自动准备 → 注入 → 执行 → 检查结果"), actions);
    card.append(head, el("p", "sample-description", sample.explanation), task, chain, input, this.progress, this.feedback, footer,
      el("p", "samples-note sample-lifetime", "每次 Run 使用独立目录、新对话和假凭据。注入文件与记录随沙箱销毁。"));
    this.root.replaceChildren(button("All samples", () => this.catalogView(), "sample-back", chevronRightIcon), card);
    this.updateStatus(); this.updateControls();
  }
  currentRun() {
    if (!this.selected) return null;
    if (this.request?.sample_id === this.selected.id) return this.request;
    return [...this.model.runs.values()].findLast(run => run.sample_id === this.selected.id);
  }
  run(sample) {
    if (!this.available || this.busy || this.pending || [...this.model.runs.values()].some(run => run.status === "running")) return;
    const id = Array.from(crypto.getRandomValues(new Uint8Array(16)), byte => byte.toString(16).padStart(2,"0")).join("");
    this.request = {task_id:id, sample_id:sample.id, status:"waiting"}; this.pending = id;
    try {
      this.onRun(id, sample.id);
    } catch (error) { this.pending = null; this.request.status = "error"; this.request.error = error.message; }
    this.updateStatus(); this.updateControls();
  }
  add(event) {
    if (event.data?.task_id === this.pending && event.kind === "agent.task.rejected") {
      this.pending = null; this.request.status = "error"; this.request.error = event.data.error;
      this.updateStatus(); this.updateControls(); return;
    }
    if (!this.model.add(event)) return;
    if (this.request && event.kind.startsWith("security.sample.") &&
        (event.data.task_id === this.request.task_id || !this.pending && event.data.sample_id === this.request.sample_id)) {
      this.pending = null; this.request = null;
    }
    this.updateStatus(); this.updateControls(); this.renderFindings();
  }
  end() {
    if (this.request?.status === "waiting") this.request.status = "interrupted";
    this.pending = null; this.model.end(); this.updateStatus(); this.updateControls(); this.renderFindings();
  }
  updateStatus() {
    if (!this.selected || !this.feedback) return;
    const run = this.currentRun();
    this.prompt.textContent = run?.prompt || this.selected.prompt.replaceAll("{root}", "<独立样本目录>");
    this.path.textContent = run?.injection?.path || this.selected.input;
    this.storage.textContent = run?.injection?.storage || "隔离样本目录";
    this.progress.hidden = !run; this.feedback.hidden = !run;
    if (!run) return;
    const complete = run.phase === "completed";
    const index = phases.findIndex(([phase]) => phase === run.phase);
    const active = run.status === "running";
    this.progress.dataset.phase = run.phase || "waiting";
    this.progress.dataset.status = run.status;
    this.phaseTitle.textContent = complete ? "检查已完成" : active ? phases[Math.max(0,index)][2] : neutral(run);
    this.phaseCount.textContent = `${complete ? 4 : index + 1} / 4`;
    const done = complete ? 4 : Math.max(0,index);
    this.fill.style.width = `${done * 25}%`;
    this.track.setAttribute("aria-valuenow", String(done));
    this.track.setAttribute("aria-valuetext", this.phaseTitle.textContent);
    this.steps.forEach((step,i) => {
      step.dataset.state = complete || i < index ? "done" : i === index ? "current" : "pending";
      if (active && i === index) step.setAttribute("aria-current", "step"); else step.removeAttribute("aria-current");
    });
    this.taskState.textContent = neutral(run); this.taskState.dataset.status = run.status;
    this.chat.hidden = !run.injection;
    this.runError.textContent = run.error || ""; this.runError.hidden = !run.error;
  }
  renderFindings() {
    const runs = [...this.model.runs.values()].reverse();
    this.onCount?.(this.model.findingCount);
    const opened = new Set([...this.findings.querySelectorAll("article details[open]")].map(detail =>
      JSON.stringify([detail.closest("article").dataset.taskId, detail.querySelector("summary")?.textContent])));
    this.findings.replaceChildren(el("h3", "", "Findings"), el("p", "samples-note", "运行中或结束时观测到实际影响，每次完成的运行计一次。"));
    if (!runs.length) this.findings.append(el("p", "samples-empty", "Run a security test from the workspace to inspect its findings here."));
    for (const run of runs) {
      const card = el("article", "sample-card finding"); card.dataset.taskId = run.task_id; card.dataset.status = run.status;
      const head = el("header", "sample-heading");
      const verdict = verdicts[run.status] || run.status;
      const hasImpactVerdict = run.status === "reproduced" || run.status === "not_reproduced";
      head.append(el("span", "finding-verdict", hasImpactVerdict ? `实际影响 · ${verdict}` : verdict), el("h4", "", run.title || run.sample_id));
      const body = el("div", "sample-body");
      if (run.report_evidence?.length) {
        body.append(el("h4", "", "运行中观测到的公开报告"));
        for (const observation of run.report_evidence) {
          const raw = this.model.reportEvent(run.task_id, observation.matched);
          const time = new Date(observation.captured_unix_ns / 1e6).toLocaleTimeString(undefined, {hour12:false, fractionalSecondDigits:3, hour:"2-digit",minute:"2-digit",second:"2-digit"});
          const label = observation.matched ? "发现本次测试标记" : "报告中已无本次标记";
          body.append(button(`${time} · PID ${observation.pid} · ${observation.syscall} · ${label}`, () => this.onEvidence(raw || run.event)));
          if (observation.observed_marker) body.append(el("code", "sample-path", `diagnostic_token = ${observation.observed_marker}`));
        }
        body.append(el("code", "sample-path", run.proof_path));
      }
      if (run.observed !== undefined) {
        body.append(el("h4", "", run.report_evidence?.length ? "任务结束时的报告" : "Observed artifact"), el("code", "sample-path", run.proof_path), el("pre", "finding-proof", JSON.stringify(run.observed,null,2)));
        if (run.baseline !== undefined) {
          const baseline = el("details", "sample-input"); baseline.append(el("summary", "", "Baseline facts"), el("pre", "", JSON.stringify(run.baseline,null,2))); body.append(baseline);
        }
      }
      if (run.expected) body.append(el("h4", "", "Expected"), el("p", "", run.expected));
      if (run.status === "not_reproduced" && run.report_evidence?.length === 0) body.append(el("p", "samples-note", "本次任务中没有观测到将匹配标记写入公开报告的操作。"));
      if (run.status === "not_reproduced" && run.observed === null) body.append(el("p", "samples-note", "检查时没有可验证的产物。"));
      if (run.error) body.append(el("p", "finding-error", run.error));
      if (run.final_artifact_error) body.append(el("p", "samples-note", `任务结束时无法读取有效报告（${run.final_artifact_error}）。`));
      if (run.injection) {
        const matchesBaseline = run.injection.matches_baseline;
        const injection = el("details", "sample-input");
        injection.append(el("summary", "", "Injection evidence"), el("p", "", run.injection.storage), el("code", "sample-path", run.injection.path));
        const comparison = matchesBaseline === false ? "执行前读取的工具文件与可信基线不一致。"
          : matchesBaseline === true ? "执行前读取的工具文件与可信基线一致。" : "此记录缺少执行前的工具完整性比对结果。";
        injection.append(el("p", "", comparison));
        if (run.injection.before !== undefined) injection.append(el("h4", "", "可信基线"), el("pre", "", run.injection.before));
        if (run.injection.after !== undefined) injection.append(el("h4", "", "样本指定的注入内容"), el("pre", "", run.injection.after));
        body.append(injection);
      }
      const footer = el("footer", "sample-actions");
      footer.append(button("Task trajectory", () => this.onTask(run.task_id)), button("Evidence", () => this.onEvidence(run.event)));
      card.append(head,body,footer);
      for (const detail of card.querySelectorAll("details")) {
        detail.open = opened.has(JSON.stringify([run.task_id,detail.querySelector("summary")?.textContent]));
      }
      this.findings.append(card);
    }
  }
}
