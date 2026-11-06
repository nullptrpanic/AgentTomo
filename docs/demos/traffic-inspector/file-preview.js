// Browser-only examples. This viewer never reads paths from the host filesystem.
import { copyText } from "./clipboard.js";
const encode = new TextEncoder();
const bytes = (content) => encode.encode(content);
const size = (n) => n < 1024 ? `${n} B` : `${(n / 1024).toFixed(1)} KiB`;
const bundle = `# AgentTomo demo certificate bundle — not a real certificate\n${Array.from({ length: 6 }, (_, index) => `-----BEGIN CERTIFICATE-----\nDEMO-CERTIFICATE-${index + 1}\n${"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/\n".repeat(6)}-----END CERTIFICATE-----\n`).join("\n")}`;

export const fileExamples = [
  { id: "ca-bundle", pid: 35, process: "curl", path: "/etc/ssl/certs/ca-certificates.crt", requestedPath: "/etc/ssl/certs/ca-certificates.crt", format: "text", language: "PEM", flags: "O_RDONLY | O_CLOEXEC", fd: 6, time: "20:48:01.212", duration: "0.083 ms", content: bundle, limit: 1024 },
  { id: "binary", pid: 35, process: "curl", path: "/tmp/sample.bin", requestedPath: "/tmp/sample.bin", format: "binary", language: "Binary", flags: "O_RDONLY", fd: 7, time: "20:48:02.405", duration: "0.061 ms", content: new Uint8Array([65, 103, 101, 110, 116, 84, 111, 109, 111, 0, 1, 2, 3, 4, 5, 6, 16, 32, 48, 64, 80, 96, 112, 128, 144, 160, 176, 192, 208, 224, 240, 0, 100, 101, 109, 111, 45, 102, 105, 108, 101, 45, 99, 111, 110, 116, 101, 110, 116, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14]), limit: 4096 },
  { id: "readme", pid: 42, process: "python", path: "/workspace/README.md", requestedPath: "README.md", cwd: "/workspace", format: "text", language: "Markdown", flags: "O_RDONLY | O_CLOEXEC", fd: 5, time: "20:48:03.126", duration: "0.072 ms", limit: 4096, content: `# AgentTomo\n\nA diagnostic workspace for agent execution.\n\n## What gets recorded\n\n- Process creation and commands\n- File open paths and outcomes\n- HTTP requests and responses\n- SSE and WebSocket interactions\n\n## Getting started\n\n    agenttomo-box run -c 'python agent_demo.py'\n\n## File preview\n\n文件行为可以定位到对应的内容预览。\n预览显示来源、文件大小与截断状态。\n\nThis is sample content, not a server-side file read.\n` },
  { id: "missing", pid: 42, process: "python", path: "/workspace/local.config.json", requestedPath: "local.config.json", cwd: "/workspace", format: "json", language: "JSON", flags: "O_RDONLY", error: "ENOENT", errno: 2, time: "20:48:03.205", duration: "0.034 ms" },
  { id: "config", pid: 51, process: "agent", path: "/workspace/agent.config.json", requestedPath: "agent.config.json", cwd: "/workspace", format: "json", language: "JSON", flags: "O_RDONLY | O_CLOEXEC", fd: 7, time: "20:48:03.408", duration: "0.065 ms", limit: 4096, content: '{"agent":{"name":"diagnostic-demo","workspace":"/workspace"},"capture":{"http":true,"sse":true,"websocket":true,"max_body_bytes":1048576},"files":{"backend":"overlayfs","preserve_lower":true},"streams":{"retention":"sliding_window","window_bytes":10485760}}\n' },
  { id: "write-only", pid: 51, process: "agent", path: "/workspace/output/report.md", requestedPath: "output/report.md", cwd: "/workspace", format: "text", language: "Markdown", flags: "O_WRONLY | O_CREAT | O_TRUNC", fd: 9, time: "20:48:03.522", duration: "0.105 ms", unavailable: "本示例只记录了写入打开事件，没有采集文件内容。" },
];

function appendText(parent, tag, className, value) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  node.textContent = value;
  parent.append(node);
  return node;
}

export class FilePreview {
  constructor(root, onClose) {
    this.root = root;
    this.onClose = onClose;
    this.selected = null;
    this.tab = "content";
    this.formatted = true;
    root.addEventListener("click", async (event) => {
      const button = event.target.closest("button");
      if (button?.dataset.fileCopy) {
        const footer = root.querySelector(".file-preview-footer");
        const text = button.dataset.fileCopy === "path" ? this.selected.path : this.copyContent;
        button.disabled = true;
        try {
          await copyText(text);
          footer.textContent = button.dataset.fileCopy === "path" ? "路径已复制。" : "当前预览内容已复制；截断内容不包含文件剩余部分。";
        } catch (error) { footer.textContent = error.message; }
        finally { button.disabled = false; }
        return;
      }
      if (button?.hasAttribute("data-file-close")) this.close();
      if (button?.dataset.fileTab) {
        this.tab = button.dataset.fileTab;
        this.render();
        root.querySelector(`[data-file-tab="${this.tab}"]`).focus({ preventScroll: true });
      }
      if (button?.dataset.fileFormat) {
        this.formatted = button.dataset.fileFormat === "pretty";
        this.render();
        root.querySelector(`[data-file-format="${button.dataset.fileFormat}"]`).focus({ preventScroll: true });
      }
    });
    root.addEventListener("keydown", (event) => {
      if (event.key === "Escape") { event.preventDefault(); this.close(); }
    });
  }

  open(id) {
    const file = fileExamples.find((entry) => entry.id === id);
    if (!file) return;
    this.selected = file;
    this.tab = "content";
    this.formatted = true;
    this.root.hidden = false;
    this.render();
    this.root.querySelector("[data-file-close]").focus({ preventScroll: true });
  }

  close() {
    this.root.hidden = true;
    this.selected = null;
    this.onClose();
  }

  render() {
    const file = this.selected;
    this.copyContent = null;
    const data = file.content === undefined ? null : file.content instanceof Uint8Array ? file.content : bytes(file.content);
    const captured = data?.slice(0, file.limit ?? data.length);
    const truncated = data !== null && captured.length < data.length;
    this.root.innerHTML = `<div class="file-preview-heading"><div><span class="eyebrow">FILE PREVIEW</span><h2></h2></div><button class="button quiet" data-file-close aria-label="关闭文件预览">关闭 ×</button></div><p class="file-preview-path"></p><div class="file-preview-meta"></div><nav class="file-preview-tabs" aria-label="文件详情"><button data-file-tab="content">内容预览</button><button data-file-tab="event">打开记录</button></nav><div class="file-preview-content"></div><footer class="file-preview-footer"></footer>`;
    this.root.dataset.fileId = file.id;
    this.root.querySelector("h2").textContent = file.path.split("/").at(-1);
    this.root.querySelector(".file-preview-path").textContent = file.path;
    const copyPath = appendText(this.root.querySelector(".file-preview-heading"), "button", "button quiet", "复制路径");
    copyPath.type = "button";
    copyPath.dataset.fileCopy = "path";
    const meta = this.root.querySelector(".file-preview-meta");
    appendText(meta, "span", file.error ? "file-result failed" : "file-result", file.error ? `打开失败 · ${file.error}` : `打开成功 · FD ${file.fd}`);
    appendText(meta, "span", "", `${file.process} · PID ${file.pid}`);
    appendText(meta, "span", "", data === null ? "内容未采集" : `${file.language} · ${size(data.length)}`);
    for (const button of this.root.querySelectorAll("[data-file-tab]")) {
      const active = button.dataset.fileTab === this.tab;
      button.classList.toggle("active", active);
      button.setAttribute("aria-pressed", String(active));
    }
    const content = this.root.querySelector(".file-preview-content");
    if (this.tab === "event") {
      const values = {
        "事件": "file.open", "进程": `${file.process} · PID ${file.pid}`, "时间": `${file.time} (UTC+08:00)`,
        "系统调用": "openat", "原始路径": file.requestedPath, "工作目录": file.cwd ?? "—", "路径": file.path,
        "打开标志": file.flags, "结果": file.error ? `-1 · errno ${file.errno} (${file.error})` : `FD ${file.fd}`,
        "耗时": file.duration, "预览来源": data === null ? "没有内容记录" : "事件关联的示例内容快照", "数据类型": "模拟数据",
      };
      const table = document.createElement("table");
      table.className = "key-values";
      const tbody = table.createTBody();
      for (const [key, value] of Object.entries(values)) {
        const row = tbody.insertRow();
        const label = appendText(row, "th", "", key);
        label.scope = "row";
        appendText(row, "td", "", value);
      }
      content.append(table);
    } else if (data === null) {
      const empty = appendText(content, "div", "file-preview-empty", "");
      appendText(empty, "span", "file-empty-symbol", file.error ? "!" : "—");
      appendText(empty, "h3", "", file.error ? "打开失败，没有可预览的内容" : "打开成功，内容未采集");
      appendText(empty, "p", "", file.error ? "目标程序打开文件时返回 ENOENT。此事件没有关联内容快照。" : file.unavailable);
    } else {
      const notice = appendText(content, "div", truncated ? "file-capture-note truncated" : "file-capture-note", truncated ? `内容已截断：预览前 ${size(captured.length)}，文件共 ${size(data.length)}。` : `示例内容快照 · 完整预览 ${size(captured.length)}`);
      notice.dataset.capture = truncated ? "truncated" : "complete";
      const toolbar = appendText(content, "div", "file-code-toolbar", "");
      appendText(toolbar, "span", "", file.format === "binary" ? "偏移 / Hex / ASCII" : `${file.language} · UTF-8`);
      const copyContent = appendText(toolbar, "button", "button quiet", file.format === "binary" ? "复制 Hex" : "复制内容");
      copyContent.type = "button";
      copyContent.dataset.fileCopy = "content";
      if (file.format === "json" && !truncated) {
        const controls = document.createElement("div");
        controls.className = "format-switch";
        controls.innerHTML = `<button data-file-format="pretty" class="${this.formatted ? "active" : ""}" aria-pressed="${this.formatted}">格式化</button><button data-file-format="raw" class="${!this.formatted ? "active" : ""}" aria-pressed="${!this.formatted}">原始</button>`;
        toolbar.append(controls);
      }
      if (file.format === "binary") {
        const rows = [];
        for (let offset = 0; offset < captured.length; offset += 16) {
          const part = captured.slice(offset, offset + 16);
          const hex = [...part].map((byte) => byte.toString(16).padStart(2, "0")).join(" ").padEnd(47, " ");
          const ascii = [...part].map((byte) => byte >= 32 && byte <= 126 ? String.fromCharCode(byte) : ".").join("");
          rows.push(`${offset.toString(16).padStart(8, "0")}  ${hex}  |${ascii}|`);
        }
        this.copyContent = rows.join("\n");
        appendText(content, "pre", "file-code file-hex", this.copyContent);
      } else {
        let text = new TextDecoder().decode(captured);
        if (file.format === "json" && this.formatted && !truncated) text = JSON.stringify(JSON.parse(text), null, 2);
        this.copyContent = text;
        const code = appendText(content, "div", "file-code file-lines", "");
        text.replace(/\n$/, "").split("\n").forEach((line, index) => {
          const row = appendText(code, "div", "file-line", "");
          const number = appendText(row, "span", "file-line-number", index + 1);
          number.setAttribute("aria-hidden", "true");
          appendText(row, "span", "file-line-text", line || " ");
        });
      }
    }
    this.root.querySelector(".file-preview-footer").textContent = "仅展示模拟快照，不读取服务器真实文件。";
  }
}
