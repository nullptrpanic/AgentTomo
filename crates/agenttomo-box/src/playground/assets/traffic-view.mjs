import { TrafficModel } from "./traffic-model.mjs";
import { contentEncoding, httpPreview } from "./http-preview.mjs";
import { copyText } from "./copy-text.mjs";
import { copyIcon } from "./icons.mjs";
import { StreamView } from "./stream-view.mjs";

const element = (parent, tag, className, text = "") => {
  const node = document.createElement(tag);
  node.className = className;
  node.textContent = text;
  parent.append(node);
  return node;
};
const headerText = headers => (headers || []).map(([name, value]) => `${name}: ${value}`).join("\n");

export class TrafficView {
  constructor(root, load, onSelect, loadStream) {
    this.root = root; this.load = load; this.onSelect = onSelect;
    this.loadStream = loadStream;
    this.model = new TrafficModel(); this.side = "response"; this.version = 0;
    this.bodyMode = "decoded";
    this.rows = new Map(); this.scheduled = false; this.detailDirty = false;
    this.connectionId = null;
    root.innerHTML = `<div class="traffic-toolbar"><input type="search" aria-label="Filter traffic" placeholder="Filter URL, PID, method or status"><span class="traffic-count">0 requests</span></div><div class="traffic-navigation" hidden><span></span><button type="button" class="traffic-show-all">Show all requests</button></div><p class="traffic-status"></p><div class="traffic-list" role="list" aria-label="HTTP requests"></div><section class="traffic-inspector" aria-label="HTTP exchange"><p>Select a request to inspect its headers and captured body.</p></section>`;
    this.list = root.querySelector(".traffic-list");
    this.empty = document.createElement("p"); this.empty.className = "traffic-empty";
    this.detail = root.querySelector(".traffic-inspector");
    root.querySelector("input").addEventListener("input", () => this.renderList());
    root.querySelector(".traffic-show-all").addEventListener("click", () => {
      this.connectionId = null; this.target = null;
      root.querySelector(".traffic-navigation").hidden = true;
      this.renderList();
    });
    this.list.addEventListener("click", event => {
      const id = Number(event.target.closest("[data-request-id]")?.dataset.requestId);
      if (!this.model.entries.has(id)) return;
      this.target = null;
      this.selected = id; this.detail.scrollTop = 0; this.side = this.model.entries.get(id).stream ? "messages" : "response"; this.renderList(); this.renderDetail();
    });
  }
  setEnabled(enabled) {
    this.enabled = enabled;
    this.root.querySelector(".traffic-status").textContent = enabled
      ? "HTTP/1.1 · TLS decrypted with task CA · Body capture is bounded; forwarding continues."
      : "Body capture is off. Start Playground with --capture-http to inject a task CA and capture HTTP/HTTPS.";
  }
  add(event) {
    const row = this.model.add(event);
    if (!row) return;
    if (this.target && this.selected == null && this.model.matching(this.target).some(match => match.id === row.id)) {
      this.select(this.target);
      return;
    }
    if (this.selected && !this.model.entries.has(this.selected)) {
      this.selected = null; this.request?.abort(); this.streamView?.stop(); this.streamView = null; this.version++;
      this.detail.textContent = "This request left the live list. Its recording remains on disk.";
    }
    if (row.id === this.selected) {
      if (event.kind === "network.stream.open") this.side = "messages";
      this.detailDirty = true;
    }
    this.schedule();
  }
  schedule() {
    if (this.scheduled) return;
    this.scheduled = true;
    requestAnimationFrame(() => {
      this.scheduled = false;
      this.renderList();
      if (this.detailDirty) this.renderDetail();
    });
  }
  select(event) {
    this.target = event;
    this.connectionId = !Number.isSafeInteger(event.data?.request_id) && Number.isSafeInteger(event.data?.connection_id) ? event.data.connection_id : null;
    this.root.querySelector("input").value = "";
    const navigation = this.root.querySelector(".traffic-navigation");
    navigation.hidden = this.connectionId === null;
    navigation.querySelector("span").textContent = `Connection #${this.connectionId}`;
    const matches = this.model.matching(event);
    this.selected = (matches.find(row => row.id === this.selected) || matches[0])?.id ?? null;
    this.request?.abort(); this.streamView?.stop(); this.streamView = null; this.version++;
    this.detailDirty = false;
    if (this.selected === null) {
      this.renderList();
      this.detail.textContent = "No captured HTTP exchange is available for this event. It may not contain HTTP, may have failed before request headers, or may have left the live list. New matching requests will appear here. The original event remains in Selected event below.";
      return;
    }
    this.side = this.model.entries.get(this.selected).stream ? "messages" : "response";
    this.detail.scrollTop = 0;
    this.renderList(); this.renderDetail();
    this.rows.get(this.selected)?.button.scrollIntoView({ block: "nearest" });
  }
  renderList() {
    const follow = this.list.scrollHeight - this.list.scrollTop - this.list.clientHeight < 32;
    const top = this.list.scrollTop;
    const rows = this.model.visible(this.root.querySelector("input").value, this.connectionId);
    for (const [id, view] of this.rows) {
      if (!this.model.entries.has(id)) { view.button.remove(); this.rows.delete(id); }
    }
    const visible = new Set(rows.map(row => row.id));
    for (const [id, view] of this.rows) if (!visible.has(id)) view.button.remove();
    this.empty.remove();
    this.root.querySelector(".traffic-count").textContent = `${rows.length} requests${this.model.omitted ? ` · ${this.model.omitted} earlier on disk` : ""}`;
    let cursor = this.list.firstChild;
    for (const row of rows) {
      let view = this.rows.get(row.id);
      if (!view) {
        const button = document.createElement("button");
        button.type = "button"; button.className = "traffic-row"; button.dataset.requestId = row.id;
        view = {
          button,
          method: element(button, "strong", "traffic-method"),
          url: element(button, "span", "traffic-url"),
          code: element(button, "span", "traffic-code"),
          meta: element(button, "span", "traffic-meta"),
        };
        this.rows.set(row.id, view);
      }
      view.button.setAttribute("aria-pressed", String(this.selected === row.id));
      if (view.latest !== row.latest) {
        const data = row.request?.data || row.latest.data;
        view.method.textContent = row.stream?.data.protocol === "websocket" ? "WS" : row.stream ? "SSE" : data.method || "…";
        view.url.textContent = data.url || "Waiting for request headers…";
        view.code.textContent = row.error ? "Failed" : row.stream ? row.streamClosed ? "Closed" : "Live" : row.response?.data.status_code ?? "Pending";
        const completed = row.stream ? row.streamClosed?.data : row.responseBody?.data;
        const duration = completed?.duration_ns ? ` · ${(completed.duration_ns / 1e6).toFixed(1)} ms` : "";
        const date = new Date(Number(data.started_unix_ns) / 1e6);
        view.meta.textContent = `PID ${data.pid ?? "—"} · ${Number.isNaN(date.getTime()) ? "—" : date.toLocaleTimeString([], { hour12: false })}${duration}${row.responseBody?.data.body.truncated ? " · Truncated" : ""}`;
        view.latest = row.latest;
      }
      if (view.button !== cursor) this.list.insertBefore(view.button, cursor);
      cursor = view.button.nextSibling;
    }
    if (!rows.length) {
      this.empty.textContent = this.enabled ? "No matching HTTP requests yet. Try curl https://example.com in the terminal." : "Waiting for captured HTTP traffic.";
      this.list.append(this.empty);
    }
    this.list.scrollTop = follow ? this.list.scrollHeight : top;
  }
  async renderDetail() {
    this.detailDirty = false;
    const row = this.model.entries.get(this.selected);
    if (!row) return;
    if (this.side === "messages" && this.streamView?.requestId === row.id) { this.streamView.update(row); return; }
    const headersOpen = this.detail.querySelector(".traffic-headers")?.open || false;
    const bodyOpen = this.detail.querySelector(".traffic-body-section")?.open ?? true;
    this.request?.abort();
    const version = ++this.version;
    this.request = new AbortController();
    this.streamView?.stop(); this.streamView = null;
    this.detail.innerHTML = `<div class="traffic-detail-heading"><strong></strong></div><p class="traffic-copy-status" role="status" hidden></p><nav aria-label="Exchange direction"><button type="button" data-side="request">Request</button><button type="button" data-side="response">Response</button></nav><p class="traffic-message-status"></p><details class="traffic-headers traffic-section"><summary><span class="traffic-section-title">Headers</span></summary><pre></pre></details><details class="traffic-body-section traffic-section"><summary class="traffic-body-tools"><span class="traffic-section-title traffic-body-title">Body</span><span class="traffic-body-modes" role="group" aria-label="Body representation"><button type="button" data-body-mode="decoded">Decoded</button><button type="button" data-body-mode="raw">Raw / Hex</button></span><button type="button" class="traffic-body-copy traffic-icon" aria-label="Copy content" title="Copy content" disabled>${copyIcon}</button></summary><p class="traffic-body-status" role="status"></p><pre class="traffic-body"></pre></details>`;
    this.detail.querySelector(".traffic-headers").open = headersOpen;
    this.detail.querySelector(".traffic-body-section").open = bodyOpen;
    const url = row.request?.data.url || "Request pending";
    this.detail.querySelector(".traffic-detail-heading strong").textContent = url;
    const status = this.detail.querySelector(".traffic-body-status");
    const copyStatus = this.detail.querySelector(".traffic-copy-status");
    if (row.stream) { const button = element(this.detail.querySelector("nav"), "button", "", "Messages"); button.type = "button"; button.dataset.side = "messages"; }
    for (const button of this.detail.querySelectorAll("[data-side]")) {
      button.setAttribute("aria-pressed", String(button.dataset.side === this.side));
      button.onclick = () => { this.side = button.dataset.side; this.renderDetail(); };
    }
    for (const button of this.detail.querySelectorAll("[data-body-mode]")) {
      button.setAttribute("aria-pressed", String(button.dataset.bodyMode === this.bodyMode));
      button.onclick = () => { this.bodyMode = button.dataset.bodyMode; this.renderDetail(); };
    }
    if (this.side === "messages" && row.stream) {
      for (const node of this.detail.querySelectorAll(".traffic-message-status,.traffic-headers,.traffic-body-section")) node.remove();
      const root = element(this.detail, "div", "stream-view");
      this.streamView = new StreamView(root, row, this.loadStream, this.onSelect);
      return;
    }
    const header = row[this.side], captured = row[`${this.side}Body`];
    const selectedEvent = captured || header || row.latest;
    this.onSelect(selectedEvent);
    this.detail.querySelector(".traffic-headers pre").textContent = header ? headerText(header.data.headers) || "No headers" : "Waiting for headers…";
    const message = this.detail.querySelector(".traffic-message-status");
    message.textContent = row.error ? row.error.data.error : header ? `${header.data.version} · ${this.side === "request" ? header.data.method : header.data.status_code}` : "Exchange in progress…";
    const body = captured?.data.body;
    if (body?.status === "stream_window" || this.side === "response" && row.stream?.data.protocol === "sse") { status.textContent = "SSE content is retained in the sliding window. Open Messages for live events."; return; }
    if (!body) { status.textContent = row.error ? "Exchange failed; body capture may be incomplete." : "Streaming. The retained prefix becomes available when this message ends."; return; }
    if (["unavailable", "disabled"].includes(body.status)) { status.textContent = `Body ${body.status}${body.error ? `: ${body.error}` : ""}.`; return; }
    status.textContent = "Loading saved body…";
    try {
      const raw = this.bodyMode === "raw";
      const payload = await this.load(body, this.request.signal, raw || body.captured_bytes === 0 ? "" : contentEncoding(header?.data.headers));
      if (version !== this.version) return;
      const content = httpPreview(payload, header?.data.headers, body, raw);
      this.detail.querySelector(".traffic-body").textContent = content.text || (content.partial ? "(no content retained in this preview)" : "(empty body)");
      status.textContent = content.status;
      const copy = this.detail.querySelector(".traffic-body-copy"); copy.disabled = false;
      copy.title = content.format === "hex" ? "Copy hex" : "Copy content";
      copy.setAttribute("aria-label", copy.title);
      copy.onclick = async () => {
        let message = content.partial ? "Displayed prefix copied; content is incomplete." : "Displayed body copied.";
        try { await copyText(content.text); } catch (error) { message = error.message; }
        if (version === this.version) { copyStatus.textContent = message; copyStatus.hidden = false; }
      };
    } catch (error) {
      if (version === this.version) status.textContent = `Body unavailable: ${error.message}`;
    }
  }
}
