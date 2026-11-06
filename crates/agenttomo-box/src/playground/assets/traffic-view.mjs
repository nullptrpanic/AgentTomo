import { TrafficModel } from "./traffic-model.mjs";
import { previewContent } from "./file-preview.mjs";
import { copyText } from "./copy-text.mjs";
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
    root.innerHTML = `<div class="traffic-toolbar"><input type="search" aria-label="Filter traffic" placeholder="Filter URL, PID, method or status"><span class="traffic-count">0 requests</span></div><p class="traffic-status"></p><div class="traffic-list" role="list" aria-label="HTTP requests"></div><section class="traffic-inspector" aria-label="HTTP exchange"><p>Select a request to inspect its headers and captured body.</p></section>`;
    this.list = root.querySelector(".traffic-list");
    this.detail = root.querySelector(".traffic-inspector");
    root.querySelector("input").addEventListener("input", () => this.renderList());
    this.list.addEventListener("click", event => {
      const id = Number(event.target.closest("[data-request-id]")?.dataset.requestId);
      if (!this.model.entries.has(id)) return;
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
    if (this.selected && !this.model.entries.has(this.selected)) {
      this.selected = null; this.request?.abort(); this.streamView?.stop(); this.streamView = null; this.version++;
      this.detail.textContent = "This request left the live list. Its recording remains on disk.";
    }
    this.renderList();
    if (row.id === this.selected) { if (event.kind === "network.stream.open") this.side = "messages"; this.renderDetail(); }
  }
  select(event) {
    if (!this.model.entries.has(event.data?.request_id)) return;
    this.selected = event.data.request_id;
    this.side = this.model.entries.get(this.selected).stream ? "messages" : "response";
    this.renderList(); this.renderDetail();
  }
  renderList() {
    const follow = this.list.scrollHeight - this.list.scrollTop - this.list.clientHeight < 32;
    const top = this.list.scrollTop;
    const rows = this.model.visible(this.root.querySelector("input").value);
    this.list.replaceChildren();
    this.root.querySelector(".traffic-count").textContent = `${rows.length} requests${this.model.omitted ? ` · ${this.model.omitted} earlier on disk` : ""}`;
    for (const row of rows) {
      const data = row.request?.data || row.latest.data;
      const button = element(this.list, "button", "traffic-row");
      button.type = "button"; button.dataset.requestId = row.id;
      button.setAttribute("aria-pressed", String(this.selected === row.id));
      element(button, "strong", "traffic-method", row.stream?.data.protocol === "websocket" ? "WS" : row.stream ? "SSE" : data.method || "…");
      element(button, "span", "traffic-url", data.url || "Waiting for request headers…");
      element(button, "span", "traffic-code", row.error ? "Failed" : row.stream ? row.streamClosed ? "Closed" : "Live" : row.response?.data.status_code ?? "Pending");
      const completed = row.stream ? row.streamClosed?.data : row.responseBody?.data;
      const duration = completed?.duration_ns ? ` · ${(completed.duration_ns / 1e6).toFixed(1)} ms` : "";
      const date = new Date(Number(data.started_unix_ns) / 1e6);
      element(button, "span", "traffic-meta", `PID ${data.pid ?? "—"} · ${Number.isNaN(date.getTime()) ? "—" : date.toLocaleTimeString([], { hour12: false })}${duration}${row.responseBody?.data.body.truncated ? " · Truncated" : ""}`);
    }
    if (!rows.length) element(this.list, "p", "traffic-empty", this.enabled ? "No matching HTTP requests yet. Try curl https://example.com in the terminal." : "Waiting for captured HTTP traffic.");
    this.list.scrollTop = follow ? this.list.scrollHeight : top;
  }
  async renderDetail() {
    const row = this.model.entries.get(this.selected);
    if (!row) return;
    if (this.side === "messages" && this.streamView?.requestId === row.id) { this.streamView.update(row); return; }
    const headersOpen = this.detail.querySelector(".traffic-headers")?.open || false;
    this.request?.abort();
    const version = ++this.version;
    this.request = new AbortController();
    this.streamView?.stop(); this.streamView = null;
    this.detail.innerHTML = `<div class="traffic-detail-heading"><strong></strong><button type="button" class="traffic-copy-url">Copy URL</button></div><nav aria-label="Exchange direction"><button type="button" data-side="request">Request</button><button type="button" data-side="response">Response</button></nav><p class="traffic-message-status"></p><details class="traffic-headers"><summary>Headers</summary><pre></pre></details><div class="traffic-body-tools"><strong>Body</strong><button type="button" class="traffic-body-copy" disabled>Copy content</button></div><p class="traffic-body-status" role="status"></p><pre class="traffic-body"></pre>`;
    this.detail.querySelector(".traffic-headers").open = headersOpen;
    const url = row.request?.data.url || "Request pending";
    this.detail.querySelector(".traffic-detail-heading strong").textContent = url;
    const status = this.detail.querySelector(".traffic-body-status");
    this.detail.querySelector(".traffic-copy-url").onclick = async () => {
      try { await copyText(url); if (version === this.version) (this.detail.querySelector(".stream-payload-status") || status).textContent = "URL copied."; } catch (error) { if (version === this.version) (this.detail.querySelector(".stream-payload-status") || status).textContent = error.message; }
    };
    if (row.stream) { const button = element(this.detail.querySelector("nav"), "button", "", "Messages"); button.type = "button"; button.dataset.side = "messages"; }
    for (const button of this.detail.querySelectorAll("[data-side]")) {
      button.setAttribute("aria-pressed", String(button.dataset.side === this.side));
      button.onclick = () => { this.side = button.dataset.side; this.renderDetail(); };
    }
    if (this.side === "messages" && row.stream) {
      for (const node of this.detail.querySelectorAll(".traffic-message-status,.traffic-headers,.traffic-body-tools,.traffic-body-status,.traffic-body")) node.remove();
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
      const bytes = await this.load(body, this.request.signal);
      if (version !== this.version) return;
      const content = previewContent(bytes, { pretty: true, truncated: body.truncated || !body.complete });
      this.detail.querySelector(".traffic-body").textContent = content.text || "(empty body)";
      const encoding = header?.data.headers?.find(([name]) => name.toLowerCase() === "content-encoding")?.[1];
      status.textContent = `${body.status} · retained ${body.captured_bytes} / observed ${body.observed_bytes} B${body.limit_reason ? ` · ${body.limit_reason}` : ""} · ${content.format === "hex" ? "Hex / ASCII" : content.json ? "JSON" : "UTF-8"}${encoding ? ` · Content-Encoding: ${encoding}; stored without decompression` : ""}${content.incompleteUtf8 ? " · UTF-8 character cut at capture limit" : ""}`;
      const copy = this.detail.querySelector(".traffic-body-copy"); copy.disabled = false;
      copy.onclick = async () => {
        try { await copyText(content.text); if (version === this.version) status.textContent = body.truncated ? "Captured prefix copied; later bytes were not saved." : "Displayed body copied."; }
        catch (error) { if (version === this.version) status.textContent = error.message; }
      };
    } catch (error) {
      if (version === this.version) status.textContent = `Body unavailable: ${error.message}`;
    }
  }
}
