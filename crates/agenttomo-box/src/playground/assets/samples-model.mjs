// Findings are separate from AgentModel: a successful task is not a successful attack.
export class SamplesModel {
  constructor() { this.runs = new Map(); this.reportEvents = new Map(); }
  reportEvent(taskId, matched) { return this.reportEvents.get(taskId)?.get(matched); }
  get findingCount() {
    let count = 0;
    for (const run of this.runs.values()) {
      if (run.event.kind === "security.sample.finished" && run.status === "reproduced") count++;
    }
    return count;
  }
  add(event) {
    if (event.kind === "file.report_marker") {
      const {task_id, matched} = event.data || {};
      if (!task_id || typeof matched !== "boolean" || this.reportEvent(task_id, matched)) return false;
      if (!this.reportEvents.has(task_id)) this.reportEvents.set(task_id, new Map());
      this.reportEvents.get(task_id).set(matched, event);
      return true;
    }
    if (!event.kind.startsWith("security.sample.")) return false;
    const data = event.data || {};
    if (!data.task_id || !data.sample_id) return false;
    const previous = this.runs.get(data.task_id);
    if (previous?.event?.sequence >= event.sequence) return false;
    if (previous && previous.status !== "running" && event.kind !== "security.sample.finished") return false;
    let phase = data.phase || previous?.phase || "preparing";
    if (event.kind === "security.sample.started") phase = "executing";
    if (event.kind === "security.sample.finished" && ["reproduced","not_reproduced"].includes(data.status)) phase = "completed";
    this.runs.set(data.task_id, {...previous,...data,phase,event});
    return true;
  }
  end() {
    for (const run of this.runs.values()) if (run.status === "running") run.status = "interrupted";
  }
}
