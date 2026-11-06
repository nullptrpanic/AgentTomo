const initialKinds = [
  "file.open", "memory.mmap", "process.exec", "process.fork", "process.vfork", "process.clone", "process.exit",
  "network.upstream", "network.protocol", "network.transfer",
  "network.udp.send", "network.udp.receive", "network.udp.transfer", "network.udp.error",
];
const category = (kind) => kind.includes(".") ? kind.split(".")[0] : "other";

export class EventFilter {
  constructor() {
    this.defaultEnabled = true;
    this.rules = new Map();
    this.groups = new Map();
    for (const kind of initialKinds) this.observe(kind);
  }

  observe(kind) {
    const group = category(kind);
    if (!this.groups.has(group)) this.groups.set(group, new Set());
    this.groups.get(group).add(kind);
  }

  allows(kind) {
    return this.rules.get(kind) ?? this.rules.get(`${category(kind)}.*`) ?? this.defaultEnabled;
  }

  set(pattern, enabled) {
    if (pattern === "*") {
      this.defaultEnabled = enabled;
      this.rules.clear();
      return;
    }
    if (pattern.endsWith(".*")) {
      const prefix = pattern.slice(0, -1);
      for (const kind of this.rules.keys()) if (kind.startsWith(prefix)) this.rules.delete(kind);
    }
    this.rules.set(pattern, enabled);
  }

  groupState(group) {
    // Include the wildcard's default for event kinds not observed yet.
    const kinds = this.groups.get(group) ?? [];
    const states = [this.allows(`${group}.*`), ...[...kinds].map(kind => this.allows(kind))];
    return { checked: states.every(Boolean), indeterminate: states.some(Boolean) && !states.every(Boolean) };
  }

  get active() {
    return !this.defaultEnabled || [...this.rules.values()].some(enabled => !enabled);
  }
}

export class FilterMenu {
  constructor(root, filter, onChange) {
    this.root = root;
    this.filter = filter;
    this.onChange = onChange;
    this.groups = new Map();
    this.inputs = new Map();
    root.querySelector('[data-filter-action="all"]').addEventListener("click", () => this.change("*", true));
    root.querySelector('[data-filter-action="none"]').addEventListener("click", () => this.change("*", false));
    root.addEventListener("keydown", event => {
      if (event.key === "Escape") { root.open = false; root.querySelector("summary").focus(); }
    });
    this.update();
  }

  change(pattern, enabled) {
    this.filter.set(pattern, enabled);
    this.update();
    this.onChange();
  }

  checkbox(pattern, parent) {
    const label = document.createElement("label");
    const input = document.createElement("input");
    input.type = "checkbox";
    input.setAttribute("aria-label", pattern);
    input.addEventListener("change", () => this.change(pattern, input.checked));
    label.append(input, document.createTextNode(pattern));
    parent.append(label);
    this.inputs.set(pattern, input);
  }

  update() {
    for (const [group, kinds] of this.filter.groups) {
      if (!this.groups.has(group)) {
        const fieldset = document.createElement("fieldset");
        const legend = document.createElement("legend");
        this.checkbox(`${group}.*`, legend);
        fieldset.append(legend);
        this.root.querySelector(".filter-groups").append(fieldset);
        this.groups.set(group, fieldset);
      }
      for (const kind of kinds) if (!this.inputs.has(kind)) this.checkbox(kind, this.groups.get(group));
      Object.assign(this.inputs.get(`${group}.*`), this.filter.groupState(group));
      for (const kind of kinds) this.inputs.get(kind).checked = this.filter.allows(kind);
    }
    this.root.querySelector(".filter-state").textContent = this.filter.active ? "Filtered" : "All events";
  }
}
