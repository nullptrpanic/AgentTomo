// Real external Codex, deterministic local Responses server, normal Playground UI.
import assert from "node:assert/strict";
import { spawn, execFileSync } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, mkdir, writeFile, rm, readFile, readdir } from "node:fs/promises";
import { createServer } from "node:http";
import { createConnection } from "node:net";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";
import { zstdDecompressSync } from "node:zlib";
import { assertSandboxSecretsAbsent } from "./credential-scan.mjs";

const { chromium } = await import(process.env.AGENTTOMO_PLAYWRIGHT_MODULE || "playwright");
const repo = fileURLToPath(new URL("../../..", import.meta.url));
const directory = await mkdtemp(`${tmpdir()}/agenttomo-agent-chat-`);
const docker = (...args) => execFileSync("docker", args, { encoding:"utf8", timeout:15000 });
const gateway = JSON.parse(docker("network", "inspect", "bridge"))[0].IPAM.Config[0].Gateway;
let step = 0, holdResponses = false, recovering = false, rejectResponses = false;
const requests = [], responses = [], observed = [], modelAuthorization = [];
const hostCredential = "agenttomo-test-key-never-used-remotely";
const fixture = createServer(async (req, res) => {
  if (req.url.startsWith("/resource")) { res.end("RESOURCE_EVIDENCE"); return; }
  if (!req.url.endsWith("/responses")) { res.writeHead(404); res.end(); return; }
  modelAuthorization.push(req.headers.authorization);
  const chunks = []; for await (const chunk of req) chunks.push(chunk);
  let bytes = Buffer.concat(chunks);
  if (req.headers["content-encoding"] === "zstd") bytes = zstdDecompressSync(bytes);
  requests.push(JSON.parse(bytes));
  step++;
  if (holdResponses || step >= 8 && !rejectResponses && !recovering) { res.writeHead(200); res.flushHeaders(); return; }
  if (rejectResponses) {
    res.writeHead(401, {"content-type":"application/json"});
    res.end(JSON.stringify({error:{message:"Authentication rejected by the test endpoint",type:"authentication_error"}}));
    return;
  }
  if (step === 1) await new Promise(resolve => setTimeout(resolve,1500));
  const call = {
    type:"custom_tool_call", id:`ctc_chat_${step}`, call_id:`call_chat_${step}`, name:"exec", status:"completed",
    input:`const result = await tools.exec_command({cmd: "test -z \\\"$HTTP_PROXY$HTTPS_PROXY$ALL_PROXY$http_proxy$https_proxy$all_proxy\\\" || exit 91; printf 'AGENTTOMO_TOOL_RESULT\\n'; curl -fsS http://${destination}/resource?step=${step}; printf 'FILE_EVIDENCE' > /tmp/agenttomo-chat-evidence.txt; cat /tmp/agenttomo-chat-evidence.txt", login: false, max_output_tokens: 2000}); text(result.output);`,
  };
  if (step >= 7) call.input = `const result = await tools.exec_command({cmd: "printf 'AGENTTOMO_CANCEL_TARGET\\n'; sleep 30", login: false, yield_time_ms: 1000}); text(result.output);`;
  const answer = { type:"message", id:`msg_chat_${step}`, role:"assistant", status:"completed", content:[{ type:"output_text", text:"Task complete. TOOL_RESULT and resource evidence were observed.", annotations:[] }] };
  const item = recovering || step % 3 === 0 ? answer : call;
  const base = { id:`resp_chat_${step}`, object:"response", created_at:Math.floor(Date.now()/1000), status:"in_progress", model:"gpt-6-astra", output:[], error:null };
  const added = { ...item, status:"in_progress", ...(item.type === "custom_tool_call" ? {input:""} : {}) };
  const events = [{ type:"response.created", response:base },{ type:"response.output_item.added", output_index:0, item:added }];
  if (item.type === "custom_tool_call") {
    for (const delta of item.input.match(/.{1,17}/gs)) events.push({ type:"response.custom_tool_call_input.delta", output_index:0, item_id:item.id, delta });
    events.push({ type:"response.custom_tool_call_input.done", item_id:item.id, input:item.input });
  } else events.push({ type:"response.output_text.delta", item_id:item.id, delta:item.content[0].text });
  events.push({ type:"response.output_item.done", output_index:0, item });
  const completed = { ...base, status:"completed", output:[item], usage:{input_tokens:1,output_tokens:1,total_tokens:2} };
  responses.push(completed);
  events.push({ type:"response.completed", response:completed });
  // Intentionally omit Content-Type, matching the observed Codex endpoint.
  res.writeHead(200);
  for (const [sequence_number,event] of events.entries()) res.write(`data: ${JSON.stringify({...event, sequence_number})}\n\n`);
  res.end();
});
fixture.listen(0, gateway); await once(fixture, "listening");
// This TEST-NET destination has no origin. Only the box-owned CONNECT proxy can
// reach the fixture; direct forwarding or application proxy injection must fail.
const destination = `192.0.2.44:${fixture.address().port}`;
const tunnels = [], proxySockets = new Set();
const upstreamProxy = createServer((_req, res) => { res.writeHead(405); res.end(); });
upstreamProxy.on("connect", (req, client, head) => {
  tunnels.push(req.url);
  if (req.url !== destination) { client.end("HTTP/1.1 502 Bad Gateway\r\n\r\n"); return; }
  const upstream = createConnection({host:gateway,port:fixture.address().port}, () => {
    client.write("HTTP/1.1 200 Connection Established\r\n\r\n");
    if (head.length) upstream.write(head);
    client.pipe(upstream); upstream.pipe(client);
  });
  for (const socket of [client, upstream]) {
    proxySockets.add(socket); socket.on("close", () => proxySockets.delete(socket));
  }
  client.on("error", () => upstream.destroy()); upstream.on("error", () => client.destroy());
});
upstreamProxy.listen(0,"127.0.0.1"); await once(upstreamProxy,"listening");
const home = `${directory}/codex-home`; await mkdir(home);
const hostAuth = JSON.stringify({OPENAI_API_KEY:hostCredential});
await writeFile(`${home}/auth.json`, hostAuth);
await writeFile(`${home}/config.toml`, `model = "gpt-6-astra"\nmodel_provider = "local_fixture"\n[model_providers.local_fixture]\nname = "Local fixture"\nbase_url = "http://${destination}/v1"\nwire_api = "responses"\nsupports_websockets = false\nrequires_openai_auth = true\nrequest_max_retries = 0\nstream_max_retries = 0\n[features]\nplugins = false\n`);
const originalHome = Object.fromEntries(await Promise.all((await readdir(home)).map(async name => [name,await readFile(`${home}/${name}`,"utf8")])));
const server = spawn(`${repo}/target/debug/agenttomo-box`, ["playground","--listen","127.0.0.1:0","--image","agenttomo-playground:24.04","--proxy",`http://127.0.0.1:${upstreamProxy.address().port}`,"--output-dir",`${directory}/sessions`, ...(process.env.AGENTTOMO_TEST_ROOT_OVERLAY === "1" ? ["--overlay-root"] : [])], {
  env:{...process.env,CODEX_HOME:home,HTTP_PROXY:"http://127.0.0.1:1",HTTPS_PROXY:"http://127.0.0.1:1",ALL_PROXY:"http://127.0.0.1:1",http_proxy:"",https_proxy:"",all_proxy:"",NO_PROXY:"*"}, stdio:["ignore","pipe","pipe"], detached:true,
});
let output="", errors="", terminalOutput="", browser, page, session, monitor;
server.stdout.on("data", data => output+=data); server.stderr.on("data", data => errors+=data);
const delay = ms => new Promise(resolve => setTimeout(resolve,ms));
async function until(predicate, timeout=45000) { const deadline=Date.now()+timeout; while (!await predicate()) { assert.ok(Date.now()<deadline, `Timed out. ${errors}`); await delay(100); } }
try {
  await until(() => output.includes("?token="));
  browser = await chromium.connectOverCDP(process.env.AGENTTOMO_CDP || "http://127.0.0.1:19222");
  page = await browser.newPage({viewport:{width:1512,height:1000}});
  await page.addInitScript(() => {
    const NativeWebSocket = window.WebSocket;
    window.WebSocket = class extends NativeWebSocket {
      constructor(...args) { super(...args); if (new URL(args[0]).pathname === "/ws") window.testOwnerSocket = this; }
    };
  });
  const pageErrors=[]; page.on("pageerror", error => pageErrors.push(String(error)));
  page.on("websocket", socket => socket.on("framereceived", ({payload}) => { if (typeof payload !== "string") { terminalOutput += Buffer.from(payload).toString(); return; } if (!payload.startsWith("{")) return; const frame=JSON.parse(payload); if (frame.type==="session") session=frame; if (frame.type==="trace") observed.push(frame.event); }));
  await page.goto(output.match(/http:\/\/\S+/)[0]);
  await until(async () => (await page.locator("#status").textContent()) === "Connected");
  assert.equal(session.agent_available,true,"external Codex discovery");
  await page.locator("#terminal").click();
  await page.keyboard.type('test -z "$HTTP_PROXY$HTTPS_PROXY$ALL_PROXY$http_proxy$https_proxy$all_proxy" && test "$CODEX_HOME" = /records/codex && codex login status && printf "TERMINAL_AUTH_%s\\n" OK');
  await page.keyboard.press("Enter");
  await until(() => terminalOutput.includes("TERMINAL_AUTH_OK"),15000);
  await page.locator("#terminal").click(); await page.keyboard.type("cat"); await page.keyboard.press("Enter");
  await until(()=>observed.some(event=>event.kind==="process.exec" && event.data.argv?.[0]?.split("/").at(-1)==="cat"));
  await page.locator("[data-workspace=chat]").click();
  await page.locator('[aria-label="Task for Codex"]').fill("Run the local evidence task.");
  await page.locator(".agent-send").click();
  await until(async () => await page.locator(".agent-progress").count() === 1);
  assert.equal(await page.locator('[aria-label="Task for Codex"]').isEnabled(),true,"the next prompt can be drafted while Codex runs");
  await until(async () => /Waiting for model/.test(await page.locator(".agent-progress").textContent()));
  await until(() => observed.some(event => event.kind==="agent.task.finished"),90000);
  const ended=observed.find(event=>event.kind==="agent.task.finished");
  assert.equal(ended.data.exit_code,0,JSON.stringify(ended.data));
  assert.deepEqual(await readdir(`${directory}/sessions`).catch(error => { if (error.code === "ENOENT") return []; throw error; }), [], "no runtime files may be staged on the host");
  assert.equal(docker("exec",session.container,"stat","-f","-c","%T","/records/codex").trim(),"tmpfs");
  assert.ok(docker("exec",session.container,"ls",`/records/${session.id}/run`).includes("agent-payloads.bin"),"evidence stays available in the live sandbox");
  assert.ok(docker("exec",session.container,"ls","/records/codex").includes("sessions"),"native Codex continuation lives inside the sandbox");
  assert.equal(requests.length,3);
  assert.ok(modelAuthorization.every(value => value === `Bearer ${hostCredential}`), "host gateway authenticates model requests upstream");
  assert.ok(!JSON.stringify(observed).includes(hostCredential), "the host credential cannot appear in captured evidence");
  assert.ok(!docker("exec",session.container,"cat","/records/codex/auth.json").includes(hostCredential), "sandbox authentication uses placeholders");
  assert.ok(!docker("exec",session.container,"cat","/records/codex/agenttomo-environment.json").includes(hostCredential));
  assert.equal(await readFile(`${home}/auth.json`,"utf8"),hostAuth,"host login stays read-only");
  await assertSandboxSecretsAbsent(session.container, [hostCredential]);
  const taskConnections = observed.filter(event=>event.kind==="network.upstream" && event.data.destination===destination && event.data.status==="connected");
  assert.ok(taskConnections.length >= 2,"model and tool open separate TCP flows");
  assert.equal(tunnels.filter(authority=>authority===destination).length,taskConnections.length,"every model/tool flow uses one box-owned CONNECT tunnel");
  // Codex may also attempt background connections. The fixture rejects those;
  // they must not be mistaken for additional model/tool destinations.
  for (const event of observed.filter(event=>event.kind==="network.upstream" && event.data.destination!==destination)) {
    assert.equal(event.data.status,"failed");
    assert.match(event.data.error,/CONNECT rejected: 502/);
    assert.ok(tunnels.includes(event.data.domain ? `${event.data.domain}:${event.data.destination.split(":").at(-1)}` : event.data.destination));
  }
  assert.ok(!observed.some(event=>event.kind==="network.http.request" && event.data.method==="CONNECT"),"transport CONNECT must not become a separate application event");
  assert.ok(observed.filter(event=>event.kind==="network.http.request").every(event=>event.data.destination===destination),"traffic retains the application's original peer");
  const tool=observed.find(event=>event.kind==="agent.tool.call");
  const result=observed.find(event=>event.kind==="agent.tool.result");
  assert.ok(tool && result, "model traffic must contain complete tool evidence");
  assert.equal(tool.data.agenttomo_toolcall_id,result.data.agenttomo_toolcall_id);
  // Codex's /proc/self/exe setup helper carries the same command in its argv;
  // assert the binding on the shell that actually executes the tool instead.
  const command=observed.find(event=>event.kind==="process.exec" && event.data.argv?.some(arg=>arg.includes("AGENTTOMO_TOOL_RESULT")) && event.data.argv?.[0]?.split("/").at(-1)==="bash");
  assert.ok(command, "shell execution must be traced");
  assert.equal(command.data.agent.agenttomo_toolcall_id,tool.data.agenttomo_toolcall_id,"process binding");
  const resource=observed.find(event=>event.kind==="network.http.request" && event.data.url.endsWith("/resource?step=1"));
  assert.equal(resource?.data.agent?.agenttomo_toolcall_id,tool.data.agenttomo_toolcall_id,"HTTP resource binding");
  assert.ok(observed.some(event=>event.kind==="file.open" && event.data.path==="/tmp/agenttomo-chat-evidence.txt" && event.data.agent?.agenttomo_toolcall_id===tool.data.agenttomo_toolcall_id));
  for (const call of observed.filter(event => event.kind === "agent.tool.call")) {
    const step = call.data.call_id.split("_").at(-1);
    const request = observed.find(event => event.kind === "network.http.request" && event.data.url.endsWith(`/resource?step=${step}`));
    assert.equal(request?.data.agent?.agenttomo_toolcall_id,call.data.agenttomo_toolcall_id,`resource attribution for ${call.data.call_id}`);
  }
  const preview = observed.find(event => event.kind === "file.open" && event.data.path === "/tmp/agenttomo-chat-evidence.txt" && event.data.preview?.captured_bytes > 0).data.preview;
  const previewURL = new URL(`/api/sessions/${session.id}/file-preview`, page.url());
  previewURL.search = new URLSearchParams({token:new URL(page.url()).searchParams.get("token"),offset:preview.offset,length:preview.captured_bytes});
  const previewResponse = await page.request.get(previewURL.href);
  assert.equal(previewResponse.status(),200);
  assert.equal(await previewResponse.text(),"FILE_EVIDENCE","file snapshots read from the container through the persistent channel");
  const execution = page.locator(".agent-work").first();
  assert.equal(await execution.evaluate(node=>node.open),false,"completed tasks collapse intermediate steps");
  assert.equal(await page.locator('.agent-final-answers .agent-entry[data-entry-type="message"]').count(),1,"unlabelled final response remains visible");
  await execution.locator(':scope > summary').click();
  await page.locator(".agent-tool-card > summary").first().click();
  await page.locator(".agent-tool-inspect").first().click();
  await until(async () => (await page.locator("#agent-detail").textContent()).includes("AGENTTOMO_TOOL_RESULT"));
  const sections=page.locator("#agent-detail .agent-content-section"); assert.equal(await sections.count(),2);
  assert.ok((await sections.nth(0).textContent()).includes("tools.exec_command"));
  assert.ok((await sections.nth(1).textContent()).includes("RESOURCE_EVIDENCE"));
  await sections.nth(0).locator("summary").click(); assert.equal(await sections.nth(0).getAttribute("open"),null);
  await sections.nth(0).locator("summary").click();
  const modelCards = page.locator('#agent-trajectory .agent-card[data-category="model"]');
  assert.deepEqual(await modelCards.locator('strong').allTextContents(),["Model call 1","Model call 2","Model call 3"]);
  await modelCards.first().locator('.agent-node-inspect').click();
  await until(async () => (await sections.nth(1).textContent()).includes('resp_chat_1'));
  assert.deepEqual(JSON.parse(await sections.nth(0).locator('.agent-content').textContent()),requests[0],"full model request is loaded from captured traffic");
  assert.deepEqual(JSON.parse(await sections.nth(1).locator('.agent-content').textContent()),responses[0],"full model response is loaded from captured traffic");
  if(process.env.AGENTTOMO_TEST_ARTIFACTS) { await mkdir(process.env.AGENTTOMO_TEST_ARTIFACTS,{recursive:true}); await page.screenshot({path:`${process.env.AGENTTOMO_TEST_ARTIFACTS}/model-response.png`}); }
  const href=await page.locator("#open-monitor").getAttribute("href"); monitor=await browser.newPage(); await monitor.goto(href);
  await monitor.locator("[data-observation=agent]").click();
  await until(async()=> (await monitor.locator("#agent-trajectory").textContent()).includes("Run the local evidence task."));
  assert.equal(docker("ps","--filter",`name=${session.container}`,"--format","{{.Names}}").trim(),session.container);
  await page.locator('[aria-label="Task for Codex"]').fill("Run a second evidence task in this conversation.");
  await page.locator(".agent-send").click();
  await until(() => observed.filter(event=>event.kind==="agent.task.finished").length===2,90000);
  assert.equal(observed.filter(event=>event.kind==="agent.task.finished").at(-1).data.exit_code,0);
  assert.equal(requests.length,6,"resumed conversation must execute another tool round");
  docker("exec",session.container,"sh","-c","printf private > /records/codex/isolation-check");
  assert.deepEqual(Object.fromEntries(await Promise.all((await readdir(home)).map(async name => [name,await readFile(`${home}/${name}`,"utf8")]))), originalHome, "sandbox Codex must not modify host auth, config or session history");
  assert.equal(observed.filter(event=>event.kind==="agent.tool.result").length,4,"prior tool results must not be displayed again");
  assert.equal(new Set(observed.filter(event=>event.kind==="agent.tool.call").map(event=>event.data.agenttomo_toolcall_id)).size,4);
  await until(async()=> (await monitor.locator("#agent-trajectory").textContent()).includes("Run a second evidence task"));
  await page.locator('[aria-label="Task for Codex"]').fill("Run a task that will be cancelled.");
  await page.locator(".agent-send").click();
  await until(()=>observed.some(event=>event.kind==="process.exec" && event.data.argv?.[0]?.split("/").at(-1)==="sleep" && event.data.argv[1]==="30"));
  await page.locator(".agent-stop").click();
  await until(()=>observed.filter(event=>event.kind==="agent.task.finished").length===3);
  assert.equal(observed.filter(event=>event.kind==="agent.task.finished").at(-1).data.cancelled,true,JSON.stringify(observed.filter(event=>event.kind==="agent.task.finished").at(-1).data));
  const cancelledTask = observed.filter(event=>event.kind==="agent.task.finished").at(-1).data.task_id;
  await until(()=>observed.filter(event=>event.kind==="process.exec" && event.data.agent?.task_id===cancelledTask)
    .every(exec=>observed.some(event=>event.kind==="process.exit" && event.data.process_instance_id===exec.data.process_instance_id)));
  assert.equal(await page.locator("#status").textContent(),"Connected","cancelling a task must preserve the terminal sandbox");
  rejectResponses = true;
  await page.locator('[aria-label="Task for Codex"]').fill("Trigger an authentication error.");
  await page.locator(".agent-send").click();
  await until(() => observed.filter(event=>event.kind==="agent.task.finished").length===4,45000);
  assert.notEqual(observed.filter(event=>event.kind==="agent.task.finished").at(-1).data.exit_code,0);
  await until(async () => (await page.locator(".agent-error").allTextContents()).some(text => text.includes("401")));
  assert.equal(await page.locator(".agent-send").isEnabled(),true,"authentication failure must release the composer");
  holdResponses = true; rejectResponses = false;
  const beforeDisconnection = requests.length;
  await page.locator('[aria-label="Task for Codex"]').fill("Wait for a response before disconnecting.");
  await page.locator(".agent-send").click();
  await until(()=>requests.length > beforeDisconnection);
  const oldSession = session;
  await page.locator('[aria-label="Task for Codex"]').fill("Keep this draft through sandbox recovery.");
  await page.evaluate(()=>window.testOwnerSocket.close());
  await until(async()=>(await page.locator('.agent-chat-status').textContent()).includes('Sandbox disconnected'));
  assert.equal(await page.locator('[aria-label="Task for Codex"]').isEnabled(),true);
  assert.equal(await page.locator('.agent-send').isDisabled(),true);
  assert.equal(await page.locator('.agent-restart').isEnabled(),true);
  await until(()=>!docker("ps","-a","--filter",`name=${oldSession.container}`,"--format","{{.Names}}").trim());
  assert.deepEqual(await readdir(`${directory}/sessions`).catch(error => { if (error.code === "ENOENT") return []; throw error; }), []);
  holdResponses = false; recovering = true;
  await page.locator('.agent-restart').click();
  await until(()=>session.id!==oldSession.id);
  await until(async()=>(await page.locator('#status').textContent())==='Connected');
  assert.equal(await page.locator('#agent-chat').isVisible(),true);
  assert.equal(await page.locator('[aria-label="Task for Codex"]').inputValue(),"Keep this draft through sandbox recovery.");
  const beforeRecovery = observed.filter(event=>event.kind==='agent.task.finished').length;
  await page.locator('.agent-send').click();
  await until(()=>observed.filter(event=>event.kind==='agent.task.finished').length>beforeRecovery,90000);
  assert.equal(observed.filter(event=>event.kind==='agent.task.finished').at(-1).data.exit_code,0,"new sandbox accepts and completes the retained draft");
  assert.deepEqual(pageErrors,[]);
  assert.ok(modelAuthorization.every(value => value === `Bearer ${hostCredential}`));
  assert.ok(!JSON.stringify(observed).includes(hostCredential));
  if(process.env.AGENTTOMO_TEST_ARTIFACTS) { await mkdir(process.env.AGENTTOMO_TEST_ARTIFACTS,{recursive:true}); await page.screenshot({path:`${process.env.AGENTTOMO_TEST_ARTIFACTS}/agent-chat.png`}); await writeFile(`${process.env.AGENTTOMO_TEST_ARTIFACTS}/agent-events.json`,JSON.stringify(observed)); }
  console.log(JSON.stringify({passed:true,modelRequests:requests.length,proxyTunnels:tunnels.length,toolId:tool.data.agenttomo_toolcall_id,processPid:command.data.pid,events:observed.length,cancelled:true,disconnectionRecovery:true}));
} catch (error) {
  if(process.env.AGENTTOMO_TEST_ARTIFACTS) {
    await mkdir(process.env.AGENTTOMO_TEST_ARTIFACTS,{recursive:true});
    await writeFile(`${process.env.AGENTTOMO_TEST_ARTIFACTS}/failure-events.json`,JSON.stringify(observed));
    await page?.screenshot({path:`${process.env.AGENTTOMO_TEST_ARTIFACTS}/failure.png`}).catch(()=>{});
  }
  throw error;
} finally {
  await monitor?.close(); await page?.close(); await browser?.close();
  try{process.kill(-server.pid,"SIGTERM");}catch{}
  await Promise.race([once(server,"exit"),delay(10000)]);
  if(session && docker("ps","-a","--filter",`name=${session.container}`,"--format","{{.Names}}").trim()) { try{docker("rm","--force",session.container);}catch{} }
  fixture.closeAllConnections(); fixture.close();
  for (const socket of proxySockets) socket.destroy(); upstreamProxy.close();
  await rm(directory,{recursive:true,force:true});
}
