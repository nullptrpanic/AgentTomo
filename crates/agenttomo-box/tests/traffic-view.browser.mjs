// Opt-in DOM regression with the existing Playwright/Chromium test installation.
import assert from "node:assert/strict";
import { once } from "node:events";
import { readFile } from "node:fs/promises";
import { createServer } from "node:http";

const { chromium } = await import(process.env.AGENTTOMO_PLAYWRIGHT_MODULE || "playwright");
const assets = new URL("../src/playground/assets/", import.meta.url);
const server = createServer(async (request, response) => {
  const path = new URL(request.url, "http://localhost").pathname.slice(1);
  if (!path) {
    response.setHeader("content-type", "text/html");
    response.end('<!doctype html><meta charset="utf-8"><link rel="stylesheet" href="/traffic.css"><div id="traffic" style="height:650px;width:700px"></div>');
    return;
  }
  if (!/^[a-z-]+\.(mjs|css)$/.test(path)) { response.writeHead(404).end(); return; }
  try {
    const bytes = await readFile(new URL(path, assets));
    response.setHeader("content-type", path.endsWith(".css") ? "text/css" : "text/javascript");
    response.end(bytes);
  } catch { response.writeHead(404).end(); }
});
server.listen(0, "127.0.0.1");
await once(server, "listening");
let browser;
try {
  browser = process.env.AGENTTOMO_CDP ? await chromium.connectOverCDP(process.env.AGENTTOMO_CDP) : await chromium.launch({ executablePath: process.env.AGENTTOMO_TEST_BROWSER || undefined, headless: true, args: ["--no-sandbox"] });
  const page = await browser.newPage();
  await page.goto(`http://127.0.0.1:${server.address().port}/`);
  const result = await page.evaluate(async () => {
    const { TrafficView } = await import("/traffic-view.mjs");
    const check = (condition, message) => { if (!condition) throw new Error(message); };
    const frames = [];
    window.requestAnimationFrame = callback => frames.push(callback);
    const flush = () => { while (frames.length) frames.shift()(); };
    const root = document.querySelector("#traffic");
    const selected = [];
    let copied;
    Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText: async text => { copied = text; } } });
    const view = new TrafficView(root, async () => ({ bytes: new TextEncoder().encode("response body"), decoding: "identity" }), event => selected.push(event));
    view.setEnabled(true);
    const list = root.querySelector(".traffic-list");
    const row = id => root.querySelector(`[data-request-id="${id}"]`);
    const event = (kind, id, fields = {}) => ({ kind: `network.http.${kind}`, data: { request_id: id, connection_id: id, pid: 7, started_unix_ns: id * 1000000, method: "GET", url: `https://example.test/r/${id}`, ...fields } });
    let renders = 0;
    const render = view.renderList.bind(view);
    view.renderList = () => { renders++; render(); };
    for (let id = 1; id <= 30; id++) view.add(event("request", id));
    check(renders === 0 && frames.length === 1, `burst rendered ${renders} times before its frame`);
    flush();
    check(renders === 1 && list.children.length === 30, "one render per burst");
    const first = row(1), seventh = row(7);
    seventh.focus();
    view.add(event("response", 7, { status_code: 201, version: "HTTP/1.1", headers: [] }));
    view.add(event("response.body", 7, { body: { status: "complete", offset: 0, captured_bytes: 13, observed_bytes: 13, complete: true } }));
    flush();
    check(row(1) === first && row(7) === seventh, "request rows were replaced");
    check(document.activeElement === seventh, "focused request lost focus");
    check(seventh.querySelector(".traffic-code").textContent === "201", "response status update");
    const filter = root.querySelector("input");
    filter.value = "/r/7"; filter.dispatchEvent(new Event("input"));
    check(list.children.length === 1 && row(7) === seventh, "filter retains matching row");
    filter.value = ""; filter.dispatchEvent(new Event("input"));
    check(list.children.length === 30 && row(1) === first, "clearing filter restores rows");
    seventh.click();
    await new Promise(resolve => setTimeout(resolve, 0));
    check(seventh.getAttribute("aria-pressed") === "true", "selected request");
    check(root.querySelector(".traffic-body").textContent === "response body", "body remains inspectable");
    check(selected.at(-1).data.request_id === 7, "raw detail selection");
    check(root.querySelector(".traffic-body-section").open, "body defaults to expanded");
    root.querySelector(".traffic-body-title").click();
    check(!root.querySelector(".traffic-body-section").open && !root.querySelector(".traffic-body").checkVisibility(), "body title collapses content");
    root.querySelector("[data-body-mode=raw]").click();
    await new Promise(resolve => setTimeout(resolve, 0));
    check(!root.querySelector(".traffic-body-section").open, "raw switch must preserve collapsed state");
    check(root.querySelector(".traffic-body-status").textContent.includes("Original entity bytes"), "raw representation label");
    check(root.querySelector(".traffic-body").textContent.includes("72 65 73 70"), "raw representation hex bytes");
    root.querySelector(".traffic-body-copy").click();
    await new Promise(resolve => setTimeout(resolve, 0));
    check(copied.includes("72 65 73 70") && !root.querySelector(".traffic-body-section").open, "copy remains usable without expanding the body");
    check(root.querySelector(".traffic-copy-status").checkVisibility() && root.querySelector(".traffic-copy-status").textContent.includes("copied"), "collapsed body copy feedback remains visible");
    check(root.querySelector(".traffic-body-copy").title === "Copied", "body copy shows its success icon");
    root.querySelector(".traffic-body-copy").dispatchEvent(new MouseEvent("mouseleave"));
    check(root.querySelector(".traffic-body-copy").title === "Copy hex", "body copy feedback restores the original representation label");
    view.add(event("response.body", 7, { body: { status: "complete", offset: 0, captured_bytes: 13, observed_bytes: 13, complete: true } }));
    flush();
    await new Promise(resolve => setTimeout(resolve, 0));
    check(!root.querySelector(".traffic-body-section").open && !root.querySelector(".traffic-body-status").checkVisibility(), "live updates must preserve collapsed body and status");
    root.querySelector("[data-body-mode=decoded]").click();
    await new Promise(resolve => setTimeout(resolve, 0));
    check(!root.querySelector(".traffic-body-section").open, "decoded switch must preserve collapsed state");
    root.querySelector(".traffic-body-title").click();
    check(root.querySelector(".traffic-body-section").open && root.querySelector(".traffic-body").checkVisibility(), "body title restores content");
    check(root.querySelector(".traffic-body").textContent === "response body", "decoded representation restored");
    filter.value = "does-not-match"; filter.dispatchEvent(new Event("input"));
    view.select({ kind: "network.upstream", data: { connection_id: 7 } });
    check(view.selected === 7 && row(7), "upstream navigation must clear the old text filter and select its exchange");
    view.add(event("request", 31, { connection_id: 7 })); flush();
    check(list.querySelectorAll(".traffic-row").length === 2, "keep-alive connection must show all its requests");
    view.select({ kind: "network.protocol", data: { connection_id: 8000 } });
    check(view.selected == null && !root.querySelector(".traffic-body"), "unmatched connection must clear unrelated details");
    check(root.querySelector(".traffic-inspector").textContent.includes("No captured HTTP exchange"), "missing exchange explanation");
    view.add(event("request", 32, { connection_id: 8000 })); flush();
    check(view.selected === 32, "pending connection must open when its request arrives");
    root.querySelector(".traffic-show-all").click();
    check(list.querySelectorAll(".traffic-row").length === 32, "return to all requests");
    view.select(event("request", 7));
    await new Promise(resolve => setTimeout(resolve, 0));
    const body = root.querySelector(".traffic-body");
    list.scrollTop = 0;
    view.add(event("response", 31)); flush();
    check(list.scrollTop === 0 && root.querySelector(".traffic-body") === body, "paused scrolling or selected detail changed");
    list.scrollTop = list.scrollHeight;
    view.add(event("response", 32)); flush();
    check(list.scrollHeight - list.scrollTop - list.clientHeight <= 1, "bottom following");
    const beforeEviction = renders;
    for (let id = 33; id <= 1033; id++) view.add(event("request", id));
    flush();
    check(renders === beforeEviction + 1, "retention burst was not batched");
    check(list.children.length === 1000 && view.rows.size === 1000, "row retention bound");
    check(!row(7) && view.selected === null && root.querySelector(".traffic-inspector").textContent.includes("left the live list"), "evicted selection cleared");

    // A final message can finish loading after the stream's closed event.
    let finishPayload;
    view.loadStream = async (_request, message) => message == null ? {
      messages: [{ id: 1, direction: "downstream", kind: "text", captured_bytes: 4, observed_bytes: 4, complete: true }],
      limits: { window_bytes: 4096, window_messages: 30, message_bytes: 128 }, evicted: 0, dropped: 0,
    } : new Promise(resolve => { finishPayload = resolve; });
    view.add(event("request", 2000));
    view.add({ kind: "network.stream.open", data: { request_id: 2000, protocol: "websocket" } });
    flush(); row(2000).click();
    await new Promise(resolve => setTimeout(resolve, 0));
    check(!root.querySelector(".traffic-body-section"), "messages view must not leave an empty body section");
    check(typeof finishPayload === "function", "stream payload should be loading");
    view.add({ kind: "network.stream.closed", data: { request_id: 2000, complete: true } });
    flush();
    finishPayload(new TextEncoder().encode("last"));
    await new Promise(resolve => setTimeout(resolve, 0));
    check(!root.querySelector(".traffic-copy-url"), "URL copy button was removed");
    const streamCopy = root.querySelector(".stream-copy");
    check(streamCopy.querySelector("svg") && streamCopy.textContent === "" && streamCopy.getAttribute("aria-label") === "Copy content", "stream copy uses a labeled icon");
    await streamCopy.onclick();
    check(copied === "last" && root.querySelector(".stream-payload-status").textContent.includes("copied"), "closed stream message copy");
    check(streamCopy.title === "Copied", "stream copy uses the shared success feedback");
    view.streamView.stop();
    return { burst_events: 30, burst_renders: 1, retained_rows: list.children.length };
  });
  assert.equal(result.burst_renders, 1);
  await page.locator("[data-side=response]").click();
  const summary = page.locator(".traffic-body-section > summary");
  await summary.focus();
  await page.keyboard.press("Enter");
  assert.equal(await page.locator(".traffic-body-section").evaluate(node => node.open), false);
  await page.keyboard.press("Space");
  assert.equal(await page.locator(".traffic-body-section").evaluate(node => node.open), true);
  console.log(JSON.stringify({ result: "PASS", ...result, checks: ["batched renders", "row identity", "focus", "filter", "connection navigation and pending requests", "body collapse across mode changes and live updates", "collapsed body copy feedback", "keyboard collapse and expand", "decoded/raw representations", "body and raw details", "scroll pause/follow", "eviction", "closed stream message copy icon"] }));
} finally {
  await browser?.close();
  server.closeAllConnections();
  server.close();
}
