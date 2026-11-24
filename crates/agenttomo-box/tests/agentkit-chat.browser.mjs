// Real Go subprocess and normal Playground UI, with a local streaming Ark endpoint.
import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:https";
import { tmpdir } from "node:os";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";
import { assertSandboxSecretsAbsent } from "./credential-scan.mjs";

const { chromium } = await import(process.env.AGENTTOMO_PLAYWRIGHT_MODULE || "playwright");
assert.ok(process.env.AGENTTOMO_AGENTKIT_BIN, "Set AGENTTOMO_AGENTKIT_BIN to the external Go executable.");
const repo = fileURLToPath(new URL("../../..", import.meta.url));
const directory = await mkdtemp(`${tmpdir()}/agenttomo-agentkit-chat-`);
const gateway = JSON.parse(execFileSync("docker", ["network", "inspect", "bridge"]))[0].IPAM.Config[0].Gateway;
const key = "fixture-ark-key-for-local-browser-test";
// Only the host gateway trusts this CA. The Go client must use the sandbox CA.
const openssl = (...args) => execFileSync("openssl", args, {cwd: directory, stdio: "ignore"});
openssl("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-subj", "/CN=Local Ark test CA",
  "-keyout", "ca.key", "-out", "ca.pem", "-addext", "basicConstraints=critical,CA:TRUE");
openssl("req", "-new", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=Local Ark test server",
  "-keyout", "server.key", "-out", "server.csr");
await writeFile(`${directory}/extensions`, `basicConstraints=CA:FALSE\nsubjectAltName=IP:${gateway}\nextendedKeyUsage=serverAuth\n`);
openssl("x509", "-req", "-in", "server.csr", "-CA", "ca.pem", "-CAkey", "ca.key", "-CAcreateserial",
  "-days", "1", "-extfile", "extensions", "-out", "server.pem");
const requests = [];
const tls = {cert: await readFile(`${directory}/server.pem`), key: await readFile(`${directory}/server.key`)};
const fixture = createServer(tls, async (req, res) => {
  const chunks = [];
  for await (const chunk of req) chunks.push(chunk);
  const body = JSON.parse(Buffer.concat(chunks));
  const turn = requests.length + 1;
  const write = (delta, finish_reason = null) => {
    const choice = { index: 0, delta, finish_reason };
    res.write(`data: ${JSON.stringify({ id: `chat-${turn}`, object: "chat.completion.chunk",
      model: "fixture-model", created: 1, choices: [choice] })}\n\n`);
  };
  const request = { body, route: req.url, method: req.method, authorized: req.headers.authorization === `Bearer ${key}`,
    finish() { write({ content: "完成" }); write({}, "stop"); res.end("data: [DONE]\n\n"); } };
  requests.push(request);
  if (body.messages.at(-1).content === "测试鉴权失败") {
    res.writeHead(401, { "content-type": "application/json" });
    res.end(JSON.stringify({error: {code: "InvalidAPIKey", message: "fixture denied"}}));
    return;
  }
  res.writeHead(200, { "content-type": "text/event-stream" });
  write({ role: "assistant", content: `第${turn}轮：` });
});
fixture.keepAliveTimeout = 60_000;
fixture.listen(0, gateway);
await once(fixture, "listening");
const server = spawn(`${repo}/target/debug/agenttomo-box`, [
  "playground", "--listen", "127.0.0.1:0", "--image", "agenttomo-playground:24.04",
  "--no-proxy", "--output-dir", `${directory}/sessions`,
], { env: { ...process.env, CODEX_HOME: `${directory}/unused-codex`, MODEL_AGENT_NAME: "fixture-model",
  SSL_CERT_FILE: `${directory}/ca.pem`,
  MODEL_AGENT_API_BASE: `https://${gateway}:${fixture.address().port}/api/v3`, MODEL_AGENT_API_KEY: key },
  stdio: ["ignore", "pipe", "pipe"] });
let output = "", errors = "", browser, page, session;
const events = [], pageErrors = [];
server.stdout.on("data", chunk => { output += chunk; });
server.stderr.on("data", chunk => { errors += chunk; });
const finished = () => events.filter(event => event.kind === "agent.task.finished");
async function until(label, predicate) {
  const end = Date.now() + 45_000;
  while (!await predicate()) {
    const info = () => events.filter(event => /agent\.task\.|agent\.capture\.error/.test(event.kind))
      .slice(-3).map(event => [event.kind, event.data.status, event.data.error]);
    if (server.exitCode !== null || Date.now() > end) {
      throw Error(`${label}. Requests received: ${requests.length}. Events: ${JSON.stringify(info())}. ${errors}`);
    }
    await delay(50);
  }
}
try {
  await until("Playground startup", () => output.includes("?token="));
  browser = await chromium.connectOverCDP(process.env.AGENTTOMO_CDP || "http://127.0.0.1:19222");
  page = await browser.newPage();
  await page.addInitScript(() => {
    const WebSocket = window.WebSocket;
    window.WebSocket = class extends WebSocket {
      constructor(...args) {
        super(...args);
        if (new URL(args[0]).pathname === "/ws") window.testOwnerSocket = this;
      }
    };
  });
  page.on("pageerror", error => pageErrors.push(error.message));
  page.on("websocket", socket => socket.on("framereceived", ({ payload }) => {
    if (typeof payload !== "string" || !payload.startsWith("{")) return;
    const frame = JSON.parse(payload);
    if (frame.type === "session") session = frame;
    if (frame.type === "trace") events.push(frame.event);
  }));
  const url = new URL(output.match(/http:\/\/\S+/)[0]);
  url.searchParams.set("agent", "agentkit");
  await page.goto(url.href);
  await until("AgentKit sandbox ready", async () => await page.locator("#status").textContent() === "Connected");
  assert.equal(session.agent.id, "agentkit");
  assert.equal(session.agent.supports_security_tests, true);
  await page.locator("[data-workspace=samples]").click();
  await page.locator('[data-sample-id="tool-leak"]').click();
  assert.ok(await page.locator(".sample-run").isEnabled());
  await page.locator("#workspace-agent summary").click();
  const choice = page.getByRole("radio", { name: "agentkit" });
  assert.ok(await choice.isEnabled() && await choice.isChecked());
  await choice.press("Escape");
  await page.locator("[data-workspace=chat]").click();
  const send = page.locator(".agent-send");
  const answers = page.locator(".agent-answer .agent-message-text");
  async function prompt(text) {
    await page.getByRole("textbox", {name: "Task for agentkit"}).fill(text);
    await send.click();
  }
  async function partial(turn, text) {
    await prompt(text);
    await until(`Turn ${turn} visible before the local endpoint finishes`, async () =>
      (await answers.allTextContents()).includes(`第${turn}轮：`));
    assert.equal(finished().length, turn - 1);
  }
  async function complete(turn, text) {
    await partial(turn, text);
    if (turn === 1) {
      const id = "f".repeat(32);
      await page.evaluate(task_id => window.testOwnerSocket.send(JSON.stringify({
        type: "agent_prompt", task_id, prompt: "重叠的一轮",
      })), id);
      await until("Overlapping prompt rejected", () => events.some(event =>
        event.kind === "agent.task.rejected" && event.data.task_id === id));
      assert.equal(requests.length, 1);
    }
    requests[turn - 1].finish();
    await until(`Turn ${turn} complete`, () => finished().length === turn);
    await until(`Turn ${turn} single final answer`, async () =>
      (await answers.allTextContents()).filter(answer => answer.trimEnd() === `第${turn}轮：完成`).length === 1);
    const task = finished()[turn - 1].data;
    assert.equal(task.status, "completed");
    const trace = events.filter(event => event.data.task_id === task.task_id);
    assert.equal(trace.filter(event => event.kind === "agent.model.request").length, 1);
    assert.equal(trace.filter(event => event.kind === "agent.message").length, 1);
    assert.equal(trace.find(event => event.kind === "agent.model.complete")?.data.response_id, `chat-${turn}`);
  }
  const dialog = index => requests[index].body.messages.filter(msg => msg.role !== "system")
    .map(({role, content}) => [role, content]);
  await complete(1, "记住青竹");
  await complete(2, "刚才说了什么？");
  assert.deepEqual(dialog(1), [["user", "记住青竹"], ["assistant", "第1轮：完成"], ["user", "刚才说了什么？"]]);
  const modelCalls = events.filter(event => event.kind === "agent.model.request");
  assert.ok(modelCalls[0].data.connection_id != null);
  assert.equal(modelCalls[0].data.connection_id, modelCalls[1].data.connection_id, "Both turns used one keep-alive connection.");
  assert.notEqual(modelCalls[0].data.task_id, modelCalls[1].data.task_id);
  await page.locator("[data-observation=agent]").click();
  await page.locator('#agent-trajectory .agent-card[data-category="model"] .agent-node-inspect').first().click();
  const response = page.locator("#agent-detail .agent-content-section").nth(1);
  await until("Captured reply opens in model details", async () => (await response.textContent()).includes("第1轮：完成"));
  await page.getByRole("button", {name: "Close detail"}).click();
  await partial(3, "等待停止");
  await page.locator(".agent-stop").click();
  await until("Stop acknowledged", () => finished().length === 3);
  assert.ok(finished()[2].data.cancelled && finished()[2].data.context_reset);
  await until("Context reset notice", async () => (await page.locator(".agent-history").textContent()).includes("Next prompt starts a new conversation"));
  await complete(4, "新会话");
  assert.deepEqual(dialog(3), [["user", "新会话"]]);
  await prompt("测试鉴权失败");
  await until("HTTP error reaches Chat", async () => finished().length === 5 &&
    (await page.locator(".agent-error").allTextContents()).some(text => text.includes("401")));
  assert.equal(finished()[4].data.status, "failed");
  assert.equal(new Set(finished().map(event => event.data.task_id)).size, 5);
  assert.ok(requests.every(req => req.authorized && req.method === "POST" && req.route === "/api/v3/chat/completions"));
  assert.ok(requests.every(({body}) => body.stream === true && body.model === "fixture-model"
    && body.tools?.length === 1 && body.tools[0].function.name === "exec_command"));
  assert.ok(events.every(event => !event.kind.startsWith("agent.tool.")));
  await assertSandboxSecretsAbsent(session.container, [key]);
  assert.ok(!JSON.stringify(events).includes(key), "Sandbox evidence must contain only the session placeholder.");
  assert.deepEqual(pageErrors, []);
  console.log("PASS: AgentKit picker, streamed chat, history, task traces, stop/reset, HTTP 401 and credential isolation.");
} finally {
  await page?.close().catch(() => {});
  await browser?.close().catch(() => {});
  if (server.exitCode === null && server.signalCode === null) {
    const closed = once(server, "close");
    server.kill("SIGTERM");
    await closed;
  }
  fixture.closeAllConnections();
  fixture.close();
  await rm(directory, {recursive: true, force: true});
}
