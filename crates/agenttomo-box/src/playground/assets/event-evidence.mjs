// Raw records are secondary details in every behavior presentation. Reconcile
// open rows by record ID so live updates preserve focus and disclosure state.
export class EventEvidence {
  constructor(label, onInspect, onToggle = () => {}) {
    this.label = label;
    this.onInspect = onInspect;
    this.records = [];
    this.rows = new Map();
    this.details = document.createElement("details");
    this.details.className = "event-evidence";
    this.summary = document.createElement("summary");
    this.list = document.createElement("ol");
    this.list.className = "evidence-records";
    this.details.append(this.summary, this.list);
    this.details.addEventListener("toggle", () => { this.update(this.records); onToggle(); });
  }

  update(records) {
    this.records = records;
    this.details.hidden = !records.length;
    const label = `${this.label} · ${records.length}`;
    if (this.summary.textContent !== label) this.summary.textContent = label;
    if (!this.details.open) { this.list.replaceChildren(); this.rows.clear(); return; }
    const retained = new Set(records.map(entry => entry.id));
    for (const [id, row] of this.rows) if (!retained.has(id)) { row.item.remove(); this.rows.delete(id); }
    let cursor = this.list.firstChild;
    for (const entry of records) {
      let row = this.rows.get(entry.id);
      if (!row) {
        const item = document.createElement("li"), button = document.createElement("button");
        button.className = "evidence-record";
        button.type = "button";
        button.dataset.sequence = entry.event.sequence;
        button.addEventListener("click", () => this.onInspect(entry.event));
        item.append(button);
        row = { item, button };
        this.rows.set(entry.id, row);
      }
      const description = entry.description.length > 240 ? entry.description.slice(0, 240) + "…" : entry.description;
      const text = `#${entry.event.sequence} ${entry.event.kind} · ${description}`;
      if (row.button.textContent !== text) row.button.textContent = text;
      if (row.item !== cursor) this.list.insertBefore(row.item, cursor);
      cursor = row.item.nextSibling;
    }
  }
}
