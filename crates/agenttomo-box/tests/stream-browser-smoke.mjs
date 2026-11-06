// Opt-in GUI check: Playwright, ws, Docker and the Chrome Playground image.
import assert from "node:assert/strict";
import { spawn, execFileSync } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, mkdir, rm } from "node:fs/promises";
import { createServer } from "node:http";
import { fileURLToPath } from "node:url";

const { chromium } = await import(process.env.AGENTTOMO_PLAYWRIGHT_MODULE || "playwright");
const { WebSocketServer } = await import(process.env.AGENTTOMO_WS_MODULE || "ws");
const repo = fileURLToPath(new URL("../../..", import.meta.url));
const directory = await mkdtemp("/tmp/agenttomo-live-streams-");
const bridge = JSON.parse(execFileSync("docker", ["network", "inspect", "bridge"], { encoding: "utf8" }))[0].IPAM.Config[0].Gateway;
const fixture = createServer((req, res) => {
  if (req.url === "/events") {
    res.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache" });
    let n = 0;
    const timer = setInterval(() => res.write(`id: ${++n}\nevent: token\ndata: ${JSON.stringify({ n, text: `token-${n}` })}\n\n`), 100);
    req.on("close", () => clearInterval(timer));
    return;
  }
  res.setHeader("content-type", "text/html");
  res.end(`<!doctype html><title>Live stream probe</title><h1>SSE and WebSocket</h1><pre id=events></pre><pre id=messages></pre><script>
    const source = new EventSource('/events');
    source.addEventListener('token', e => document.querySelector('#events').textContent=e.data);
    const socket = new WebSocket('ws://' + location.host + '/socket');
    let n=0,timer;
    socket.onopen=()=>timer=setInterval(()=>socket.send(JSON.stringify({client:++n})),150);
    socket.onmessage=e=>document.querySelector('#messages').textContent=e.data;
    socket.onclose=()=>clearInterval(timer);
  </script>`);
});
const wss = new WebSocketServer({ server: fixture, perMessageDeflate: { threshold: 0 } });
wss.on("connection", socket => socket.on("message", bytes => socket.send(JSON.stringify({ echo: JSON.parse(bytes.toString()), server: "live reply" }))));
fixture.listen(0, bridge); await once(fixture, "listening");
const fixtureUrl = `http://${bridge}:${fixture.address().port}/`;
const server = spawn(`${repo}/target/debug/agenttomo-box`, ["playground", "--browser", "--capture-http", "--listen", "127.0.0.1:0", "--output-dir", directory, "--image", "agenttomo-playground-chrome:24.04", "--stream-window-messages", "30", "--stream-window-bytes", "65536", "--stream-message-max-bytes", "128", "--file-preview-max-bytes", "0"], { detached: true, stdio: ["ignore", "pipe", "pipe"] });
let output = "", errors = "", browser, session;
server.stdout.on("data", bytes => output += bytes);
server.stderr.on("data", bytes => errors += bytes);
const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
async function until(test, message, timeout = 30000) {
  const deadline = Date.now() + timeout;
  while (!await test()) { assert.ok(Date.now() < deadline, message); await pause(100); }
}

try {
  await until(() => output.includes("?token="), "Playground startup");
  const url = output.match(/http:\/\/\S+/)[0];
  browser = await chromium.launch({ executablePath: process.env.AGENTTOMO_TEST_BROWSER || undefined, headless: true, args: ["--no-sandbox", "--no-proxy-server", "--host-resolver-rules=MAP agenttomo.test 127.0.0.1"] });
  const page = await browser.newPage({ viewport: { width: 1512, height: 982 } });
  const pageErrors = [];
  page.on("pageerror", error => pageErrors.push(String(error)));
  page.on("websocket", socket => {
    if (new URL(socket.url()).pathname !== "/ws") return;
    socket.on("framereceived", ({ payload }) => {
      if (typeof payload === "string" && payload.startsWith("{")) {
        const frame = JSON.parse(payload); if (frame.type === "session") session = frame;
      }
    });
  });
  await page.goto(url.replace("127.0.0.1", "agenttomo.test"));
  await until(async () => await page.locator("#browser-status").textContent() === "Connected · Chrome", "Chrome display");
  await until(() => execFileSync("docker", ["exec", session.container, "xwininfo", "-root", "-tree"], { encoding: "utf8" }).includes("Google Chrome"), "Chrome window");
  await page.locator("#browser-screen canvas").click(); await page.keyboard.press("Control+l"); await pause(500);
  await page.keyboard.type(fixtureUrl, { delay: 50 }); await page.keyboard.press("Enter");
  await page.locator("[data-observation=traffic]").click();
  await page.locator('[aria-label="Filter traffic"]').fill("/events");
  await page.locator(".traffic-row").first().click();
  await until(async () => (await page.locator(".stream-payload").textContent()).includes("token-"), "live SSE payload before EOF");
  assert.match(await page.locator(".stream-state").textContent(), /Live/);
  await until(async () => await page.locator(".stream-message").count() >= 8, "enough live events for scroll check");
  await page.locator(".stream-list").evaluate(node => node.scrollTop = 0);
  await until(async () => await page.locator(".stream-follow").getAttribute("aria-pressed") === "false", "scroll pauses follow");
  await pause(600);
  assert.equal(await page.locator(".stream-list").evaluate(node => node.scrollTop), 0);
  await page.locator(".stream-message").first().click();
  await until(async () => (await page.locator(".stream-payload-status").textContent()).includes("expired"), "old selected message expires from sliding window", 10000);
  await page.locator(".stream-follow").click();
  await until(async () => (await page.locator(".stream-payload").textContent()).includes("token-"), "follow resumes");
  await page.locator(".stream-copy").click();
  await until(async () => (await page.locator(".stream-payload-status").textContent()).includes("copied"), "copy live content");
  if (process.env.AGENTTOMO_TEST_ARTIFACTS) {
    await mkdir(process.env.AGENTTOMO_TEST_ARTIFACTS, { recursive: true });
    await page.screenshot({ path: `${process.env.AGENTTOMO_TEST_ARTIFACTS}/sse-live.png`, fullPage: true });
  }
  await page.locator('[aria-label="Filter traffic"]').fill("/socket");
  await page.locator(".traffic-row").first().click();
  await until(async () => (await page.locator(".stream-field-0").allTextContents()).includes("↑ Sent") && (await page.locator(".stream-field-0").allTextContents()).includes("↓ Received"), "WebSocket both directions");
  const received = page.locator(".stream-message").filter({ hasText: "↓ Received" }).last();
  await received.click();
  await until(async () => (await page.locator(".stream-payload").textContent()).includes("live reply"), "WebSocket decompressed payload");
  assert.match(await page.locator(".stream-payload-status").textContent(), /decompressed/);
  assert.equal(JSON.parse(await page.locator("#event-json").textContent()).kind, "network.stream.message");
  if (process.env.AGENTTOMO_TEST_ARTIFACTS) await page.screenshot({ path: `${process.env.AGENTTOMO_TEST_ARTIFACTS}/websocket-live.png`, fullPage: true });
  await page.locator('[data-side="request"]').click();
  assert.ok(await page.locator(".traffic-headers").count());
  await page.locator('[data-side="messages"]').click();
  await until(async () => await page.locator(".stream-message").count() > 0, "messages reopen");
  for (const socket of wss.clients) socket.close(1000, "finished");
  await until(async () => (await page.locator(".stream-state").textContent()).includes("Closed"), "stream closes without replacing its view");
  await page.locator(".traffic-copy-url").click();
  await until(async () => (await page.locator(".stream-payload-status").textContent()).includes("URL copied"), "copy URL after lifecycle update");
  for (const width of [768, 390, 320]) {
    await page.setViewportSize({ width, height: 900 });
    assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth + 1), `overflow at ${width}`);
  }
  assert.deepEqual(pageErrors, []); assert.equal(errors, "");
  console.log(JSON.stringify({ result: "PASS", checks: ["Chrome EventSource", "Chrome compressed WebSocket", "live payload before EOF", "bidirectional messages", "window expiry", "scroll pause and resume", "copy", "raw details", "request/message tabs", "closed state and URL copy", "narrow layout"] }));
} catch (error) {
  console.error("Playground stderr:", errors);
  throw error;
} finally {
  await browser?.close();
  for (const socket of wss.clients) socket.terminate();
  wss.close(); fixture.closeAllConnections(); fixture.close();
  const exited = server.exitCode === null ? once(server, "exit") : Promise.resolve();
  try { process.kill(-server.pid, "SIGTERM"); } catch {}
  let timer; await Promise.race([exited, new Promise(resolve => timer = setTimeout(resolve, 15000))]); clearTimeout(timer);
  if (server.exitCode === null) { try { process.kill(-server.pid, "SIGKILL"); } catch {} }
  await rm(directory, { recursive: true, force: true });
}
