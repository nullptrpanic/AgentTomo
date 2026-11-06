import { copyText } from "./copy-text.mjs";

const size = bytes => bytes < 1024 ? `${bytes} B` : `${(bytes / 1024).toFixed(1)} KiB`;
const reasons = {
  disabled: "File content capture was disabled for this run.",
  open_failed: "The file open failed; no content was captured.",
  write_only: "The file was opened for writing only; no content was read.",
  path_only: "O_PATH opens do not read file content.",
  not_regular: "Only regular files have content previews.",
  unsupported_filesystem: "This filesystem is not supported for file snapshots.",
  unknown_flags: "The file access mode could not be determined.",
  read_handle_unavailable: "A readable snapshot handle was unavailable.",
  read_error: "The file snapshot could not be read.",
  storage_error: "The file snapshot could not be saved.",
  budget_exhausted: "The run's file-preview budget was exhausted. File events are still recorded.",
};

export function previewContent(bytes, { pretty = false, truncated = false, forceHex = false } = {}) {
  let text, incompleteUtf8 = false;
  try {
    if (forceHex) throw new Error("binary");
    text = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(bytes, { stream: truncated });
    if (/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/.test(text)) throw new Error("binary");
    if (truncated) {
      try { new TextDecoder("utf-8", { fatal: true }).decode(bytes); } catch { incompleteUtf8 = true; }
    }
  } catch {
    const rows = [];
    for (let offset = 0; offset < bytes.length; offset += 16) {
      const part = bytes.slice(offset, offset + 16);
      const hex = [...part].map(byte => byte.toString(16).padStart(2, "0")).join(" ").padEnd(47, " ");
      const ascii = [...part].map(byte => byte >= 32 && byte <= 126 ? String.fromCharCode(byte) : ".").join("");
      rows.push(`${offset.toString(16).padStart(8, "0")}  ${hex}  |${ascii}|`);
    }
    return { text: rows.join("\n"), format: "hex", json: false, incompleteUtf8: false };
  }
  let json = false;
  if (!truncated) {
    try {
      const value = JSON.parse(text);
      // Native parsing can round numbers, drop duplicate keys or normalize
      // escapes. Offer pretty mode only when a round trip preserves all tokens.
      const compact = text.replace(/"(?:\\.|[^"\\])*"|\s+/g, token => token.startsWith('"') ? token : "");
      json = compact === JSON.stringify(value);
      if (pretty && json) text = JSON.stringify(value, null, 2);
    } catch { /* Keep original text for incomplete/non-JSON files. */ }
  }
  return { text, format: "text", json, incompleteUtf8 };
}

function append(parent, tag, className, text) {
  const node = document.createElement(tag);
  node.className = className;
  node.textContent = text;
  parent.append(node);
  return node;
}

export class FilePreview {
  constructor(root, load) {
    this.root = root;
    this.load = load;
    this.version = 0;
    root.addEventListener("click", async event => {
      const button = event.target.closest("button");
      if (!button) return;
      if (button.hasAttribute("data-preview-close")) { this.close(); return; }
      if (button.dataset.previewFormat) {
        const mode = button.dataset.previewFormat;
        this.pretty = mode === "pretty";
        this.renderContent();
        root.querySelector(`[data-preview-format="${mode}"]`)?.focus({ preventScroll: true });
        return;
      }
      if (button.dataset.previewCopy) {
        const version = this.version;
        const text = button.dataset.previewCopy === "path" ? this.path : this.displayed?.text;
        if (text === undefined) return;
        button.disabled = true;
        try {
          await copyText(text);
          if (version === this.version) this.status.textContent = button.dataset.previewCopy === "path" ? "Path copied." : this.event.data.preview?.status === "truncated" ? "Captured prefix copied; the full file was not captured." : "Displayed content copied.";
        } catch (error) {
          if (version === this.version) this.status.textContent = error.message;
        } finally { if (button.isConnected) button.disabled = false; }
      }
    });
    root.addEventListener("keydown", event => { if (event.key === "Escape") { event.preventDefault(); this.close(); } });
  }

  close() {
    this.version++;
    this.request?.abort();
    this.root.hidden = true;
    this.bytes = null;
    this.displayed = null;
  }

  async show(event) {
    this.request?.abort();
    const version = ++this.version;
    this.request = new AbortController();
    this.event = event;
    this.bytes = null;
    this.displayed = null;
    this.pretty = false;
    const data = event.data || {};
    const preview = data.preview;
    this.path = data.resolved_path || data.path || "[path unavailable]";
    this.root.hidden = false;
    this.root.innerHTML = `<div class="file-inspector-heading"><strong>File preview</strong><button type="button" data-preview-close aria-label="Close file preview">Close ×</button></div><div class="file-inspector-path"><code></code><button type="button" data-preview-copy="path">Copy path</button></div><p class="file-inspector-meta"></p><div class="file-inspector-body"></div><p class="file-inspector-status" role="status"></p>`;
    this.root.querySelector("code").textContent = this.path;
    this.root.querySelector("[data-preview-copy=path]").disabled = !data.resolved_path && !data.path;
    this.root.querySelector(".file-inspector-meta").textContent = `PID ${data.pid ?? "—"} · ${data.syscall || "open"} · ${data.result >= 0 ? `FD ${data.result}` : `errno ${data.errno ?? "?"}`} · Event #${event.sequence}`;
    this.body = this.root.querySelector(".file-inspector-body");
    this.status = this.root.querySelector(".file-inspector-status");
    if (!preview || !["complete", "truncated"].includes(preview.status)) {
      append(this.body, "p", "file-inspector-empty", reasons[preview?.reason] || "Content was not captured for this event.");
      return;
    }
    append(this.body, "p", "file-inspector-empty", "Loading captured content…");
    try {
      this.bytes = null;
      const bytes = await this.load(event, this.request.signal);
      if (version !== this.version) return;
      this.bytes = bytes;
      this.pretty = previewContent(bytes, { truncated: preview.status === "truncated" }).json;
      this.renderContent();
    } catch (error) {
      if (version !== this.version) return;
      this.body.replaceChildren();
      append(this.body, "p", "file-inspector-empty", `Preview unavailable: ${error.message}`);
    }
  }

  renderContent() {
    const preview = this.event.data.preview;
    const truncated = preview.status === "truncated";
    this.displayed = previewContent(this.bytes, { pretty: this.pretty, truncated });
    this.body.replaceChildren();
    append(this.body, "p", `file-inspector-note${truncated ? " truncated" : ""}`, `${truncated ? "Truncated prefix" : "Captured snapshot"} · ${size(this.bytes.length)}${Number.isFinite(preview.size_at_open) ? ` · size at open ${size(preview.size_at_open)}` : ""}`);
    const toolbar = append(this.body, "div", "file-inspector-tools", "");
    append(toolbar, "span", "", this.displayed.format === "hex" ? "Hex / ASCII" : this.displayed.json ? "JSON · UTF-8" : "Text · UTF-8");
    if (this.displayed.json) {
      for (const [mode, label] of [["pretty", "Pretty"], ["raw", "Raw"]]) {
        const button = append(toolbar, "button", "", label);
        button.type = "button";
        button.dataset.previewFormat = mode;
        button.setAttribute("aria-pressed", String((mode === "pretty") === this.pretty));
      }
    }
    const copy = append(toolbar, "button", "", this.displayed.format === "hex" ? "Copy hex" : "Copy content");
    copy.type = "button";
    copy.dataset.previewCopy = "content";
    if (this.bytes.length === 0) append(this.body, "p", "file-inspector-empty", "Empty file · 0 bytes captured.");
    else if (this.displayed.format === "hex") append(this.body, "pre", "file-inspector-code hex", this.displayed.text);
    else {
      const code = append(this.body, "div", "file-inspector-code lines", "");
      // Keep DOM size constant even for a small file containing many newlines.
      const count = this.displayed.text.split("\n").length;
      const numbers = Array.from({ length: count }, (_, index) => index + 1).join("\n");
      append(code, "pre", "line-number", numbers).setAttribute("aria-hidden", "true");
      append(code, "pre", "line-text", this.displayed.text);
    }
    this.status.textContent = this.displayed.incompleteUtf8 ? "The byte limit cuts a UTF-8 character; that incomplete character is not displayed or copied." : "Snapshot near file open; not a record of bytes read by the application.";
  }
}
