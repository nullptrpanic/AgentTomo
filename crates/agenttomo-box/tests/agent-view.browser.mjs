// Chat DOM regression. The caller owns and cleans up the Chromium CDP endpoint.
import assert from "node:assert/strict";
import { once } from "node:events";
import { readFile, mkdir } from "node:fs/promises";
import { createServer } from "node:http";

const { chromium } = await import(process.env.AGENTTOMO_PLAYWRIGHT_MODULE || "playwright");
const assets = new URL("../src/playground/assets/", import.meta.url);
const index = await readFile(new URL("index.html", assets), "utf8");
const chat = index.match(/<section id="agent-chat"[\s\S]*?<\/section>/)[0].replace(" hidden", "");
const server = createServer(async (request, response) => {
  const path = new URL(request.url, "http://localhost").pathname.slice(1);
  if (!path) {
    response.setHeader("content-type", "text/html");
    response.end(`<!doctype html><meta charset="utf-8"><link rel="stylesheet" href="/style.css"><link rel="stylesheet" href="/agent.css"><style>main{display:flex;margin:0;height:820px;padding:24px;gap:24px;border:0}#agent-chat{width:720px;max-width:720px;border:1px solid #314b5a}#detail{width:430px;overflow:auto}</style><main>${chat}<section id="agent-trajectory" hidden></section><aside id="detail" hidden></aside></main>`);
    return;
  }
  if (!/^(vendor\/)?[a-z-]+\.(mjs|css)$/.test(path)) { response.writeHead(404).end(); return; }
  try {
    response.setHeader("content-type", path.endsWith(".css") ? "text/css" : "text/javascript");
    response.end(await readFile(new URL(path, assets)));
  } catch { response.writeHead(404).end(); }
});
server.listen(0, "127.0.0.1");
await once(server, "listening");
let browser, page;
try {
  browser = await chromium.connectOverCDP(process.env.AGENTTOMO_CDP || "http://127.0.0.1:19222");
  page = await browser.newPage({viewport:{width:1280,height:900}});
  const errors = [];
  page.on("pageerror", error => errors.push(String(error)));
  await page.goto(`http://127.0.0.1:${server.address().port}/`);
  await page.evaluate(async () => {
    const { AgentView } = await import("/agent-view.mjs");
    const root = document.querySelector("#agent-chat");
    window.view = new AgentView({root:document.querySelector("#agent-trajectory"),chat:root,detail:document.querySelector("#detail"),
      load:async payload => ({0:{model:"test-model",generate:false},1:{cmd:"curl https://example.test/",timeout_ms:1000},2:"Complete tool output",3:{id:"response",status:"completed",output:[{type:"message",content:[{type:"output_text",text:"Full model answer"}]}]}})[payload.offset],
      onSelect:() => {},onRaw:() => {},reveal:() => {},filter:{allows:() => true}});
    window.cancels = 0;
    view.configure({agent_available:true, agent:{id:"fixture",name:"Example Agent",avatar:"EA",authentication_hint:"Sign in to the fixture on the host."}});
    view.setControl({prompt:() => {},cancel:() => { window.cancels++; }});
    view.setReady(true);
    let sequence = 0;
    window.add = (kind,data={}) => view.add({kind,sequence:++sequence,observed_unix_ns:Date.now()*1e6,
      data:{task_id:"task",request_id:1,captured_unix_ns:Date.now()*1e6,...data}});
    add("agent.task.started",{prompt:"Check the endpoint and explain the result."});
    add("agent.model.request",{model:"test-model",generate:false,started_unix_ns:10,payload:{offset:0,status:"complete"}});
    add("agent.message.delta",{item_id:"before",delta:"I will check the endpoint.",started_unix_ns:20});
    add("agent.tool.started",{agenttomo_toolcall_id:"tool",name:"shell",started_unix_ns:30});
    add("agent.tool.call",{agenttomo_toolcall_id:"tool",preview:"curl https://example.test/",payload:{offset:1,status:"complete"}});
    add("agent.message.delta",{item_id:"after",delta:"The endpoint returned a response.",started_unix_ns:40});
  });
  await page.locator(".agent-tool-card").waitFor({state:"attached",timeout:5000});
  assert.equal(await page.locator(".agent-reply-heading strong").textContent(),"Example Agent","assistant name comes from session metadata");
  assert.equal(await page.locator(".agent-reply-heading .agent-avatar").textContent(),"EA");
  assert.equal(await page.locator("textarea").getAttribute("aria-label"),"Task for Example Agent");
  assert.equal(await page.locator(".agent-reply").count(),1,"one assistant reply owns both the answer and execution");
  assert.equal(await page.locator('.agent-timeline > .agent-response').count(),1,"one model response owns its text and tool calls");
  assert.deepEqual(await page.locator('.agent-response .agent-entry').evaluateAll(nodes => nodes.map(node => node.dataset.entryId)),["before","tool","after"]);
  assert.equal(await page.locator('.agent-final-answers .agent-entry').count(),0,"running commentary stays with tools from the same response");
  assert.equal(await page.locator('.agent-user .agent-message-heading time').count(),0);
  assert.equal(await page.locator('.agent-user .agent-message-actions time').count(),1,"timestamps live in the bottom action toolbar");
  assert.equal(await page.locator('.agent-user .agent-message-heading').evaluate(node=>getComputedStyle(node).justifyContent),"flex-start");
  assert.equal(await page.locator('.agent-user .agent-message-heading > .agent-avatar svg').count(),1,"You has a matching avatar before its label");
  assert.equal(await page.locator('.agent-user .agent-message-heading').evaluate(node=>node.lastElementChild.textContent),"You");
  assert.equal(await page.locator('.agent-work').evaluate(node=>node.open),true,"execution starts expanded while running");
  assert.match(await page.locator('.agent-work-title').textContent(),/Task in progress/);
  assert.equal(await page.locator('.agent-reply > .agent-work .agent-spinner').count(),1);
  assert.equal(await page.locator(".agent-tool-card").evaluate(node => node.open),false);
  assert.match(await page.locator(".agent-tool-card > summary").textContent(),/shell.*curl https:\/\/example.test\//s);
  await page.locator(".agent-tool-card > summary").click();
  await page.waitForFunction(() => document.querySelector(".agent-tool-card").textContent.includes('"timeout_ms": 1000'));
  await page.evaluate(() => {
    window.card = document.querySelector(".agent-tool-card");
    window.answer = document.querySelector('[data-entry-id="before"] .agent-message-text');
    const range = document.createRange();
    range.setStart(answer.firstChild,0); range.setEnd(answer.firstChild,6);
    document.getSelection().removeAllRanges(); document.getSelection().addRange(range);
    window.focused = card.querySelector("summary"); focused.focus();
    add("agent.message.delta",{item_id:"before",delta:" Next I will explain it."});
    add("agent.tool.result",{agenttomo_toolcall_id:"tool",preview:"Complete tool output",payload:{offset:2,status:"complete"}});
  });
  await page.waitForFunction(() => document.querySelector(".agent-tool-card").textContent.includes("Complete tool output"));
  assert.deepEqual(await page.evaluate(() => ({sameCard:card===document.querySelector(".agent-tool-card"),sameAnswer:answer===document.querySelector('[data-entry-id="before"] .agent-message-text'),open:card.open,selected:document.getSelection().toString(),focused:document.activeElement===focused})),
    {sameCard:true,sameAnswer:true,open:true,selected:"I will",focused:true});
  await page.locator(".agent-tool-inspect").click();
  assert.ok(await page.locator(".agent-tool-inspect").evaluate(node => node.parentElement.getBoundingClientRect().right-node.getBoundingClientRect().right<20),"tool inspection control aligns with the card's right edge");
  await page.waitForFunction(() => document.querySelector("#detail").textContent.includes("Complete tool output"));
  await page.evaluate(() => {
    const agent={agent_instance_id:"agent",task_id:"task",agenttomo_toolcall_id:"tool",association:"inferred_active_call"};
    window.processAgent=agent;
    add("process.exec",{pid:40,process_instance_id:9,argv:["curl"],agent});
    add("process.exec",{pid:39,process_instance_id:8,argv:["codex"],started_unix_ns:5,agent:{task_id:"task",association:"agent_runtime"}});
    view.root.hidden=false; view.render();
    add("file.open",{pid:40,process_instance_id:9,path:"/tmp/first",agent});
    view.render();
  });
  for (const id of ["task-task","tool","pid-9"]) assert.equal(await page.locator(`[data-agent-node="${id}"] > .agent-card`).evaluate(node=>node.classList.contains("has-activity")),true,`${id} receives descendant activity`);
  // A live refresh between pointer-down/up must not discard the pressed node.
  const taskToggle = page.locator('[data-agent-node="task-task"] > .agent-card > .agent-fold');
  await page.evaluate(() => document.querySelector('#detail').hidden=true);
  await taskToggle.scrollIntoViewIfNeeded();
  const hit = await taskToggle.boundingBox();
  await page.mouse.move(hit.x+hit.width/2,hit.y+hit.height/2);
  await page.mouse.down();
  await page.evaluate(() => { add("file.open",{pid:40,process_instance_id:9,path:"/tmp/during-click",agent:processAgent}); view.render(); });
  await page.mouse.up();
  assert.equal(await taskToggle.getAttribute("aria-expanded"),"false","a single click survives a concurrent event refresh");
  await taskToggle.press("Enter");
  assert.equal(await taskToggle.getAttribute("aria-expanded"),"true","keyboard disclosure stays usable after refresh");
  await page.locator('[data-agent-node="task-task"] > .agent-card > .agent-fold').click();
  await page.evaluate(() => { add("file.open",{pid:40,process_instance_id:9,path:"/tmp/second",agent:processAgent}); view.render(); });
  assert.equal(await page.locator('[data-agent-node="task-task"] > .agent-card > .agent-fold').getAttribute("aria-expanded"),"false");
  assert.equal(await page.locator('[data-agent-node="task-task"] > .agent-card').evaluate(node=>node.classList.contains("has-activity")),true);
  await page.waitForTimeout(900);
  await page.locator('[data-agent-node="task-task"] > .agent-card > .agent-fold').click();
  assert.equal(await page.locator('[data-agent-node="tool"] > .agent-card').evaluate(node=>node.classList.contains("has-activity")),false,"expanding an old branch does not replay activity");
  const round = page.locator('[data-agent-node="round-2"]');
  assert.equal(await round.locator(':scope > .agent-card strong').textContent(),"Model call 1");
  assert.deepEqual(await round.locator(':scope > .agent-children > .agent-branch').evaluateAll(nodes=>nodes.map(node=>node.dataset.agentNode)),["message-before","tool","message-after"],"messages and tools share chronological order");
  assert.deepEqual(await page.locator('[data-agent-node="task-task"] > .agent-children > .agent-branch').evaluateAll(nodes=>nodes.map(node=>node.dataset.agentNode)),["pid-8","round-2"],"runtime processes are interleaved with model calls by start time");
  await round.locator(':scope > .agent-card > .agent-node-inspect').click();
  assert.equal(await page.locator('#detail .agent-content-section').count(),2);
  await page.locator('#detail .agent-content-section > summary').first().click();
  await page.evaluate(()=>add("agent.model.complete",{status:"completed",payload:{offset:3,status:"complete"}}));
  await page.waitForFunction(()=>document.querySelectorAll('#detail .agent-content')[1]?.textContent.includes('Full model answer'));
  assert.equal(await page.locator('#detail .agent-content-section').first().evaluate(node=>node.open),false,"arriving responses preserve the request disclosure");
  await page.evaluate(() => { view.root.hidden=true; });
  if(process.env.AGENTTOMO_TEST_ARTIFACTS) {
    await mkdir(process.env.AGENTTOMO_TEST_ARTIFACTS,{recursive:true});
    await page.screenshot({path:`${process.env.AGENTTOMO_TEST_ARTIFACTS}/tool-card.png`});
    await page.locator('.agent-tool-card > summary').click();
    await page.evaluate(()=>document.querySelector('#detail').hidden=true);
    await page.screenshot({path:`${process.env.AGENTTOMO_TEST_ARTIFACTS}/approved-running.png`});
  }
  await page.evaluate(() => {
    for(let id=0;id<35;id++) add("agent.message.delta",{item_id:`history-${id}`,delta:`Recorded progress ${id}\nMore information about the task.`});
  });
  await page.waitForFunction(() => document.querySelectorAll(".agent-entry").length===38);
  await page.evaluate(() => {
    const history=document.querySelector(".agent-history");
    history.scrollTop=0; history.dispatchEvent(new Event("scroll"));
    add("agent.message.delta",{item_id:"history-34",delta:"\nMore streamed content."});
  });
  await page.waitForFunction(() => !document.querySelector(".agent-latest").hidden);
  assert.ok(await page.locator(".agent-history").evaluate(node => node.scrollTop<10));
  await page.locator(".agent-latest").click();
  assert.ok(await page.locator(".agent-history").evaluate(node => node.scrollHeight-node.scrollTop-node.clientHeight<5));
  await page.locator(".agent-stop").click();
  assert.equal(await page.locator(".agent-stop").isDisabled(),true);
  assert.match(await page.locator(".agent-chat-status").textContent(),/Stopping/);
  assert.equal(await page.evaluate(() => cancels),1);
  await page.locator("textarea").fill("Keep this newer draft.");
  await page.evaluate(() => add("agent.task.finished",{exit_code:1,error:"Connection timed out"}));
  await page.waitForFunction(()=>document.querySelector('[data-task-id="task"] .agent-work').dataset.state==="failed");
  assert.equal(await page.locator('[data-task-id="task"] .agent-work-title').textContent(),"Task failed","the terminal outcome replaces a pending stop");
  assert.equal(await page.locator('[data-task-id="task"] .agent-state-dot.agent-spinner').count(),0);
  assert.equal(await page.locator("textarea").inputValue(),"Keep this newer draft.");
  await page.locator("textarea").fill("");
  await page.evaluate(() => {
    add("agent.task.started",{task_id:"failed",prompt:"Recover this prompt."});
    add("agent.task.finished",{task_id:"failed",exit_code:1,error:"HTTP 401 Unauthorized"});
  });
  assert.equal(await page.locator("textarea").inputValue(),"Recover this prompt.");
  await page.waitForFunction(()=>document.querySelector('[data-task-id="failed"] .agent-error > p')?.textContent==="Sign in to the fixture on the host.");
  const latestError = page.locator('.agent-chat-task[data-task-id="failed"] .agent-error-details');
  await latestError.waitFor({state:"visible"});
  await latestError.locator("summary").click();
  await page.evaluate(async () => {
    add("agent.task.diagnostic",{task_id:"failed",error:"HTTP 401 Unauthorized"});
    await new Promise(requestAnimationFrame);
  });
  assert.equal(await latestError.evaluate(node => node.open),true);
  await page.evaluate(() => add("agent.tool.call",{task_id:"failed",agenttomo_toolcall_id:"unavailable",name:"read_file",payload:{status:"unavailable",error:"Semantic payload budget exhausted"}}));
  await page.locator('[data-tool-id="unavailable"] > summary').click();
  await page.waitForFunction(() => document.querySelector('[data-tool-id="unavailable"]').textContent.includes("Semantic payload budget exhausted"));
  assert.equal(await page.locator('[data-tool-id="unavailable"] .agent-content-section button').first().isDisabled(),true);
  const markdown = "**Finished** checking `example.test`.\n\n> The endpoint is healthy.\n\n- Response received\n- Evidence retained\n\n[Source](https://example.test/)\n\n<script>window.unsafeMarkdown=true</script><img src=x onerror=\"window.unsafeMarkdown=true\">[bad](javascript:alert(1))";
  await page.evaluate(markdown => {
    add("agent.task.started",{task_id:"complete",prompt:"Check **this** endpoint."});
    add("agent.model.request",{task_id:"complete",request_id:20,model:"test-model"});
    add("agent.message",{task_id:"complete",request_id:20,item_id:"progress",phase:"commentary",preview:"I am checking the endpoint."});
    for (const [index,id] of ["request", "verify"].entries()) {
      if (index) add("agent.model.request",{task_id:"complete",request_id:20+index,model:"test-model"});
      add("agent.tool.call",{task_id:"complete",request_id:20+index,agenttomo_toolcall_id:id,name:"shell",preview:id === "request" ? "curl https://example.test/" : "Verify the captured response",payload:{offset:1,status:"complete"}});
      add("agent.tool.result",{task_id:"complete",request_id:20+index,agenttomo_toolcall_id:id,preview:"Complete tool output",payload:{offset:2,status:"complete"}});
    }
    add("agent.model.request",{task_id:"complete",request_id:22,model:"test-model",payload:{offset:0,status:"complete"}});
    add("agent.message.delta",{task_id:"complete",request_id:22,item_id:"final",delta:markdown});
    add("agent.message",{task_id:"complete",request_id:22,item_id:"final",phase:"final_answer",preview:markdown});
    add("agent.model.complete",{task_id:"complete",request_id:22,status:"completed",payload:{offset:3,status:"complete"}});
    add("agent.task.finished",{task_id:"complete",exit_code:0});
  },markdown);
  const completed = page.locator('[data-task-id="complete"]');
  const work = completed.locator('.agent-work');
  await work.waitFor({state:"visible"});
  assert.equal(await work.evaluate(node=>node.open),false,"completed execution folds automatically");
  assert.match(await work.locator(':scope > summary').textContent(),/Task completed/);
  assert.equal(await completed.locator(':scope > .agent-reply').count(),1);
  assert.equal(await completed.locator(':scope > .agent-work').count(),0,"execution must not form a separate card");
  assert.equal(await work.evaluate(node=>node===node.parentElement.lastElementChild),true,"execution is the reply card footer");
  assert.ok(await completed.evaluate(node=>node.querySelector('.agent-final-answers').getBoundingClientRect().bottom<=node.querySelector('.agent-work').getBoundingClientRect().top),"the final answer remains above the collapsed footer");
  assert.equal(await completed.locator('.agent-final-answers .agent-entry[data-entry-type="message"]').count(),1);
  assert.equal(await completed.locator('.agent-final-answers strong').textContent(),"Finished");
  assert.equal(await completed.locator('.agent-final-answers blockquote').count(),1);
  assert.equal(await completed.locator('.agent-final-answers li').count(),2);
  assert.equal(await completed.locator('script,img,[onerror],a[href^="javascript:"]').count(),0,"Markdown cannot add active content");
  assert.equal(await page.evaluate(()=>window.unsafeMarkdown),undefined);
  assert.equal(await completed.locator('.agent-user .agent-message-text').textContent(),"Check **this** endpoint.");
  assert.ok(await completed.locator('time').first().getAttribute('datetime'));
  await work.locator(':scope > summary').click();
  await page.evaluate(()=>add("agent.message",{task_id:"complete",request_id:20,item_id:"progress",phase:"commentary",preview:"Checked the endpoint."}));
  await page.waitForFunction(()=>document.querySelector('[data-entry-id="progress"]').textContent.includes("Checked the endpoint."));
  assert.equal(await work.evaluate(node=>node.open),true,"later updates preserve manual expansion");
  if(process.env.AGENTTOMO_TEST_ARTIFACTS) {
    await completed.scrollIntoViewIfNeeded();
    await page.screenshot({path:`${process.env.AGENTTOMO_TEST_ARTIFACTS}/approved-completed.png`});
  }
  await work.locator(':scope > summary').click();
  await page.context().grantPermissions(["clipboard-read","clipboard-write"],{origin:`http://127.0.0.1:${server.address().port}`});
  const copy = completed.locator('.agent-final-answers .agent-copy');
  const icon = await copy.innerHTML();
  await copy.click();
  await page.waitForFunction(()=>document.querySelector('[data-task-id="complete"] .agent-final-answers .agent-copy').title==="Copied");
  assert.notEqual(await copy.innerHTML(),icon);
  assert.equal(await page.evaluate(()=>navigator.clipboard.readText()),markdown,"answer copies the original Markdown");
  await page.waitForFunction(()=>document.querySelector('[data-task-id="complete"] .agent-final-answers .agent-copy').title==="Copy answer");
  assert.equal(await copy.innerHTML(),icon,"copy feedback expires");
  await copy.click();
  await page.waitForFunction(()=>document.querySelector('[data-task-id="complete"] .agent-final-answers .agent-copy').title==="Copied");
  await page.mouse.move(0,0);
  assert.equal(await copy.innerHTML(),icon,"leaving the button resets feedback");
  await page.evaluate(()=>view.setReady(false,"Sandbox disconnected."));
  assert.match(await page.locator('.agent-chat-status').textContent(),/Sandbox disconnected/);
  assert.equal(await page.locator('textarea').isEnabled(),true,"draft remains editable after disconnection");
  assert.equal(await page.locator('.agent-send').isDisabled(),true,"cannot send to a removed sandbox");
  assert.deepEqual(await page.locator('.agent-compose footer button').allTextContents(),["Stop","Send ↗"]);
  assert.deepEqual(errors,[]);
  if(process.env.AGENTTOMO_TEST_ARTIFACTS) {
    await mkdir(process.env.AGENTTOMO_TEST_ARTIFACTS,{recursive:true});
    await page.screenshot({path:`${process.env.AGENTTOMO_TEST_ARTIFACTS}/chat-experience.png`});
  }
  await page.evaluate(()=>document.querySelector('#detail').hidden=true);
  await page.setViewportSize({width:480,height:900});
  await completed.scrollIntoViewIfNeeded();
  assert.ok(await page.locator('.agent-history').evaluate(node=>node.scrollWidth<=node.clientWidth+1),"narrow chat wraps without horizontal overflow");
  const bounds=await completed.evaluate(node=>({user:node.querySelector('.agent-user').getBoundingClientRect().right,answer:node.querySelector('.agent-reply').getBoundingClientRect().left,root:node.getBoundingClientRect().left}));
  assert.ok(bounds.user>bounds.answer && bounds.answer===bounds.root,"question and answer retain distinct alignment");
  if(process.env.AGENTTOMO_TEST_ARTIFACTS) await page.screenshot({path:`${process.env.AGENTTOMO_TEST_ARTIFACTS}/chat-narrow.png`});
  await page.evaluate(async () => {
    add("agent.task.started",{task_id:"order",prompt:"Keep execution chronological."});
    add("agent.message.delta",{task_id:"order",item_id:"early",delta:"First commentary"});
    add("agent.tool.call",{task_id:"order",agenttomo_toolcall_id:"later-tool",name:"shell"});
    await new Promise(requestAnimationFrame);
    add("agent.message.delta",{task_id:"order",item_id:"latest",delta:"Latest commentary"});
  });
  await page.waitForFunction(()=>document.querySelector('[data-task-id="order"] .agent-response [data-entry-id="latest"]'));
  assert.deepEqual(await page.locator('[data-task-id="order"] .agent-timeline .agent-entry').evaluateAll(nodes=>nodes.map(node=>node.dataset.entryId)),["early","later-tool","latest"],"one response preserves interleaved text and tool order");
  await page.setViewportSize({width:1280,height:900});
  await page.evaluate(() => {
    add("agent.task.started",{task_id:"parallel",prompt:"Keep model responses distinct."});
    add("agent.model.request",{task_id:"parallel",request_id:100,model:"test-model",started_unix_ns:20});
    add("agent.model.request",{task_id:"parallel",request_id:101,model:"test-model",started_unix_ns:10});
    add("agent.message.delta",{task_id:"parallel",request_id:100,item_id:"parallel-a",delta:"Response A"});
    add("agent.message.delta",{task_id:"parallel",request_id:101,item_id:"parallel-b",delta:"Response B"});
    add("agent.tool.call",{task_id:"parallel",request_id:100,agenttomo_toolcall_id:"parallel-tool",name:"shell"});
    add("agent.tool.result",{task_id:"parallel",request_id:101,agenttomo_toolcall_id:"parallel-tool",payload:{offset:2,status:"complete"}});
    view.root.hidden=false; view.render();
  });
  const groups=page.locator('[data-task-id="parallel"] .agent-timeline > .agent-response');
  await groups.first().waitFor();
  assert.deepEqual(await groups.evaluateAll(nodes=>nodes.map(node=>[node.querySelector('.agent-response-title').textContent,...[...node.querySelectorAll('.agent-entry')].map(entry=>entry.dataset.entryId)])),[["Model call 1","parallel-b"],["Model call 2","parallel-a","parallel-tool"]],"membership follows observed IDs while numbers follow start time");
  await page.evaluate(() => {
    add("agent.task.started",{task_id:"orphan",prompt:"Preserve partial evidence."});
    add("agent.tool.result",{task_id:"orphan",agenttomo_toolcall_id:"orphan-tool",payload:{offset:2,status:"complete"}});
  });
  const orphan=page.locator('[data-task-id="orphan"]');
  await orphan.locator('[data-round-id="unassigned"]').waitFor();
  await orphan.locator('.agent-tool-card > summary').click();
  await orphan.locator('.agent-tool-card .agent-content').last().waitFor();
  await page.evaluate(() => {
    window.orphanTool = document.querySelector('[data-tool-id="orphan-tool"]');
    add("agent.tool.call",{task_id:"orphan",request_id:200,agenttomo_toolcall_id:"orphan-tool",name:"shell",payload:{offset:1,status:"complete"}});
  });
  await page.waitForFunction(()=>document.querySelector('[data-tool-id="orphan-tool"]').closest('.agent-response').dataset.roundId!=="unassigned");
  assert.equal(await orphan.locator('.agent-tool-card').count(),1);
  assert.equal(await page.evaluate(()=>orphanTool===document.querySelector('[data-tool-id="orphan-tool"]') && orphanTool.open),true,"late ownership reuses the existing tool and disclosure");
  assert.equal(await orphan.locator('[data-round-id="unassigned"]').count(),0);
  assert.match(await orphan.locator('.agent-tool-card').textContent(),/Complete tool output/);
  assert.equal(await page.locator('[data-agent-node="message-final"]').count(),0,"the trajectory omits duplicate final-answer nodes");
  const finalRound=await page.evaluate(()=>view.model.tasks.get("complete").rounds.find(round=>round.requestId===22).id);
  const finalBranch=page.locator(`[data-agent-node="${finalRound}"]`);
  assert.equal(await finalBranch.locator(':scope > .agent-card > .agent-fold').isVisible(),false,"empty responses do not advertise expansion");
  assert.equal(await finalBranch.locator(':scope > .agent-card > .agent-node-content').getAttribute('aria-expanded'),null);
  assert.ok(await finalBranch.evaluate(node=>{
    const fold=node.querySelector(':scope > .agent-card > .agent-fold');
    const label=node.querySelector('.agent-label');
    const sibling=node.previousElementSibling.querySelector('.agent-label');
    return fold.getBoundingClientRect().width>0 && Math.abs(label.getBoundingClientRect().left-sibling.getBoundingClientRect().left)<1;
  }),"empty and expandable siblings keep their text aligned");
  assert.equal(await finalBranch.locator(':scope > .agent-children > li').count(),0);
  await finalBranch.locator(':scope > .agent-card > .agent-node-inspect').click();
  await page.waitForFunction(()=>document.querySelectorAll('#detail .agent-content')[1]?.textContent.includes('Full model answer'));
  await page.evaluate(() => {
    view.root.hidden=true; document.querySelector('#detail').hidden=true;
    add("agent.tool.result",{task_id:"orphan",agenttomo_toolcall_id:"orphan-tool",payload:{status:"unavailable",error:"Return capture unavailable"}});
  });
  for (const width of [390,320]) {
    await page.setViewportSize({width,height:900});
    await orphan.scrollIntoViewIfNeeded();
    assert.ok(await page.locator('.agent-history').evaluate(node=>node.scrollWidth<=node.clientWidth+1),`response/tool cards must wrap at ${width}px`);
  }
  await page.setViewportSize({width:1280,height:900});
  const normalizedRounds = await page.evaluate(() => {
    add("agent.task.started",{task_id:"normalized",prompt:"Render normalized messages from another protocol."});
    add("agent.model.request",{task_id:"normalized",request_id:301});
    add("agent.message",{task_id:"normalized",request_id:301,item_id:"normalized-progress",message_kind:"intermediate",phase:"final_answer",preview:"Checking the result."});
    add("agent.model.request",{task_id:"normalized",request_id:302});
    add("agent.model.started",{task_id:"normalized",request_id:302,model:"provider-model"});
    add("agent.message",{task_id:"normalized",request_id:302,item_id:"normalized-final",message_kind:"final",phase:"commentary",preview:"The result is ready."});
    add("agent.task.finished",{task_id:"normalized",exit_code:0});
    view.root.hidden=false; view.render(); view.renderChat();
    return view.model.task("normalized").rounds.map(round=>round.id);
  });
  const normalized = page.locator('[data-task-id="normalized"]');
  assert.equal(await normalized.locator('.agent-final-answers [data-entry-id="normalized-final"]').count(),1);
  assert.equal(await normalized.locator('.agent-timeline [data-entry-id="normalized-progress"]').count(),1);
  assert.equal(await normalized.locator('.agent-work').evaluate(node=>node.open),false,"normalized messages retain completed-task folding");
  assert.equal(await page.locator(`[data-agent-node="${normalizedRounds[0]}"] > .agent-card .agent-node-meta`).textContent(),"Unknown model · ended");
  assert.equal(await page.locator(`[data-agent-node="${normalizedRounds[1]}"] > .agent-card .agent-node-meta`).textContent(),"provider-model · ended");
  assert.equal(await page.locator('[data-agent-node="message-normalized-final"]').count(),0,"the normalized final answer is not duplicated in the trajectory");
  const liveRound=await page.evaluate(()=>{
    add("agent.task.started",{task_id:"simple",prompt:"test"});
    add("agent.model.request",{task_id:"simple",request_id:400,model:"test-model"});
    view.render();
    return view.model.task("simple").rounds[0].id;
  });
  const liveFold=page.locator(`[data-agent-node="${liveRound}"] > .agent-card > .agent-fold`);
  assert.equal(await liveFold.isVisible(),false);
  await page.evaluate(()=>{
    add("agent.message.delta",{task_id:"simple",request_id:400,item_id:"simple-answer",delta:"Test received."});
    view.render();
  });
  assert.equal(await liveFold.isVisible(),true,"a new child makes the disclosure available without reloading");
  await liveFold.click();
  assert.equal(await liveFold.getAttribute('aria-expanded'),"false");
  await page.evaluate(()=>{
    add("agent.message",{task_id:"simple",request_id:400,item_id:"simple-answer",message_kind:"final",preview:"Test received."});
    add("agent.task.finished",{task_id:"simple",exit_code:0});
    view.render();
    view.root.hidden=true; document.querySelector('#detail').hidden=true;
    Object.assign(view.chat.style,{width:"100%",maxWidth:"none"});
  });
  const simple=page.locator('[data-task-id="simple"]');
  await simple.locator('.agent-final-answers [data-entry-id="simple-answer"]').waitFor();
  assert.equal(await liveFold.isDisabled(),true,"the disclosure disappears when its last visible child is removed");
  assert.equal(await page.locator(`[data-agent-node="${liveRound}"] > .agent-children > li`).count(),0);
  const emptySummary=simple.locator('.agent-work > summary');
  assert.equal(await emptySummary.evaluate(node=>getComputedStyle(node,'::after').visibility),"hidden","a completed answer without execution steps has no empty disclosure");
  await emptySummary.click();
  assert.equal(await simple.locator('.agent-work').evaluate(node=>node.open),false);
  await page.setViewportSize({width:1600,height:900});
  await simple.scrollIntoViewIfNeeded();
  assert.ok(await simple.evaluate(node=>node.closest('.agent-history').clientWidth-node.clientWidth<=48),"wide chat uses the panel width with compact side margins");
  assert.ok(await emptySummary.evaluate(node=>node.getBoundingClientRect().right-node.querySelector('.agent-chat-outcome').getBoundingClientRect().right<64),"the status duration stays right-aligned without an arrow");
  if(process.env.AGENTTOMO_TEST_ARTIFACTS) await page.screenshot({path:`${process.env.AGENTTOMO_TEST_ARTIFACTS}/chat-wide.png`});
  assert.deepEqual(errors,[]);
  console.log(JSON.stringify({passed:true,ordering:true,stableNodes:true,textSelection:true,toolDetails:true,scrollFollowing:true,stopFeedback:true,draftPreserved:true,completedHierarchy:true,sanitizedMarkdown:true,answerCopyFeedback:true,narrowLayout:true,normalizedMessages:true,genericModelLabels:true}));
} finally {
  await page?.close(); await browser?.close(); server.closeAllConnections(); server.close();
}
