// Opt-in real Docker + browser regression. Uses the existing local tools image;
// no remote websites, image pulls or public listening ports are needed.
import assert from "node:assert/strict";
import { spawn, execFileSync } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, readFile, readdir, rm } from "node:fs/promises";
import { createServer } from "node:http";
import { createSocket } from "node:dgram";
import { fileURLToPath } from "node:url";
import { gzipSync } from "node:zlib";

const { chromium } = await import(process.env.AGENTTOMO_PLAYWRIGHT_MODULE || "playwright");
const repo = fileURLToPath(new URL("../../..", import.meta.url));
const directory = await mkdtemp("/tmp/agenttomo-monitor-pages-");
const docker = (...args) => execFileSync("docker", args, { encoding: "utf8", timeout: 10000, stdio: ["ignore", "pipe", "pipe"] });
const bridge = JSON.parse(docker("network", "inspect", "bridge"))[0].IPAM.Config[0].Gateway;
let releaseSecond;
const secondResponse = new Promise(resolve => releaseSecond = resolve);
const fixture = createServer(async (_req, response) => {
  response.setHeader("content-type", "text/plain; charset=utf-8");
  response.setHeader("content-encoding", "gzip");
  response.setHeader("connection", _req.url.endsWith("?second") ? "close" : "keep-alive");
  if (_req.url.endsWith("?second")) await secondResponse;
  response.end(gzipSync("shared-monitor-response"));
});
fixture.listen(0, bridge);
await once(fixture, "listening");
const httpUrl = `http://${bridge}:${fixture.address().port}/monitor-test`;
const udpFixture = createSocket("udp4");
udpFixture.on("message", (bytes, peer) => udpFixture.send(bytes, peer.port, peer.address));
udpFixture.bind(0, bridge);
await once(udpFixture, "listening");
const udpDestination = `${bridge}:${udpFixture.address().port}`;
const server = spawn(`${repo}/target/debug/agenttomo-box`, [
  "playground", "--listen", "127.0.0.1:0", "--output-dir", directory,
  "--image", "agenttomo-playground:24.04", "--capture-http",
], { detached: true, env: {...process.env, HTTP_PROXY:"", HTTPS_PROXY:"", ALL_PROXY:"", http_proxy:"", https_proxy:"", all_proxy:""}, stdio: ["ignore", "pipe", "pipe"] });
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
async function expandGraph(page, event) {
  await page.locator('[data-observation="graph"]').click();
  const process = page.locator(`.graph-process-card[data-pid="${event.data.pid}"][data-instance="${event.data.process_instance_id}"]`);
  await process.waitFor({ state: "attached" });
  await process.evaluate(button => {
    const ancestors = [];
    for (let branch = button.closest(".graph-branch"); branch; branch = branch.parentElement.closest(".graph-branch")) ancestors.unshift(branch);
    for (const branch of ancestors) {
      const control = branch.querySelector(":scope > .graph-layout > .graph-process-card");
      if (control.getAttribute("aria-expanded") === "false") control.click();
    }
  });
  return process;
}

try {
  await until(() => { assert.equal(server.exitCode, null, errors); return output.includes("?token="); }, "server start");
  url = output.match(/http:\/\/\S+/)[0];
  browser = process.env.AGENTTOMO_CDP ? await chromium.connectOverCDP(process.env.AGENTTOMO_CDP)
    : await chromium.launch({ executablePath: process.env.AGENTTOMO_TEST_BROWSER || undefined,
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
  assert.equal(await owner.locator('[data-observation="graph"]').count(), 1, "the live monitor must expose the behavior graph");
  const session = sessions[0];
  const initialWidth = (await owner.locator("#workspace-panel").boundingBox()).width;
  monitor = await openMonitor(owner);
  second = await openMonitor(monitor);
  assert.equal(new URL(monitor.url()).searchParams.get("monitor"), session.id);
  assert.equal(new URL(second.url()).searchParams.get("monitor"), session.id);
  assert.equal(await monitor.evaluate(() => window.opener), null, "new tabs must be independent");
  assert.equal(await monitor.locator("#workspace-panel").isVisible(), false);
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
  assert.deepEqual(await monitor.locator(".event-row .event-type").allTextContents().then(kinds => kinds.filter(kind => ["exec", "exit", "fork", "vfork", "clone"].includes(kind))), [], "process lifecycle must not be duplicated as behavior rows");
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

  await expandGraph(owner, preview);
  assert.equal(await owner.locator('.graph-event-card[data-kind^="process."]').count(), 0, "lifecycle belongs to the process card, not separate event cards");
  const lifecycle = owner.locator(`.graph-process-card[data-instance="${preview.data.process_instance_id}"]`).locator("..").locator(":scope > .graph-body > .graph-context > .event-evidence");
  await lifecycle.locator("summary").click();
  const execEvent = events.find(event => event.kind === "process.exec" && event.data.process_instance_id === preview.data.process_instance_id);
  await lifecycle.locator(`[data-sequence="${execEvent.sequence}"]`).click();
  assert.equal(await owner.locator("#selected-sequence").textContent(), `#${execEvent.sequence}`);
  assert.equal(await owner.locator("#graph-scroll").isVisible(), true);
  await lifecycle.locator("summary").click();
  const fileCard = owner.locator(`.graph-event-card[data-sequence="${preview.sequence}"]`);
  await fileCard.click();
  await until(async () => (await owner.locator(".file-inspector-body").textContent()).includes("shared-preview-content"), "graph file card must open the captured file");
  assert.equal(await owner.locator("#selected-sequence").textContent(), `#${preview.sequence}`);
  await owner.locator("#event-detail summary").click();
  assert.ok((await owner.locator("#event-json").textContent()).includes("/tmp/monitor-preview.txt"));
  await owner.locator("#event-detail summary").click();
  await owner.getByRole("button", { name: "Close file preview", exact: true }).click();
  await owner.locator("#event-filter summary").click();
  await owner.getByRole("checkbox", { name: "file.*", exact: true }).uncheck();
  await until(async () => await owner.locator('.graph-event-card[data-kind="file.open"]').count() === 0, "graph applies file wildcard filter");
  await owner.locator('[data-observation="process"]').click();
  assert.equal(await owner.locator('input[aria-label="file.*"]').isChecked(), false, "graph and tree share filter state");
  await owner.getByRole("checkbox", { name: "file.*", exact: true }).check();
  await owner.locator("#event-filter summary").click();
  await owner.locator('[data-observation="graph"]').click();
  await fileCard.waitFor();
  await expandGraph(second, preview);
  const secondFile = second.locator(`.graph-event-card[data-sequence="${preview.sequence}"]`);
  await secondFile.focus();
  const focusedCard = await secondFile.elementHandle();
  // Activity from another page must preserve this card and its keyboard focus.
  await command("sh -c 'sleep 1; uname -m'");
  await until(() => events.some(event => event.kind === "process.exec" && event.data.argv?.join(" ").includes("uname -m")), "graph receives a new real process");
  await caughtUp(second);
  assert.ok(await focusedCard.evaluate(node => node.isConnected && node === document.activeElement), "live graph updates must preserve the focused card");
  await focusedCard.dispose();
  const ids = await owner.locator(".graph-process-card").evaluateAll(cards => cards.map(card => card.dataset.instance));
  assert.equal(new Set(ids).size, ids.length, "graph must have one card per process lifetime");
  const graphTop = await owner.locator("#graph-scroll").evaluate(node => { node.scrollTop = 93; return node.scrollTop; });
  await owner.locator("#toggle-monitor").click();
  await command("sh -c 'sleep 1; printf graph-hidden-update; sleep 1'");
  await until(() => events.some(event => event.kind === "process.exec" && event.data.argv?.[0] === "sleep" && event.sequence > preview.sequence), "hidden graph receives live work");
  await owner.waitForTimeout(1600);
  await owner.locator("#toggle-monitor").click();
  assert.ok(Math.abs(await owner.locator("#graph-scroll").evaluate(node => node.scrollTop) - graphTop) < 2, "hidden graph must retain its reading position");
  console.log("PASS: real graph cards, file inspection, shared filters and stable process identities");

  // Send both requests through one actual socket, independent of a client's
  // connection-pool policy. The second response explicitly closes the stream.
  const authority = `${bridge}:${fixture.address().port}`;
  await command(`bash -c 'exec 3<>/dev/tcp/${bridge}/${fixture.address().port}; printf "GET /monitor-test HTTP/1.1\\r\\nHost: ${authority}\\r\\nConnection: keep-alive\\r\\n\\r\\nGET /monitor-test?second HTTP/1.1\\r\\nHost: ${authority}\\r\\nConnection: close\\r\\n\\r\\n" >&3; cat <&3 >/dev/null'`);
  await until(() => events.filter(event => event.kind === "network.http.request").length === 2, "both keep-alive requests captured");
  await caughtUp(second);
  const httpEvent = events.find(event => event.kind === "network.http.request" && event.data.url === httpUrl);
  const nextHttp = events.find(event => event.kind === "network.http.request" && event.data.url === `${httpUrl}?second`);
  assert.equal(nextHttp.data.connection_id, httpEvent.data.connection_id, "fixture must actually reuse the TCP connection");
  await second.locator('[data-observation="process"]').click();
  const traceProcess = second.locator(`.process-node[data-instance="${httpEvent.data.process_instance_id}"]`);
  await traceProcess.evaluate(node => {
    for (let parent = node; parent; parent = parent.parentElement) if (parent.tagName === "DETAILS") parent.open = true;
  });
  const traceFlow = second.locator(`.trace-network-flow[data-connection-id="${httpEvent.data.connection_id}"]`);
  await traceFlow.waitFor();
  await second.locator("#event-filter summary").click();
  await second.locator('[data-filter-action="none"]').click();
  await second.getByRole("checkbox", { name: "network.*", exact: true }).check();
  await second.locator("#event-filter summary").click();
  await until(async () => await second.locator("#event-count").textContent() === "3", "TCP plus two HTTP exchanges count once in the process list");
  const traceRequests = traceFlow.locator(":scope > .flow-behaviors > .behavior-item > .event-row");
  assert.deepEqual(await traceRequests.evaluateAll(rows => rows.map(row => Number(row.dataset.requestId))), [httpEvent.data.request_id, nextHttp.data.request_id]);
  const pendingRequest = traceRequests.nth(1);
  assert.match(await pendingRequest.textContent(), /Pending/);
  await pendingRequest.focus();
  const pendingHandle = await pendingRequest.elementHandle();
  const pendingCount = await second.locator("#event-count").textContent();
  releaseSecond();
  await until(() => events.filter(event => event.kind === "network.http.response.body").length === 2, "both keep-alive HTTP bodies captured");
  await until(() => events.some(event => event.kind === "network.transfer"), "TCP summary captured");
  await caughtUp(second);
  await until(async () => (await pendingRequest.textContent()).includes("200"), "response updates existing process-list request");
  assert.ok(await pendingHandle.evaluate(node => node.isConnected && node === document.activeElement), "HTTP response must preserve the process-list row and keyboard focus");
  await pendingHandle.dispose();
  assert.equal(await traceRequests.count(), 2);
  assert.equal(await second.locator("#event-count").textContent(), pendingCount, "response/body/transfer updates must not increase behavior counts");
  await pendingRequest.click();
  await until(async () => (await second.locator(".traffic-body").textContent()).includes("shared-monitor-response"), "process-list HTTP row opens its exchange");
  assert.equal(await second.locator(".traffic-row").count(), 2, "Traffic has one row per exchange, not per stage");
  await second.locator('[data-observation="process"]').click();
  await traceFlow.locator(":scope > .flow-toggle").click();
  assert.equal(await traceRequests.first().isVisible(), false);
  await traceFlow.locator(":scope > .event-row").click();
  assert.ok((await second.locator(".traffic-navigation").textContent()).includes(`Connection #${httpEvent.data.connection_id}`), "TCP row preserves connection-level Traffic navigation");
  await second.locator('[data-observation="process"]').click();
  assert.equal(await traceRequests.first().isVisible(), false, "view switches preserve flow collapse");
  await traceFlow.locator(":scope > .flow-toggle").click();
  if (process.env.AGENTTOMO_TEST_PROCESS_SCREENSHOT) {
    await traceFlow.scrollIntoViewIfNeeded();
    await second.screenshot({ path: process.env.AGENTTOMO_TEST_PROCESS_SCREENSHOT, fullPage: true });
  }
  await expandGraph(second, httpEvent);
  assert.equal(await second.locator("#event-count").textContent(), "3", "Graph and process list share the same logical count");
  await second.locator("#event-filter summary").click();
  await second.locator('[data-filter-action="all"]').click();
  await second.locator("#event-filter summary").click();
  const flow = second.locator(`.graph-network-flow[data-connection-id="${httpEvent.data.connection_id}"]`);
  await flow.waitFor();
  assert.equal(await flow.count(), 1, "one TCP parent per connection");
  const requests = flow.locator(":scope > .graph-flow-body > .graph-http-requests > .graph-event-item > .graph-event-card");
  assert.deepEqual(await requests.evaluateAll(cards => cards.map(card => Number(card.dataset.requestId))),
    [httpEvent.data.request_id, nextHttp.data.request_id], "keep-alive requests must be chronological siblings");
  assert.match(await requests.first().textContent(), /200/);
  assert.equal(await second.locator('.graph-event-card[data-kind="network.http.response"], .graph-event-card[data-kind="network.http.response.body"], .graph-event-card[data-kind="network.protocol"]').count(), 0, "stages must not appear as independent graph cards");
  const flowButton = flow.locator(":scope > .graph-event-card");
  const graphProcess = second.locator(`.graph-process-card[data-instance="${httpEvent.data.process_instance_id}"]`);
  const parentBox = await graphProcess.boundingBox(), flowBox = await flowButton.boundingBox();
  const requestBoxes = await requests.evaluateAll(cards => cards.map(card => {
    const box = card.getBoundingClientRect();
    return { x: box.x, y: box.y, height: box.height };
  }));
  assert.ok(flowBox.x > parentBox.x && flowBox.y >= parentBox.y + parentBox.height, "TCP must be indented below its owning process");
  assert.ok(requestBoxes[0].x > flowBox.x && requestBoxes[0].y >= flowBox.y + flowBox.height, "HTTP must be indented below its TCP parent");
  assert.ok(Math.abs(requestBoxes[0].x - requestBoxes[1].x) < 1 && requestBoxes[1].y >= requestBoxes[0].y + requestBoxes[0].height, "HTTP siblings must be vertically stacked at the same tree depth");
  await flowButton.click();
  assert.equal(await requests.first().isVisible(), false);
  await graphProcess.click();
  assert.equal(await flowButton.isVisible(), false, "collapsing a process hides its whole subtree");
  await command("printf 'flow-stays-collapsed\\n'");
  await caughtUp(second);
  assert.equal(await graphProcess.getAttribute("aria-expanded"), "false", "new events must not reopen a collapsed parent");
  await graphProcess.click();
  assert.equal(await flowButton.getAttribute("aria-expanded"), "false");
  assert.equal(await requests.first().isVisible(), false, "expanding a process preserves its TCP child's collapsed state");
  await flowButton.click();
  await flow.locator(":scope > .graph-flow-body > .graph-context > .event-evidence > summary").click();
  const evidence = flow.locator(`.evidence-record[data-sequence="${httpEvent.sequence}"]`);
  await evidence.click();
  assert.equal(await second.locator("#graph-scroll").isVisible(), true, "raw evidence inspection stays in Graph");
  assert.equal(await second.locator("#selected-sequence").textContent(), `#${httpEvent.sequence}`);
  await flow.locator(":scope > .graph-flow-body > .graph-context > .event-evidence > summary").click();
  await second.locator("#event-detail summary").click();
  const httpCard = second.locator(`.graph-event-card[data-sequence="${httpEvent.sequence}"]`);
  await httpCard.scrollIntoViewIfNeeded();
  assert.match(await httpCard.locator("time").textContent(), /^\d{2}:\d{2}:\d{2}\.\d{3}$/, "graph must show the full operation start time, not only fractional seconds");
  if (process.env.AGENTTOMO_TEST_SCREENSHOT) {
    await second.locator("#event-filter summary").click();
    await second.locator('[data-filter-action="none"]').click();
    await second.getByRole("checkbox", { name: "network.*", exact: true }).check();
    await second.locator("#event-filter summary").click();
    await graphProcess.scrollIntoViewIfNeeded();
    await second.screenshot({ path: process.env.AGENTTOMO_TEST_SCREENSHOT, fullPage: true });
    await second.locator("#event-filter summary").click();
    await second.locator('[data-filter-action="all"]').click();
    await second.locator("#event-filter summary").click();
  }
  await httpCard.click();
  assert.equal(await second.locator("#traffic").isVisible(), true, "HTTP card opens the Traffic inspector");
  await until(async () => (await second.locator(".traffic-body").textContent()).includes("shared-monitor-response"), "decoded HTTP body in detached monitor");
  assert.equal(await owner.locator("#graph-scroll").isVisible(), true, "view selection stays local");
  await command(`bash -c 'exec 3<>/dev/udp/${bridge}/${udpFixture.address().port}; printf "one\\n" >&3; IFS= read -r -t 2 reply <&3; printf "two\\n" >&3; IFS= read -r -t 2 reply <&3'`);
  await until(() => events.filter(event => event.kind === "network.udp.receive" && event.data.destination === udpDestination).length === 2, "UDP echo replies captured");
  const udpEvent = events.find(event => event.kind === "network.udp.send" && event.data.destination === udpDestination);
  await caughtUp(second);
  await second.locator('[data-observation="process"]').click();
  const udpProcess = second.locator(`.process-node[data-instance="${udpEvent.data.process_instance_id}"]`);
  await udpProcess.evaluate(node => {
    for (let parent = node; parent; parent = parent.parentElement) if (parent.tagName === "DETAILS") parent.open = true;
  });
  const udpRow = second.locator(`.trace-network-flow[data-flow-id="${udpEvent.data.flow_id}"] > .event-row`);
  assert.equal(await udpRow.count(), 1);
  assert.match(await udpRow.textContent(), /↑ 2 packets · 8 B.*↓ 2 packets · 8 B/s);
  await udpRow.click();
  await until(async () => await second.locator(`.trace-network-flow[data-flow-id="${udpEvent.data.flow_id}"] .evidence-record`).count() === 4, "UDP packet evidence stays expandable in the process list");
  await expandGraph(second, udpEvent);
  const udpCard = second.locator(`.graph-network-flow[data-flow-id="${udpEvent.data.flow_id}"] > .graph-event-card`);
  assert.equal(await udpCard.count(), 1);
  assert.match(await udpCard.textContent(), /↑ 2 packets · 8 B.*↓ 2 packets · 8 B/s);
  await udpCard.click();
  await until(async () => await udpCard.getAttribute("aria-expanded") === "true", "UDP card exposes its actual details expansion state");
  assert.equal(await second.locator(`.graph-network-flow[data-flow-id="${udpEvent.data.flow_id}"] .evidence-record`).count(), 4);
  console.log("PASS: TCP parent, chronological keep-alive children, retained evidence and grouped UDP");
  await owner.locator('[data-observation="process"]').click();
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
  await second.locator(".traffic-row").filter({ hasText: httpUrl }).first().click();
  await until(async () => (await second.locator(".traffic-body").textContent()).includes("shared-monitor-response"), "HTTP replay after refresh");
  assert.equal(sessions.length, 1);
  assert.deepEqual(await readdir(directory), [], "live recordings must stay in the container");
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
      await expandGraph(page, httpEvent);
      await page.setViewportSize({ width, height: 900 });
      assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth + 1), `overflow at ${width}`);
      assert.ok(await page.locator("#graph-scroll").evaluate(node => node.scrollWidth <= node.clientWidth + 1), `graph overflow at ${width}`);
      const card = page.locator(`.graph-event-card[data-request-id="${httpEvent.data.request_id}"]`);
      const parent = page.locator(`.graph-network-flow[data-connection-id="${httpEvent.data.connection_id}"] > .graph-event-card`);
      const childBox = await card.boundingBox(), parentBox = await parent.boundingBox();
      assert.ok(childBox.x > parentBox.x && childBox.y >= parentBox.y + parentBox.height, `HTTP hierarchy must remain visible at ${width}`);
      await page.locator('[data-observation="process"]').click();
      await page.locator(".process-node").evaluateAll(nodes => nodes.forEach(node => node.open = true));
      assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth + 1), `process view overflow at ${width}`);
      assert.ok(await page.locator("#trace-scroll").evaluate(node => node.scrollWidth <= node.clientWidth + 1), `nested TCP/HTTP rows overflow at ${width}`);
    }
  }
  const invalid = await context.newPage();
  await invalid.goto(`${url}&monitor=invalid`);
  await until(async () => await invalid.locator("#status").textContent() === "Session error", "invalid monitor must fail read-only");
  assert.equal(sockets.filter(socket => socket.page !== owner).length, 0, "monitors must never open owner or desktop WebSockets");
  await invalid.close();
  await owner.close();
  await until(async () => await monitor.locator("#status").textContent() === "Session ended", "closing owner ends the existing session");
  assert.ok(!(await readdir(directory)).includes(session.id), "ended session records must be destroyed");
  assert.equal(docker("ps", "-a", "--filter", `name=${session.container}`, "--format", "{{.Names}}").trim(), "");
  assert.match(await monitor.locator("#notice").textContent(), /deleted when the sandbox ends/);
  await monitor.reload();
  await until(async () => await monitor.locator("#status").textContent() === "Session ended", "destroyed session cannot be replayed");
  assert.equal(Number((await monitor.locator("#event-count").textContent()).replaceAll(",", "")), 0);
  assert.equal(sessions.length, 1);

  // Normal completion drains the owner's events, then destroys the recording.
  owner = await context.newPage();
  await owner.goto(url);
  await until(async () => await owner.locator("#status").textContent() === "Connected", "second workspace ready");
  second = await openMonitor(owner);
  await command("exit");
  await until(async () => await owner.locator("#status").textContent() === "Collection complete", "owner completion");
  await until(async () => await second.locator("#status").textContent() === "Session ended", "expired monitor");
  const ended = sessions[1];
  assert.ok(!(await readdir(directory)).includes(ended.id));
  assert.deepEqual(pageErrors, []);
  assert.equal(errors, "");
  console.log(JSON.stringify({ result: "PASS", checks: ["two icon controls", "independent live monitors", "read-only refresh", "local filters", "shared logical counts", "process/graph TCP and HTTP hierarchy", "live HTTP row identity and focus", "UDP flow aggregation", "process details", "file/traffic navigation", "file preview and raw event", "decoded HTTP body", "retry cursor", "monitor close", "owner cleanup", "recording destruction and expired monitor", "final event", "both tracking layouts at 320/390/768 px"], events: events.length }));
} catch (error) {
  console.error("Playground stderr:", errors);
  if (monitor && !monitor.isClosed()) console.error("Monitor state:", await monitor.locator("#file-preview").textContent(), await monitor.locator("#status").textContent());
  if (second && !second.isClosed()) console.error("Second monitor state:", await second.locator("#status").textContent());
  console.error("Preview events:", JSON.stringify(events.filter(event => event.kind === "file.open" && event.data.path === "/tmp/monitor-preview.txt").map(event => ({sequence:event.sequence, data:event.data}))));
  throw error;
} finally {
  releaseSecond();
  await browser?.close();
  fixture.close();
  udpFixture.close();
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
