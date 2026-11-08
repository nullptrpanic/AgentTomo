// Opt-in real Docker + browser regression. Uses the existing local tools image;
// no remote websites, image pulls or public listening ports are needed.
import assert from "node:assert/strict";
import { spawn, execFileSync } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, readFile, readdir, rm } from "node:fs/promises";
import { createServer } from "node:http";
import { fileURLToPath } from "node:url";
import { gzipSync } from "node:zlib";

const { chromium } = await import(process.env.AGENTTOMO_PLAYWRIGHT_MODULE || "playwright");
const repo = fileURLToPath(new URL("../../..", import.meta.url));
const directory = await mkdtemp("/tmp/agenttomo-monitor-pages-");
const docker = (...args) => execFileSync("docker", args, { encoding: "utf8", timeout: 10000, stdio: ["ignore", "pipe", "pipe"] });
const bridge = JSON.parse(docker("network", "inspect", "bridge"))[0].IPAM.Config[0].Gateway;
const fixture = createServer((_req, response) => {
  response.setHeader("content-type", "text/plain; charset=utf-8");
  response.setHeader("content-encoding", "gzip");
  response.end(gzipSync("shared-monitor-response"));
});
fixture.listen(0, bridge);
await once(fixture, "listening");
const httpUrl = `http://${bridge}:${fixture.address().port}/monitor-test`;
const server = spawn(`${repo}/target/debug/agenttomo-box`, [
  "playground", "--listen", "127.0.0.1:0", "--output-dir", directory,
  "--image", "agenttomo-playground:24.04", "--capture-http",
], { detached: true, stdio: ["ignore", "pipe", "pipe"] });
let output = "", errors = "", browser, owner, monitor, second, url;
const sessions = [], events = [], pageErrors = [], sockets = [], observed = new Map();
server.stdout.on("data", bytes => output += bytes);
server.stderr.on("data", bytes => errors += bytes);
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
async function until(predicate, message, timeout = 30000) {
  const deadline = Date.now() + timeout;
  while (!await predicate()) {
    assert.ok(Date.now() < deadline, message);
    await delay(100);
  }
}
async function command(text) {
  await owner.locator("#terminal").click();
  await owner.keyboard.type(text, { delay: 1 });
  await owner.keyboard.press("Enter");
}
async function openMonitor(page) {
  const popup = page.waitForEvent("popup");
  await page.locator("#open-monitor").click();
  const result = await popup;
  await result.waitForLoadState();
  await until(async () => await result.locator("#status").textContent() === "Following session", "monitor must become live");
  return result;
}
async function caughtUp(page) {
  const sequence = events.at(-1)?.sequence;
  await until(() => observed.get(page)?.some(event => event.sequence === sequence), `monitor must receive event ${sequence}`);
}

try {
  await until(() => { assert.equal(server.exitCode, null, errors); return output.includes("?token="); }, "server start");
  url = output.match(/http:\/\/\S+/)[0];
  browser = await chromium.launch({ executablePath: process.env.AGENTTOMO_TEST_BROWSER || undefined,
    headless: true, args: ["--no-sandbox", "--no-proxy-server", "--disable-dev-shm-usage"] });
  const context = await browser.newContext({ viewport: { width: 1500, height: 950 } });
  context.on("page", page => {
    observed.set(page, []);
    page.on("pageerror", error => pageErrors.push(String(error)));
    page.on("websocket", socket => {
      sockets.push({ page, path: new URL(socket.url()).pathname });
      socket.on("framereceived", ({ payload }) => {
        if (typeof payload !== "string" || !payload.startsWith("{")) return;
        const frame = JSON.parse(payload);
        if (frame.type === "session") sessions.push(frame);
        if (frame.type === "trace") events.push(frame.event);
      });
    });
    page.on("response", async response => {
      if (!new URL(response.url()).pathname.endsWith("/events") || !response.ok()) return;
      try { observed.get(page).push(...(await response.json()).events); } catch { /* navigation cancels reads */ }
    });
  });
  owner = await context.newPage();
  await owner.goto(url);
  await until(async () => await owner.locator("#status").textContent() === "Connected", "workspace ready");
  const session = sessions[0];
  const initialWidth = (await owner.locator("#workspace-panel").boundingBox()).width;
  monitor = await openMonitor(owner);
  second = await openMonitor(monitor);
  assert.equal(new URL(monitor.url()).searchParams.get("monitor"), session.id);
  assert.equal(new URL(second.url()).searchParams.get("monitor"), session.id);
  assert.equal(await monitor.evaluate(() => window.opener), null, "new tabs must be independent");
  assert.equal(await monitor.locator("#workspace-panel").isVisible(), false);
  assert.equal(await monitor.locator("#new-session").isVisible(), false);
  assert.equal(await monitor.locator("#toggle-monitor").isVisible(), false);
  assert.equal((await owner.locator("#workspace-panel").boundingBox()).width, initialWidth);
  assert.equal(sessions.length, 1, "opening monitors must not create sessions");

  await owner.locator("#toggle-monitor").click();
  assert.equal(await owner.locator("#monitor-panel").isVisible(), false);
  assert.ok((await owner.locator("#workspace-panel").boundingBox()).width > initialWidth + 400);
  assert.equal(await owner.locator("#panel-resizer").isVisible(), false);
  await command("printf 'shared-preview-content\\n' > /tmp/monitor-preview.txt; cat /tmp/monitor-preview.txt");
  await until(() => events.some(event => event.kind === "file.open" && event.data.path === "/tmp/monitor-preview.txt" && event.data.preview?.status === "complete"), "file preview recorded");
  await caughtUp(monitor);
  await caughtUp(second);
  const preview = events.find(event => event.kind === "file.open" && event.data.path === "/tmp/monitor-preview.txt" && event.data.preview?.status === "complete");
  const previewProcess = monitor.locator(`.process-node[data-pid="${preview.data.pid}"]`).last();
  await previewProcess.evaluate(node => {
    for (let parent = node; parent; parent = parent.parentElement) if (parent.tagName === "DETAILS") parent.open = true;
  });
  await previewProcess.locator(".event-row").filter({ hasText: "file.open" }).filter({ hasText: "/tmp/monitor-preview.txt" }).last().click();
  assert.equal(await monitor.locator("#selected-sequence").textContent(), `#${preview.sequence}`);
  await until(async () => (await monitor.locator(".file-inspector-body").textContent()).includes("shared-preview-content"), "monitor file preview");
  await monitor.locator("#event-detail summary").click();
  assert.ok((await monitor.locator("#event-json").textContent()).includes("/tmp/monitor-preview.txt"));

  await owner.locator("#toggle-monitor").click();
  assert.equal((await owner.locator("#workspace-panel").boundingBox()).width, initialWidth);
  assert.equal(await owner.locator("#panel-resizer").isVisible(), true);
  await monitor.locator("#event-filter summary").click();
  await monitor.getByRole("checkbox", { name: "file.*", exact: true }).uncheck();
  assert.equal(await second.locator('input[aria-label="file.*"]').isChecked(), true);
  assert.equal(await owner.locator('input[aria-label="file.*"]').isChecked(), true);
  await monitor.locator("#event-filter summary").click();
  console.log("PASS: icon controls, independent monitors, file preview and filters");

  await command(`curl --noproxy '*' -s '${httpUrl}'`);
  await until(() => events.some(event => event.kind === "network.http.response.body"), "HTTP body captured");
  await caughtUp(second);
  await second.locator("[data-observation=traffic]").click();
  await second.locator(".traffic-row").filter({ hasText: httpUrl }).click();
  await until(async () => (await second.locator(".traffic-body").textContent()).includes("shared-monitor-response"), "decoded HTTP body in detached monitor");
  assert.equal(await owner.locator("#trace-scroll").isVisible(), true, "view selection stays local");
  await owner.locator(".process-node").evaluateAll(nodes => nodes.forEach(node => node.open = true));
  await owner.locator("#trace-scroll").evaluate(node => { node.scrollTop = 173; });
  await owner.locator("#toggle-monitor").click();
  await command("uname -s");
  await until(() => events.some(event => event.kind === "process.exec" && event.data.argv?.join(" ").includes("uname -s")), "activity while collapsed");
  await owner.locator("#toggle-monitor").click();
  await until(async () => Math.abs(await owner.locator("#trace-scroll").evaluate(node => node.scrollTop) - 173) < 2, "expanding must preserve the reader's scroll position", 3000);
  await caughtUp(monitor);
  for (const page of [monitor, second]) {
    const ids = observed.get(page).map(event => event.sequence);
    assert.equal(new Set(ids).size, ids.length, "each cursor must deliver events once");
  }
  await second.reload();
  await until(async () => await second.locator("#status").textContent() === "Following session", "refresh resumes same session");
  await second.locator("[data-observation=traffic]").click();
  await second.locator(".traffic-row").filter({ hasText: httpUrl }).click();
  await until(async () => (await second.locator(".traffic-body").textContent()).includes("shared-monitor-response"), "HTTP replay after refresh");
  assert.equal(sessions.length, 1);
  assert.deepEqual(await readdir(directory), [session.id]);
  console.log("PASS: HTTP inspector and same-session refresh");

  // A failed poll must resume from the existing cursor, not create a session.
  await monitor.route("**/events?*", route => route.abort(), { times: 1 });
  await until(async () => await monitor.locator("#status").textContent() === "Reconnecting", "retry status");
  await command("uname -a");
  await until(async () => await monitor.locator("#status").textContent() === "Following session", "retry succeeds");
  await caughtUp(monitor);
  await second.close();
  await command("cat /etc/hostname");
  await until(() => events.some(event => event.kind === "file.open" && event.data.path === "/etc/hostname"), "owner still runs after monitor closes");
  await caughtUp(monitor);
  assert.equal(docker("inspect", "--format", "{{.State.Running}}", session.container).trim(), "true");

  for (const width of [768, 390, 320]) {
    for (const page of [owner, monitor]) {
      await page.setViewportSize({ width, height: 900 });
      assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth + 1), `overflow at ${width}`);
    }
  }
  const invalid = await context.newPage();
  await invalid.goto(`${url}&monitor=invalid`);
  await until(async () => await invalid.locator("#status").textContent() === "Session error", "invalid monitor must fail read-only");
  assert.equal(sockets.filter(socket => socket.page !== owner).length, 0, "monitors must never open owner or desktop WebSockets");
  await invalid.close();
  await owner.close();
  await until(async () => await monitor.locator("#status").textContent() === "Session ended", "closing owner ends the existing session");
  assert.equal(JSON.parse(await readFile(`${directory}/${session.id}/session-result.json`)).container_removed, true);
  await monitor.reload();
  await until(async () => await monitor.locator("#status").textContent() === "Session ended", "archived session can be replayed");
  assert.ok(Number((await monitor.locator("#event-count").textContent()).replaceAll(",", "")) > 0);
  assert.equal(sessions.length, 1);

  // Normal completion still drains the final event before the monitor ends.
  owner = await context.newPage();
  await owner.goto(url);
  await until(async () => await owner.locator("#status").textContent() === "Connected", "second workspace ready");
  second = await openMonitor(owner);
  await command("exit");
  await until(async () => await second.locator("#status").textContent() === "Collection complete", "complete monitor");
  const ended = sessions[1];
  const journal = (await readFile(`${directory}/${ended.id}/run/events.jsonl`, "utf8")).trim().split("\n").map(JSON.parse);
  assert.deepEqual(observed.get(second).map(event => event.sequence), journal.map(event => event.sequence), "complete replay includes the last event exactly once");
  assert.deepEqual(pageErrors, []);
  assert.equal(errors, "");
  console.log(JSON.stringify({ result: "PASS", checks: ["two icon controls", "independent live monitors", "read-only refresh", "local filters", "file preview and raw event", "decoded HTTP body", "retry cursor", "monitor close", "owner cleanup", "archived replay", "final event", "narrow layout"], events: events.length }));
} catch (error) {
  console.error("Playground stderr:", errors);
  if (monitor && !monitor.isClosed()) console.error("Monitor state:", await monitor.locator("#file-preview").textContent(), await monitor.locator("#status").textContent());
  console.error("Preview events:", JSON.stringify(events.filter(event => event.kind === "file.open" && event.data.path === "/tmp/monitor-preview.txt").map(event => ({sequence:event.sequence, data:event.data}))));
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
  for (const session of sessions) {
    try { docker("rm", "-f", session.container); } catch { /* already removed */ }
  }
  await rm(directory, { recursive: true, force: true });
}
