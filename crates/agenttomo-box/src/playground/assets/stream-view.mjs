import { StreamModel } from "./stream-model.mjs";
import { previewContent } from "./file-preview.mjs";
import { copyText } from "./copy-text.mjs";

export class StreamView {
  constructor(root, row, load, onSelect) {
    this.root = root; this.requestId = row.id; this.row = row; this.load = load; this.onSelect = onSelect;
    this.model = new StreamModel(); this.active = true; this.version = 0;
    root.innerHTML = `<div class="stream-toolbar"><strong class="stream-state"></strong><button type="button" class="stream-follow" aria-pressed="true">Follow latest</button></div><p class="stream-retention" role="status">Loading recent messages…</p><div class="stream-list" role="list" aria-label="Stream messages"></div><div class="traffic-body-tools"><strong>Message</strong><button type="button" class="stream-copy" disabled>Copy content</button></div><p class="stream-payload-status" role="status"></p><pre class="traffic-body stream-payload"></pre>`;
    this.list = root.querySelector(".stream-list");
    this.follow = root.querySelector(".stream-follow");
    this.follow.onclick = () => { this.model.latest(); this.renderMessages(); this.renderPayload(); };
    this.list.addEventListener("scroll", () => {
      this.model.follow = this.list.scrollHeight - this.list.scrollTop - this.list.clientHeight < 24;
      this.follow.setAttribute("aria-pressed", String(this.model.follow));
    });
    this.list.onclick = event => {
      const button = event.target.closest("[data-message-id]");
      if (!button) return;
      this.model.select(Number(button.dataset.messageId)); this.renderMessages(); this.renderPayload();
    };
    this.update(row);
    this.poll();
  }
  update(row) {
    this.row = row;
    this.root.querySelector(".stream-state").textContent = `${row.stream?.data.protocol === "sse" ? "SSE events" : "WebSocket messages"} · ${row.streamClosed?.data.error ? `Capture error: ${row.streamClosed.data.error}` : row.streamClosed ? (row.streamClosed.data.complete ? "Closed" : "Interrupted") : "Live"}`;
  }
  async poll() {
    if (!this.active) return;
    if (this.root.getClientRects().length) {
      this.pollRequest = new AbortController();
      try {
        const snapshot = await this.load(this.requestId, null, this.pollRequest.signal);
        if (!this.active) return;
        const previous = this.model.selected;
        const expired = this.model.update(snapshot);
        const signature = `${this.model.messages.length}:${this.model.messages[0]?.id}:${this.model.messages.at(-1)?.id}`;
        if (signature !== this.signature) { this.signature = signature; this.renderMessages(); }
        const limits = snapshot.limits;
        this.root.querySelector(".stream-retention").textContent = `${this.model.messages.length} recent messages · run window ${limits.window_bytes} B / ${limits.window_messages} messages · ${snapshot.evicted} evicted · ${snapshot.dropped} dropped under load${limits.message_bytes === 0 || limits.window_bytes === 0 ? " · retention disabled" : ""}`;
        if (previous !== this.model.selected || this.loaded === undefined) this.renderPayload(expired);
      } catch (error) {
        if (this.active && error.name !== "AbortError") this.root.querySelector(".stream-retention").textContent = `Messages unavailable: ${error.message}. Retrying…`;
      }
    }
    if (this.active) this.timer = setTimeout(() => this.poll(), 250);
  }
  renderMessages() {
    const top = this.list.scrollTop;
    const fragment = document.createDocumentFragment();
    for (const message of this.model.messages) {
      const button = document.createElement("button");
      button.type = "button"; button.className = "stream-message"; button.dataset.messageId = message.id;
      button.setAttribute("aria-pressed", String(message.id === this.model.selected));
      const time = new Date(Number(message.started_unix_ns) / 1e6);
      const fields = [message.direction === "upstream" ? "↑ Sent" : "↓ Received", Number.isNaN(time.getTime()) ? "—" : `${time.toLocaleTimeString([], { hour12: false })}.${String(time.getMilliseconds()).padStart(3, "0")}`, message.event || message.kind, `${message.observed_bytes ?? 0} B${message.truncated ? " · truncated" : ""}${message.complete === false ? " · incomplete" : ""}`];
      for (const [index, text] of fields.entries()) { const span = document.createElement("span"); span.className = `stream-field-${index}`; span.textContent = text; button.append(span); }
      fragment.append(button);
    }
    this.list.replaceChildren(fragment);
    this.follow.setAttribute("aria-pressed", String(this.model.follow));
    this.list.scrollTop = this.model.follow ? this.list.scrollHeight : top;
  }
  async renderPayload(expired = false) {
    this.payloadRequest?.abort(); const version = ++this.version;
    this.payloadRequest = new AbortController();
    const message = this.model.messages.find(message => message.id === this.model.selected);
    const status = this.root.querySelector(".stream-payload-status"), body = this.root.querySelector(".stream-payload"), copy = this.root.querySelector(".stream-copy");
    this.loaded = message?.id ?? null; copy.disabled = true; copy.onclick = null; body.textContent = "";
    if (!message) { status.textContent = expired ? "Selected message expired from the sliding window." : "Waiting for messages…"; return; }
    this.onSelect({ kind: "network.stream.message", category: "network", source: "tcp_proxy", data: message });
    if (message.kind === "inspection_error") { status.textContent = message.error; return; }
    status.textContent = "Loading retained message…";
    try {
      const bytes = await this.load(this.requestId, message.id, this.payloadRequest.signal);
      if (!this.active || version !== this.version) return;
      const content = previewContent(bytes, { pretty: true, truncated: message.truncated || message.complete === false, forceHex: message.kind === "binary" || message.decoded === false });
      body.textContent = content.text || "(empty message)";
      status.textContent = `${message.kind} · ${message.captured_bytes} / ${message.observed_bytes} B retained${message.event_id ? ` · id ${message.event_id}` : ""}${message.frames ? ` · ${message.frames} frame(s)` : ""}${message.close_code ? ` · close ${message.close_code}` : ""}${message.decoded === false ? " · extension bytes; not decoded" : message.compressed ? " · decompressed" : ""}${message.truncated ? " · prefix only" : ""}${message.complete === false ? " · incomplete" : ""}`;
      copy.disabled = false;
      copy.onclick = async () => { try { await copyText(content.text); if (this.active && version === this.version) status.textContent = "Displayed message copied."; } catch (error) { if (this.active && version === this.version) status.textContent = error.message; } };
    } catch (error) {
      if (this.active && version === this.version) status.textContent = error.status === 410 ? "Message expired from the sliding window." : `Message unavailable: ${error.message}`;
    }
  }
  stop() { this.active = false; ++this.version; clearTimeout(this.timer); this.pollRequest?.abort(); this.payloadRequest?.abort(); }
}
