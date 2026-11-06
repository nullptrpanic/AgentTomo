// Presentation prototype: all traffic is generated in this browser. No requests
// are made to the example hosts and no real terminal commands are executed.
import { FilePreview, fileExamples } from "./file-preview.js";

const $ = (id) => document.getElementById(id);
const escape = (value) => String(value).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
const encoder = new TextEncoder();
const byteSize = (text) => encoder.encode(text).length;
const formatBytes = (n) => n < 1024 ? `${n} B` : `${(n / 1024).toFixed(1)} KiB`;
const clock = (ms) => new Date(ms).toLocaleTimeString("zh-CN", { hour12: false, timeZone: "Asia/Shanghai" }) + `.${String(ms % 1000).padStart(3, "0")}`;
const commonHeaders = { "user-agent": "AgentTomo-Demo/0.1", "accept": "application/json" };
const requests = [
  { id: "http-docs", n: 1, pid: 35, process: "curl", command: "curl https://docs.example.test/guide/quickstart", method: "GET", type: "http", host: "docs.example.test", path: "/guide/quickstart", status: 200, duration: 146, bytes: 13642, destination: "203.0.113.12:443", requestHeaders: { "user-agent": "curl/8.5.0", "accept": "text/html" }, responseHeaders: { "content-type": "text/html; charset=utf-8", "cache-control": "max-age=3600", "server": "example-gateway" }, responseBody: "<!doctype html>\n<html lang=\"en\">\n  <title>Project quickstart</title>\n  <main>\n    <h1>Getting started</h1>\n    <p>Install dependencies and run the example agent.</p>\n  </main>\n</html>" },
  { id: "http-search", n: 2, pid: 42, process: "python", command: "python tools.py", method: "POST", type: "http", host: "search.example.test", path: "/v1/search?q=overlayfs&limit=3", status: 200, duration: 284, bytes: 2180, destination: "203.0.113.21:443", requestHeaders: { ...commonHeaders, "content-type": "application/json" }, requestBody: { query: "overlayfs CA injection", limit: 3 }, responseHeaders: { "content-type": "application/json", "x-request-id": "demo-search-002" }, responseBody: { results: [{ title: "Overlay filesystem", url: "https://docs.example.test/overlayfs", excerpt: "Combine an upper and lower directory into one filesystem view." }, { title: "Certificate trust", url: "https://docs.example.test/tls", excerpt: "Trust an additional CA without replacing existing roots." }], count: 2 } },
  { id: "sse-response", n: 3, pid: 51, process: "agent", command: "python agent_demo.py", method: "POST", type: "sse", host: "api.example.test", path: "/v1/responses", status: 200, duration: null, bytes: 0, destination: "203.0.113.8:443", requestHeaders: { ...commonHeaders, "content-type": "application/json", "accept": "text/event-stream", "authorization": "Bearer demo-key-not-a-real-secret" }, requestBody: { model: "agent-demo", input: "阅读项目文档，给出 CA 注入与网络诊断方案。", stream: true }, responseHeaders: { "content-type": "text/event-stream", "cache-control": "no-cache", "connection": "keep-alive", "x-request-id": "demo-stream-003" } },
  { id: "ws-tools", n: 4, pid: 42, process: "python", command: "python tools.py", method: "GET", type: "ws", host: "tools.example.test", path: "/events", status: 101, duration: null, bytes: 0, destination: "203.0.113.18:443", requestHeaders: { "host": "tools.example.test", "connection": "Upgrade", "upgrade": "websocket", "sec-websocket-version": "13", "sec-websocket-protocol": "agent-tools" }, responseHeaders: { "connection": "Upgrade", "upgrade": "websocket", "sec-websocket-protocol": "agent-tools" } },
  { id: "http-config", n: 5, pid: 51, process: "agent", command: "python agent_demo.py", method: "GET", type: "http", host: "api.example.test", path: "/v1/config", status: 200, duration: 42, bytes: 526, destination: "203.0.113.8:443", requestHeaders: commonHeaders, responseHeaders: { "content-type": "application/json", "etag": '"demo-config-v1"' }, responseBody: { tracing: { process: true, network: true, files: true }, network: { capture: true, body_limit: 1048576, stream_retention: "sliding_window" }, sandbox: { filesystem: "overlayfs" } } },
  { id: "http-notfound", n: 6, pid: 35, process: "curl", command: "curl https://docs.example.test/old-guide", method: "GET", type: "http", host: "docs.example.test", path: "/old-guide", status: 404, duration: 61, bytes: 184, destination: "203.0.113.12:443", requestHeaders: { "user-agent": "curl/8.5.0" }, responseHeaders: { "content-type": "application/json" }, responseBody: { error: { code: "not_found", message: "This page has moved to /guide/quickstart." } } },
  { id: "http-upload", n: 7, pid: 42, process: "python", command: "python tools.py", method: "POST", type: "http", host: "api.example.test", path: "/v1/artifacts", status: 201, duration: 193, bytes: 380, destination: "203.0.113.8:443", requestHeaders: { ...commonHeaders, "content-type": "application/json" }, requestBody: { name: "analysis.md", content: "Use an isolated OverlayFS view to extend the existing CA bundle." }, responseHeaders: { "content-type": "application/json", "location": "/v1/artifacts/demo-42" }, responseBody: { id: "demo-42", name: "analysis.md", status: "created" } },
  { id: "http-binary", n: 8, pid: 35, process: "curl", command: "curl https://docs.example.test/assets/sample.bin", method: "GET", type: "http", host: "docs.example.test", path: "/assets/sample.bin", status: 200, duration: 88, bytes: 64, destination: "203.0.113.12:443", requestHeaders: { "user-agent": "curl/8.5.0", "accept": "application/octet-stream" }, responseHeaders: { "content-type": "application/octet-stream", "content-length": "64" }, binary: true, responseBody: "00000000  41 67 65 6e 74 54 6f 6d 6f 00 01 02 03 04 05 06  |AgentTomo.......|\n00000010  10 20 30 40 50 60 70 80 90 a0 b0 c0 d0 e0 f0 00  |. 0@P`p.........|\n00000020  64 65 6d 6f 2d 74 72 61 66 66 69 63 2d 64 61 74  |demo-traffic-dat|\n00000030  61 00 01 02 03 04 05 06 07 08 09 0a 0b 0c 0d 0e  |a..............|" },
];
const byId = new Map(requests.map((request) => [request.id, request]));
const state = { pane: "traffic", selected: "sse-response", tab: "messages", query: "", pid: "all", protocol: "all", grouped: false, terminal: matchMedia("(min-width:821px)").matches, playing: true, follow: true, direction: "all", selectedMessage: null, windowBytes: 2048, format: "pretty", messageFormat: "pretty" };
const streams = new Map();
let fileTrigger = null;
const filePreview = new FilePreview($("file-preview"), () => {
  $("process-workspace").classList.remove("preview-open");
  document.querySelectorAll(".selected-file").forEach((row) => row.classList.remove("selected-file"));
  fileTrigger?.focus({ preventScroll: true });
});
let started = Date.now() - 14000;
let ticks = 0;

function makeStream() { return { messages: [], next: 1, bytes: 0, totalBytes: 0, totalCount: 0, evicted: 0 }; }
function trim(stream) {
  while (stream.bytes > state.windowBytes && stream.messages.length) {
    const removed = stream.messages.shift();
    stream.bytes -= removed.bytes;
    stream.evicted++;
  }
}
function append(id, direction, event, data, time = Date.now()) {
  const stream = streams.get(id);
  const sequence = stream.next++;
  const text = typeof data === "string" ? data : JSON.stringify(data);
  const wire = id === "sse-response" ? `id: evt-${sequence}\nevent: ${event}\ndata: ${text}\n\n` : text;
  const summary = typeof data === "string" ? data
    : data.delta ?? data.message ?? data.result?.text
      ?? (data.params ? `${data.params.name} · ${data.params.path}` : `${data.id} · ${data.status ?? event}`);
  const message = { sequence, time, direction, event, text, wire, summary, bytes: byteSize(wire) };
  stream.messages.push(message);
  stream.bytes += message.bytes;
  stream.totalBytes += message.bytes;
  stream.totalCount++;
  trim(stream);
}
const tokens = ["正在阅读项目文档。", "首先确认现有 CA 信任链。", "保留系统原有根证书。", "将诊断 CA 追加到替代文件。", "在 upper 中准备文件覆盖。", "启动程序并验证 TLS 连接。", "请求和响应在代理内采集。", "SSE 消息按事件边界展示。", "长连接保留最近一段消息。", "旧消息随窗口推进被淘汰。", "现在检查工具调用结果。", "准备下一轮诊断。"];
function generate(time = Date.now()) {
  const token = tokens[ticks % tokens.length];
  append("sse-response", "down", "response.output_text.delta", { type: "response.output_text.delta", delta: token, output_index: 0 }, time);
  const job = Math.floor(ticks / 4) + 1;
  const phases = [
    ["up", "tool.call", { id: `call-${job}`, method: "tools/call", params: { name: "read_file", path: "README.md" } }],
    ["down", "tool.progress", { id: `call-${job}`, event: "progress", message: "Reading README.md", progress: 0.5 }],
    ["down", "tool.result", { id: `call-${job}`, result: { text: "AgentTomo: process, file and network diagnostics.", bytes: 4826 } }],
    ["up", "tool.ack", { id: `call-${job}`, method: "ack", status: "received" }],
  ];
  const [direction, event, data] = phases[ticks % phases.length];
  append("ws-tools", direction, event, data, time + 12);
  ticks++;
  $("terminal-token").textContent = token;
}
function seed() {
  streams.clear();
  streams.set("sse-response", makeStream());
  streams.set("ws-tools", makeStream());
  ticks = 0;
  started = Date.now() - 14000;
  for (let i = 0; i < 12; i++) generate(started + i * 1050);
}
function url(request) { return `${request.type === "ws" ? "wss" : "https"}://${request.host}${request.path}`; }
function requestBytes(request) { return streams.get(request.id)?.totalBytes ?? request.bytes; }
function statusClass(request) { return request.status >= 400 ? "error" : request.status === 101 ? "upgrade" : ""; }
function protocolName(request) { return { http: "HTTPS", sse: "SSE", ws: "WS" }[request.type]; }
function filteredRequests() {
  return requests.filter((r) => (state.protocol === "all" || r.type === state.protocol) && (state.pid === "all" || String(r.pid) === state.pid) && `${r.host}${r.path} ${r.method} ${r.pid} ${r.status}`.toLowerCase().includes(state.query.toLowerCase()));
}
function renderRequests() {
  const list = filteredRequests();
  if (state.grouped) list.sort((a, b) => a.host.localeCompare(b.host) || a.n - b.n);
  let group = "";
  $("request-list").innerHTML = list.map((r) => {
    let heading = "";
    if (state.grouped && r.host !== group) {
      group = r.host;
      heading = `<tr class="host-group"><td colspan="7">⌄ &nbsp; ${escape(group)}</td></tr>`;
    }
    return `${heading}<tr data-request="${r.id}" class="${state.selected === r.id ? "selected" : ""}"><td class="number-cell">${String(r.n).padStart(2, "0")}</td><td><span class="method ${r.method.toLowerCase()}">${r.method}</span></td><td><button class="request-url" data-select="${r.id}" aria-label="查看 ${escape(r.method + " " + url(r))}" aria-pressed="${state.selected === r.id}"><span class="protocol-tag ${r.type}">${protocolName(r)}</span><span class="target"><span class="request-host">${r.host}</span><span class="request-path">${escape(r.path)}</span></span></button></td><td class="pid-cell muted">${r.pid}</td><td><span class="status-code ${statusClass(r)}">${r.status}</span>${r.duration === null ? '<span class="tiny-live"></span>' : ""}</td><td class="size-cell numeric row-time" data-bytes="${r.id}">${formatBytes(requestBytes(r))}</td><td class="time-cell numeric row-time ${r.duration === null ? "running-time" : ""}">${r.duration === null ? "持续中" : `${r.duration} ms`}</td></tr>`;
  }).join("");
  $("empty-requests").hidden = list.length > 0;
  $("request-count").textContent = `${list.length} / ${requests.length} 个请求`;
  document.querySelectorAll("[data-protocol]").forEach((button) => {
    button.classList.toggle("active", button.dataset.protocol === state.protocol);
    button.setAttribute("aria-pressed", String(button.dataset.protocol === state.protocol));
  });
  $("group-toggle").setAttribute("aria-pressed", String(state.grouped));
  $("group-toggle").textContent = state.grouped ? "切换时间顺序" : "按域名分组";
}
function keyValues(values) {
  return `<table class="key-values"><tbody>${Object.entries(values).map(([key, value]) => `<tr><th scope="row">${escape(key)}</th><td>${escape(value)}</td></tr>`).join("")}</tbody></table>`;
}
function code(value, raw = false) {
  return typeof value === "string" ? value : JSON.stringify(value, null, raw ? 0 : 2);
}
function selectRequest(id, clearFilters = false) {
  if (!byId.has(id)) return;
  state.selected = id;
  state.pane = "traffic";
  state.tab = byId.get(id).type === "http" ? "response" : "messages";
  state.selectedMessage = null;
  state.follow = true;
  state.direction = "all";
  if (clearFilters) {
    state.query = "";
    state.pid = "all";
    state.protocol = "all";
    $("traffic-search").value = "";
    $("pid-filter").value = "all";
  }
  renderPane();
  renderRequests();
  renderDetail();
  $("announcement").textContent = `已选择 ${byId.get(id).method} ${url(byId.get(id))}`;
}
function renderDetail() {
  const r = byId.get(state.selected);
  $("detail-method").textContent = r.method;
  $("detail-method").className = `method ${r.method.toLowerCase()}`;
  $("detail-url").textContent = url(r);
  $("detail-meta").innerHTML = `<span class="status-code ${statusClass(r)}">${r.status} ${r.status === 101 ? "Switching Protocols" : r.status === 404 ? "Not Found" : r.status === 201 ? "Created" : "OK"}</span><button data-show-process="${r.pid}">${r.process} · PID ${r.pid} ↗</button><span>连接 #${r.n} · 请求 #${String(r.n).padStart(3, "0")}</span><span>${r.type === "http" ? "已完成" : "● 连接中"}</span>`;
  document.querySelectorAll("[data-detail]").forEach((button) => {
    button.hidden = button.dataset.detail === "messages" && r.type === "http";
    button.classList.toggle("active", button.dataset.detail === state.tab);
    button.setAttribute("aria-pressed", String(button.dataset.detail === state.tab));
  });
  if (state.tab === "messages") { renderMessageShell(r); return; }
  const content = $("detail-content");
  if (state.tab === "overview") {
    content.innerHTML = `<div class="detail-scroll"><div class="overview-grid"><section class="info-section"><h3>请求</h3>${keyValues({ "URL": url(r), "方法": r.method, "协议": r.type === "ws" ? "WebSocket over TLS" : "HTTP/1.1 over TLS", "状态": `${r.status}`, "耗时": r.duration === null ? "持续接收中" : `${r.duration} ms` })}</section><section class="info-section"><h3>所属进程与连接</h3>${keyValues({ "进程": `${r.process} · PID ${r.pid}`, "启动参数": r.command, "目标地址": r.destination, "连接 ID": r.n, "请求 ID": `demo-${r.n}`, "TLS": "TLS 1.3 · 诊断 CA（模拟）" })}</section><section class="info-section"><h3>内容记录</h3>${keyValues({ "策略": r.type === "http" ? "单条大小上限" : "滑动窗口", "状态": r.type === "http" ? "已完成 · 示例内容" : "持续记录，淘汰最旧消息", "来源": "浏览器本地模拟数据", "磁盘写入": "本 Demo 不写入采集记录" })}</section></div></div>`;
    return;
  }
  const response = state.tab === "response";
  const headers = response ? r.responseHeaders : r.requestHeaders;
  const body = response ? r.responseBody : r.requestBody;
  let bodyHTML;
  if (response && r.type !== "http") {
    bodyHTML = `<div class="response-live"><span class="live-dot"></span><div><strong>${r.type === "sse" ? "事件流正在接收" : "已升级为双向 WebSocket"}</strong><p>${r.type === "sse" ? "每个完整事件到达后即可查看，无需等待响应结束。" : "请求与响应之后的双向消息，按时间排列。"}</p></div><button class="button" data-open-messages>查看实时消息 ↗</button></div>`;
  } else if (body === undefined) {
    bodyHTML = '<p class="body-note">此请求没有正文。</p>';
  } else {
    bodyHTML = `<div class="subsection-heading"><h3>正文 <span class="muted">· ${r.binary && response ? "二进制 / Hex" : typeof body === "object" ? "JSON" : "文本"}</span></h3>${!r.binary ? `<div class="format-switch"><button data-format="pretty" class="${state.format === "pretty" ? "active" : ""}">格式化</button><button data-format="raw" class="${state.format === "raw" ? "active" : ""}">原始</button></div>` : ""}</div><pre class="code-body">${escape(code(body, state.format === "raw"))}</pre>`;
  }
  const params = !response && r.path.includes("?") ? `<section class="body-section"><div class="subsection-heading"><h3>查询参数</h3></div>${keyValues(Object.fromEntries(new URL(url(r)).searchParams))}</section>` : "";
  content.innerHTML = `<div class="detail-scroll"><section><div class="subsection-heading"><h3>${response ? "响应" : "请求"} Headers <span class="muted">· ${Object.keys(headers).length}</span></h3></div>${keyValues(headers)}</section>${params}<section class="body-section">${bodyHTML}</section></div>`;
}
function renderMessageShell(r) {
  const directions = r.type === "ws" ? '<div class="direction-buttons"><button class="filter" data-direction="all">双向</button><button class="filter" data-direction="up">↑ 发出</button><button class="filter" data-direction="down">↓ 收到</button></div>' : '<span class="stream-type">↓ 服务器 → 应用 · SSE 事件</span>';
  $("detail-content").innerHTML = `<div class="stream-toolbar">${directions}<div class="stream-controls"><label>保留窗口 <select id="window-size" aria-label="保留窗口大小"><option value="1024">1 KiB</option><option value="2048">2 KiB</option><option value="4096">4 KiB</option><option value="8192">8 KiB</option></select></label><button id="follow-toggle" class="button quiet follow-button">↓ 跟随最新</button></div></div><div class="window-status"><span id="window-size-label"></span><span id="window-eviction" class="eviction"></span></div><div class="stream-body"><div id="message-scroll" class="message-scroll" aria-label="流式消息列表"><ol id="message-list" class="message-list"></ol><p id="empty-messages" class="message-scroll-empty" hidden>窗口内暂无这个方向的消息。</p></div><aside id="message-preview" class="message-preview" aria-label="消息内容"></aside></div><div class="stream-footnote"><span id="stream-position"></span><span>滑动窗口模拟 · 丢弃旧内容与索引</span></div>`;
  $("window-size").value = String(state.windowBytes);
  $("window-size").addEventListener("change", (event) => {
    state.windowBytes = Number(event.target.value);
    for (const stream of streams.values()) trim(stream);
    renderMessages();
  });
  $("follow-toggle").addEventListener("click", () => {
    state.follow = !state.follow;
    if (state.follow) state.selectedMessage = null;
    renderMessages();
  });
  $("message-scroll").addEventListener("wheel", (event) => {
    if (event.deltaY < 0) { state.follow = false; updateFollow(); }
  }, { passive: true });
  $("message-scroll").addEventListener("touchmove", () => { state.follow = false; updateFollow(); }, { passive: true });
  renderMessages();
}
function updateFollow() {
  const button = $("follow-toggle");
  if (!button) return;
  button.textContent = state.follow ? "↓ 跟随最新" : "↓ 回到最新";
  button.classList.toggle("paused", !state.follow);
  button.setAttribute("aria-pressed", String(state.follow));
}
function renderMessages() {
  if (state.pane !== "traffic" || state.tab !== "messages" || !$("message-list")) return;
  const stream = streams.get(state.selected);
  if (!stream) return;
  const messages = stream.messages.filter((message) => state.direction === "all" || message.direction === state.direction);
  const list = $("message-list");
  const scroll = $("message-scroll");
  const oldTop = scroll.scrollTop;
  const anchor = [...list.children].find((row) => row.offsetTop + row.offsetHeight > scroll.scrollTop + list.offsetTop);
  const anchorOffset = anchor ? anchor.getBoundingClientRect().top - scroll.getBoundingClientRect().top : 0;
  const activeSequence = state.selectedMessage ?? messages.at(-1)?.sequence;
  const retained = new Set(messages.map((message) => String(message.sequence)));
  const existing = new Map([...list.children].map((row) => [row.dataset.sequence, row]));
  let cursor = list.firstElementChild;
  for (const message of messages) {
    let row = existing.get(String(message.sequence));
    if (!row) {
      row = document.createElement("li");
      row.dataset.sequence = String(message.sequence);
      row.innerHTML = `<button class="message-row" data-message="${message.sequence}" aria-label="查看消息 ${message.sequence}，${message.direction === "up" ? "应用发出" : "服务器返回"}"><span class="message-seq">${message.sequence}</span><time class="message-time">${clock(message.time)}</time><span class="message-direction ${message.direction}">${message.direction === "up" ? "↑" : "↓"}</span><span class="message-data"><strong>${escape(message.event)}</strong><span class="message-byte">${formatBytes(message.bytes)}</span><p>${escape(message.summary)}</p></span></button>`;
    }
    if (row !== cursor) list.insertBefore(row, cursor);
    cursor = row.nextElementSibling;
    const button = row.firstElementChild;
    button.classList.toggle("selected", activeSequence === message.sequence);
    button.setAttribute("aria-pressed", String(activeSequence === message.sequence));
  }
  for (const row of [...list.children]) if (!retained.has(row.dataset.sequence)) row.remove();
  $("empty-messages").hidden = messages.length !== 0;
  $("window-size-label").textContent = `窗口 ${formatBytes(stream.bytes)} / ${formatBytes(state.windowBytes)} · 保留 ${stream.messages.length} 条`;
  $("window-eviction").textContent = stream.evicted ? `已淘汰 ${stream.evicted} 条旧消息` : "窗口内消息完整";
  $("stream-position").textContent = stream.messages.length ? `累计 ${stream.totalCount} 条 · 当前 #${stream.messages[0].sequence}—#${stream.messages.at(-1).sequence}` : "等待消息";
  document.querySelectorAll("[data-direction]").forEach((button) => {
    button.classList.toggle("active", button.dataset.direction === state.direction);
    button.setAttribute("aria-pressed", String(button.dataset.direction === state.direction));
  });
  updateFollow();
  renderMessagePreview(stream.messages.find((message) => message.sequence === activeSequence));
  if (state.follow) scroll.scrollTop = scroll.scrollHeight;
  else if (anchor?.isConnected) scroll.scrollTop += anchor.getBoundingClientRect().top - scroll.getBoundingClientRect().top - anchorOffset;
  else scroll.scrollTop = Math.min(oldTop, scroll.scrollHeight - scroll.clientHeight);
}
function renderMessagePreview(message) {
  const preview = $("message-preview");
  if (!message) {
    preview.innerHTML = `<div class="message-preview-heading">消息内容</div><p class="body-note">${state.selectedMessage !== null ? `消息 #${state.selectedMessage} 已滑出保留窗口，正文与索引已淘汰。请选择窗口内的消息。` : "选择一条消息查看内容。"}</p>`;
    return;
  }
  let value = message.wire;
  if (state.messageFormat === "pretty") {
    try { value = JSON.stringify(JSON.parse(message.text), null, 2); } catch { value = message.text; }
  }
  preview.innerHTML = `<div class="message-preview-heading"><span>消息 #${message.sequence}</span><div class="format-switch"><button data-message-format="pretty" class="${state.messageFormat === "pretty" ? "active" : ""}">内容</button><button data-message-format="raw" class="${state.messageFormat === "raw" ? "active" : ""}">原始</button></div></div><div class="message-preview-meta"><span>${message.direction === "up" ? "↑ 应用 → 服务器" : "↓ 服务器 → 应用"}</span><span>${formatBytes(message.bytes)}</span></div><pre class="code-body">${escape(value)}</pre>`;
}
function renderPane() {
  $("traffic-pane").hidden = state.pane !== "traffic";
  $("process-pane").hidden = state.pane !== "processes";
  document.querySelectorAll("[data-pane]").forEach((button) => {
    button.classList.toggle("active", button.dataset.pane === state.pane);
    button.setAttribute("aria-pressed", String(button.dataset.pane === state.pane));
  });
}
function renderProcesses() {
  const processRows = [
    { pid: 35, name: "curl", command: "curl https://docs.example.test/guide/quickstart", state: "exit 0", links: ["http-docs", "http-notfound", "http-binary"] },
    { pid: 42, name: "python", command: "python tools.py", state: "Running", links: ["http-search", "ws-tools", "http-upload"] },
    { pid: 51, name: "agent", command: "python agent_demo.py", state: "Running", links: ["sse-response", "http-config"] },
  ];
  $("process-list").innerHTML = `<details class="process-card" open><summary><div><div class="process-title">bash</div><div class="process-subtitle">PID 28 · root</div></div><span class="process-state">Running</span></summary></details><div class="process-branch">${processRows.map((p) => `<details class="process-card" data-process="${p.pid}"><summary><div><div class="process-title">${p.name}</div><div class="process-subtitle">PID ${p.pid} · ${fileExamples.filter((file) => file.pid === p.pid).length} 个文件 · ${p.links.length} 个请求</div></div><span class="process-state">${p.state}</span></summary><div class="process-behaviors"><div class="process-behavior"><span class="event-kind">exec</span><code>${escape(p.command)}</code></div>${fileExamples.filter((file) => file.pid === p.pid).map((file) => `<div class="process-behavior" data-file-row="${file.id}"><span class="event-kind">file.open</span><code>${escape(file.requestedPath)}</code><span class="file-open-result ${file.error ? "failed" : ""}">${file.error ?? `FD ${file.fd}`}</span><button class="file-preview-link" data-file="${file.id}">预览文件 ↗</button></div>`).join("")}${p.links.map((id) => `<div class="process-behavior"><span class="event-kind network">network.${byId.get(id).type}</span><code>${escape(byId.get(id).method + " " + byId.get(id).host + byId.get(id).path)}</code><button data-jump="${id}">查看流量 ↗</button></div>`).join("")}</div></details>`).join("")}</div>`;
}
function openFile(id, trigger) {
  const file = fileExamples.find((entry) => entry.id === id);
  if (!file) return;
  state.pane = "processes";
  renderPane();
  document.querySelector(`[data-process="${file.pid}"]`).open = true;
  fileTrigger = trigger ?? document.querySelector(`[data-file-row="${id}"] button`);
  document.querySelectorAll(".selected-file").forEach((row) => row.classList.remove("selected-file"));
  document.querySelector(`[data-file-row="${id}"]`).classList.add("selected-file");
  $("process-workspace").classList.add("preview-open");
  filePreview.open(id);
  $("file-preview").scrollIntoView({ block: "nearest" });
  $("announcement").textContent = `已预览 ${file.path}，PID ${file.pid}，模拟数据`;
}
function renderTerminal() {
  const mobile = matchMedia("(max-width:820px)").matches;
  $("workspace").classList.toggle("terminal-hidden", !state.terminal);
  $("workspace").classList.toggle("show-terminal", state.terminal && mobile);
  $("terminal-panel").hidden = !state.terminal;
  $("terminal-toggle").textContent = state.terminal ? "收起终端" : "显示终端";
  $("terminal-toggle").setAttribute("aria-expanded", String(state.terminal));
}
function renderSimulation() {
  $("simulation-toggle").textContent = state.playing ? "暂停模拟" : "继续模拟";
  $("simulation-status").classList.toggle("paused", !state.playing);
  $("simulation-status").innerHTML = `<span class="live-dot"></span>${state.playing ? "模拟流量运行中" : "模拟已暂停"}`;
}
document.addEventListener("click", (event) => {
  const button = event.target.closest("button");
  if (button?.dataset.file) { openFile(button.dataset.file, button); return; }
  if (button?.dataset.jump) { selectRequest(button.dataset.jump, true); return; }
  if (button?.dataset.pane) { state.pane = button.dataset.pane; renderPane(); if (state.pane === "traffic") renderMessages(); return; }
  if (button?.dataset.protocol) { state.protocol = button.dataset.protocol; renderRequests(); return; }
  if (button?.dataset.detail) { state.tab = button.dataset.detail; renderDetail(); return; }
  if (button?.hasAttribute("data-open-messages")) { state.tab = "messages"; renderDetail(); return; }
  if (button?.dataset.format) { state.format = button.dataset.format; renderDetail(); return; }
  if (button?.dataset.direction) { state.direction = button.dataset.direction; state.selectedMessage = null; renderMessages(); return; }
  if (button?.dataset.message) { state.selectedMessage = Number(button.dataset.message); renderMessages(); return; }
  if (button?.dataset.messageFormat) { state.messageFormat = button.dataset.messageFormat; renderMessages(); return; }
  if (button?.dataset.showProcess) {
    state.pane = "processes";
    renderPane();
    const node = document.querySelector(`[data-process="${button.dataset.showProcess}"]`);
    node.open = true;
    node.scrollIntoView({ block: "nearest" });
    return;
  }
  const row = event.target.closest("[data-request]");
  if (row) selectRequest(row.dataset.request);
});
$("traffic-search").addEventListener("input", (event) => { state.query = event.target.value.trim(); renderRequests(); });
$("pid-filter").addEventListener("change", (event) => { state.pid = event.target.value; renderRequests(); });
$("group-toggle").addEventListener("click", () => { state.grouped = !state.grouped; renderRequests(); });
$("terminal-toggle").addEventListener("click", () => { state.terminal = !state.terminal; renderTerminal(); });
$("simulation-toggle").addEventListener("click", () => { state.playing = !state.playing; renderSimulation(); });
$("reset-demo").addEventListener("click", () => {
  state.playing = true; state.windowBytes = 2048; state.format = "pretty"; state.messageFormat = "pretty"; seed();
  selectRequest("sse-response", true); renderSimulation();
});
matchMedia("(max-width:820px)").addEventListener("change", () => { state.terminal = !matchMedia("(max-width:820px)").matches; renderTerminal(); });
seed();
renderProcesses();
renderPane();
renderRequests();
renderDetail();
renderTerminal();
renderSimulation();
if (location.hash === "#files") openFile("readme");
setInterval(() => {
  if (!state.playing || document.hidden) return;
  generate();
  for (const id of streams.keys()) {
    const cell = document.querySelector(`[data-bytes="${id}"]`);
    if (cell) cell.textContent = formatBytes(streams.get(id).totalBytes);
  }
  renderMessages();
  const count = [...streams.values()].reduce((total, stream) => total + stream.totalCount, 0);
  $("footer-summary").textContent = `8 个请求 · 2 个活动流 · 已观察 ${count} 条消息`;
}, 1100);
