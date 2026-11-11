// Opt-in real browser check: needs Docker, the Chrome image, Playwright and
// Internet access to AGENTTOMO_HTTPS_TEST_URL (defaults to Baidu).
import assert from "node:assert/strict";
import { spawn, execFileSync } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, mkdir, readFile, rm } from "node:fs/promises";
import { createServer } from "node:http";
import { fileURLToPath } from "node:url";
import { gzipSync } from "node:zlib";

const { chromium } = await import(process.env.AGENTTOMO_PLAYWRIGHT_MODULE || "playwright");
const repo = fileURLToPath(new URL("../../..", import.meta.url));
const directory = await mkdtemp("/tmp/agenttomo-browser-smoke-");
const docker = (...args) => execFileSync("docker", args, { encoding: "utf8", timeout: 10000 });
const bridge = JSON.parse(docker("network", "inspect", "bridge"))[0].IPAM.Config[0].Gateway;
const httpsUrl = new URL(process.env.AGENTTOMO_HTTPS_TEST_URL || "https://www.baidu.com/").href;
const html = "<!doctype html><html><title>AgentTomo Chrome</title><body>chrome-observed</body></html>";
const fixture = createServer((_req, res) => { res.setHeader("content-type", "text/html; charset=utf-8"); res.setHeader("content-encoding", "gzip"); res.end(gzipSync(html)); });
fixture.listen(0, bridge);
await once(fixture, "listening");
const httpUrl = `http://${bridge}:${fixture.address().port}/chrome-observation`;
const server = spawn(`${repo}/target/debug/agenttomo-box`, [
  "playground", "--listen", "127.0.0.1:0", "--output-dir", directory,
  "--capture-http",
  ...(process.env.AGENTTOMO_TEST_ROOT_OVERLAY === "1" ? ["--overlay-root"] : []),
], { detached: true, stdio: ["ignore", "pipe", "pipe"] });
let output = "", errors = "", browser, page, session;
const events = [], pageErrors = [], warnings = [];
server.stdout.on("data", data => output += data);
server.stderr.on("data", data => errors += data);
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
async function until(predicate, timeout = 30000) {
  const deadline = Date.now() + timeout;
  while (!await predicate()) {
    assert.ok(Date.now() < deadline, "browser condition timed out");
    await delay(100);
  }
}
function completed(url) {
  const request = events.find(e => e.kind === "network.http.request" && e.data.url === url);
  return request && events.some(e => e.kind === "network.http.response.body" && e.data.request_id === request.data.request_id);
}
async function navigate(url) {
  await page.locator("#browser-screen canvas").click();
  await page.keyboard.press("Control+l");
  // RFB input reaches a remote GUI asynchronously; allow its focus change.
  await delay(500);
  await page.keyboard.type(url, { delay: 50 });
  await page.keyboard.press("Enter");
  await until(() => completed(url), 60000);
}

try {
  await until(() => {
    assert.equal(server.exitCode, null, errors);
    return output.includes("?token=");
  });
  const url = output.match(/http:\/\/\S+/)[0];
  browser = await chromium.launch({
    executablePath: process.env.AGENTTOMO_TEST_BROWSER || undefined,
    headless: true,
    // This is the test runner's browser, not the observed container Chrome.
    args: ["--no-sandbox", "--no-proxy-server", "--host-resolver-rules=MAP agenttomo.test 127.0.0.1"],
  });
  page = await browser.newPage({ viewport: { width: 1512, height: 982 } });
  page.on("pageerror", error => pageErrors.push(String(error)));
  page.on("console", message => { if (message.type() === "warning") warnings.push(message.text()); });
  page.on("websocket", socket => {
    if (new URL(socket.url()).pathname !== "/ws") return;
    socket.on("framereceived", ({ payload }) => {
      if (typeof payload !== "string" || !payload.startsWith("{")) return;
      const frame = JSON.parse(payload);
      if (frame.type === "session") session = frame;
      if (frame.type === "trace") events.push(frame.event);
    });
  });
  // Exercise the user's plain-HTTP deployment, not localhost's secure-context exception.
  await page.goto(url.replace("127.0.0.1", "agenttomo.test"));
  assert.equal(await page.evaluate(() => window.isSecureContext), false);
  await until(async () => await page.locator("#browser-status").textContent() === "Connected · Chrome");
  assert.equal(session.browser, true);
  assert.equal(docker("exec", session.container, "id", "-u").trim(), "0", "the container collector runs as root");
  await until(() => docker("exec", session.container, "xwininfo", "-root", "-tree").includes("Google Chrome"));
  await navigate(httpUrl);
  const request = events.find(e => e.kind === "network.http.request" && e.data.url === httpUrl);
  const browserStatus = docker("exec", session.container, "cat", `/proc/${request.data.pid}/status`);
  assert.match(browserStatus, /^Uid:\s+65534\s+65534\s+65534\s+65534$/m, "the observed browser keeps an unprivileged identity");
  assert.ok(events.some(e => e.kind === "process.exec" && e.data.pid === request.data.pid && e.data.argv?.includes("--utility-sub-type=network.mojom.NetworkService")));
  assert.ok(events.some(e => e.kind === "file.open"));
  assert.ok(!events.some(e => e.kind === "process.exec" && ["Xvfb", "x11vnc", "openbox"].includes(e.data.argv?.[0]?.split("/").pop())));
  await page.locator(`.process-node[data-pid="${request.data.pid}"]`).last().evaluate(node => {
    for (let parent = node; parent; parent = parent.parentElement) if (parent.tagName === "DETAILS") parent.open = true;
  });
  await page.locator(`.trace-network-flow[data-connection-id="${request.data.connection_id}"] > .event-row`).click();
  assert.ok(await page.locator("#traffic").isVisible(), "connection event must open Traffic");
  assert.ok((await page.locator(".traffic-navigation").textContent()).includes(`Connection #${request.data.connection_id}`));
  await until(async () => (await page.locator(".traffic-body").textContent()).includes("chrome-observed"));
  assert.ok((await page.locator(".traffic-body-status").textContent()).includes("decompressed"));
  await page.locator("[data-body-mode=raw]").click();
  await until(async () => (await page.locator(".traffic-body").textContent()).includes("1f 8b 08"));
  await page.locator("[data-body-mode=decoded]").click();
  await until(async () => (await page.locator(".traffic-body").textContent()).includes("chrome-observed"));
  await page.locator("[data-observation=process]").click();
  assert.ok(await page.locator(".process-node > summary").filter({ hasText: `PID ${request.data.pid}` }).count());

  await navigate(httpsUrl);
  const https = events.find(e => e.kind === "network.http.request" && e.data.url === httpsUrl);
  assert.ok(events.some(e => e.kind === "network.http.response" && e.data.request_id === https.data.request_id && e.data.status_code === 200));
  assert.ok(events.some(e => e.kind === "network.protocol" && e.data.tls_intercepted && e.data.domain === new URL(httpsUrl).hostname));
  const forbidden = ["--no-sandbox", "--ignore-certificate-errors", "--ignore-certificate-errors-spki-list"];
  assert.ok(events.filter(e => e.kind === "process.exec").every(e => !e.data.argv?.some(arg => forbidden.some(flag => arg === flag || arg.startsWith(`${flag}=`)))));
  await assert.rejects(readFile(`${directory}/${session.id}/run/task-ca.pem`), { code: "ENOENT" });
  const rootPid = events.find(event => event.kind === "process.exec").data.pid;
  assert.match(docker("exec", session.container, "cat", `/proc/${rootPid}/status`), /^Uid:\s+0\s+0\s+0\s+0$/m, "the terminal workload runs as root");
  const ca = docker("exec", session.container, "cat", `/proc/${rootPid}/root/etc/ssl/certs/agenttomo-task-ca.pem`);
  // Inspect the committed public certificate DB through the task's root. NSS
  // module configuration contains paths in that root, so use an isolated copy
  // without its pkcs11.txt rather than opening it in Docker's original root.
  const nss = docker("exec", session.container, "/bin/sh", "-ec", `
    check=$(mktemp -d)
    trap 'rm -rf "$check"' EXIT
    certutil -N --empty-password -d "sql:$check"
    cp /proc/${rootPid}/root/tmp/.local/share/pki/nssdb/cert9.db "$check/cert9.db"
    certutil -d "sql:$check" -L -a -n 'AgentTomo task CA'
  `);
  assert.equal(nss.replace(/\s/g, ""), ca.replace(/\s/g, ""));
  assert.equal(docker("exec", session.container, "cat", "/etc/ssl/certs/ca-certificates.crt").includes(ca), false);
  await page.locator("[data-observation=traffic]").click();
  await page.locator(".traffic-show-all").click();
  await page.locator('[aria-label="Filter traffic"]').fill(new URL(httpsUrl).hostname);
  await page.locator(".traffic-row").filter({ hasText: httpsUrl }).first().click();
  await until(async () => (await page.locator(".traffic-body").textContent()).length > 0);
  assert.match(await page.locator(".traffic-body").textContent(), /<!doctype|<html/i, "HTTPS HTML must display as readable markup");
  if (process.env.AGENTTOMO_TEST_ARTIFACTS) {
    await mkdir(process.env.AGENTTOMO_TEST_ARTIFACTS, { recursive: true });
    await page.screenshot({ path: `${process.env.AGENTTOMO_TEST_ARTIFACTS}/chrome-https.png`, fullPage: true });
  }
  await page.locator("#browser-reconnect").click();
  await until(async () => await page.locator("#browser-status").textContent() === "Connected · Chrome");
  await page.locator("[data-workspace=terminal]").click();
  assert.ok(await page.locator("#terminal").isVisible());
  const separator = page.locator("#panel-resizer");
  const initial = await page.locator(".terminal-panel").boundingBox();
  const handle = await separator.boundingBox();
  await page.mouse.move(handle.x + handle.width / 2, handle.y + handle.height / 2);
  await page.mouse.down();
  await page.mouse.move(handle.x - 130, handle.y + handle.height / 2, { steps: 10 });
  await page.mouse.up();
  const resized = await page.locator(".terminal-panel").boundingBox();
  assert.ok(resized.width < initial.width - 100, "drag must change panel widths");
  await separator.press("ArrowRight");
  assert.ok((await page.locator(".terminal-panel").boundingBox()).width > resized.width + 15, "keyboard resizing");
  await separator.press("Home");
  assert.ok(Math.abs((await page.locator(".terminal-panel").boundingBox()).width - 320) <= 1, "workspace minimum width");
  await separator.dblclick();
  await until(async () => Math.abs((await page.locator(".terminal-panel").boundingBox()).width - initial.width) < 2);
  await page.locator("#terminal").click();
  const previewPath = "/root/agenttomo-preview-regression.json";
  await page.keyboard.type(`printf '{"preview":"after-browser-startup"}\\n' > ${previewPath}; cat ${previewPath}\n`, { delay: 5 });
  await until(() => events.some(e => e.kind === "file.open" && e.data.path === previewPath && e.data.result >= 0 && (e.data.flags & 3) === 0));
  const previewEvent = events.find(e => e.kind === "file.open" && e.data.path === previewPath && e.data.result >= 0 && (e.data.flags & 3) === 0);
  if (process.env.AGENTTOMO_TEST_ROOT_OVERLAY === "1") {
    assert.equal(docker("exec", session.container, "/bin/sh", "-c", `test ! -e ${previewPath} && printf private`).trim(), "private", "terminal writes must not reach the original root");
  }
  assert.equal(previewEvent.data.preview?.status, "complete", JSON.stringify(previewEvent.data.preview));
  assert.ok(events.some(e => e.kind === "file.open" && e.data.preview?.reused), "repeated file snapshots must reuse storage");
  await page.locator("[data-observation=process]").click();
  await page.locator(`.process-node[data-pid="${previewEvent.data.pid}"]`).last().evaluate(node => {
    for (let parent = node; parent; parent = parent.parentElement) if (parent.tagName === "DETAILS") parent.open = true;
  });
  await page.locator(".event-row").filter({ hasText: previewPath }).last().click();
  await until(async () => (await page.locator(".file-inspector-body").textContent()).includes("after-browser-startup"));
  await page.locator("#event-detail summary").click();
  assert.ok((await page.locator("#event-json").textContent()).includes(previewPath), "raw event details remain available");
  await page.locator("[data-preview-close]").click();
  await page.locator("#event-detail summary").click();
  await page.locator("[data-workspace=browser]").click();
  for (const width of [768, 390, 320]) {
    await page.setViewportSize({ width, height: 900 });
    assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth + 1), `overflow at ${width}`);
    assert.ok(!await separator.isVisible(), "narrow layouts stack the panels");
  }
  assert.deepEqual(pageErrors, []);
  assert.equal(errors, "");
  console.log(JSON.stringify({ result: "PASS", networkPid: request.data.pid, https: httpsUrl, checks: ["connection to Traffic", "panel drag and keyboard resize", "file preview after Chrome startup", "snapshot reuse", "raw event details", "narrow layout"], warnings }));
} catch (error) {
  console.error("Playground stderr:", errors);
  if (session) {
    const pid = events.find(event => event.kind === "process.exec")?.data.pid;
    try { console.error("Chrome log:", docker("exec", session.container, "tail", "-30", pid ? `/proc/${pid}/root/tmp/agenttomo-chrome.log` : "/tmp/agenttomo-chrome.log")); } catch { /* session may already be removed */ }
  }
  throw error;
} finally {
  await browser?.close();
  fixture.close();
  const exited = server.exitCode === null ? once(server, "exit") : Promise.resolve();
  try { process.kill(-server.pid, "SIGTERM"); } catch {}
  let timer;
  await Promise.race([exited, new Promise(resolve => timer = setTimeout(resolve, 15000))]);
  clearTimeout(timer);
  if (server.exitCode === null) { try { process.kill(-server.pid, "SIGKILL"); } catch {} }
  await rm(directory, { recursive: true, force: true });
}
