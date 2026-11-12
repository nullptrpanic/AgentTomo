// Opt-in: real host Agent + real model + normal Playground UI. No model fixture.
// Uses only the sample's disposable files and fake credential. Requires a live
// login, proxy and existing test browser; ordinary unit suites never call a model.
import assert from 'node:assert/strict';
import {spawn,execFileSync} from 'node:child_process';
import {mkdtemp,rm,mkdir} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {fileURLToPath} from 'node:url';
const {chromium}=await import(process.env.AGENTTOMO_PLAYWRIGHT_MODULE || 'playwright');
const repo=fileURLToPath(new URL('../../..',import.meta.url));
const directory=await mkdtemp(`${tmpdir()}/agenttomo-security-e2e-`);
const server=spawn(`${repo}/target/debug/agenttomo-box`,['playground','--listen','127.0.0.1:0','--image','agenttomo-playground:24.04','--capture-http','--overlay-root','--output-dir',directory],{stdio:['ignore','pipe','pipe'],detached:true});
let output='',errors='',session,browser,page,monitor;
const events=[],pageErrors=[];
server.stdout.on('data',data=>output+=data);server.stderr.on('data',data=>errors+=data);
const delay=ms=>new Promise(resolve=>setTimeout(resolve,ms));
async function until(predicate,message,timeout=240000) {const deadline=Date.now()+timeout;while(!await predicate()){assert.ok(Date.now()<deadline,`${message}\n${errors.slice(-3000)}`);await delay(200);}}
try {
 await until(()=>output.includes('?token='),'server start',30000);
 browser=await chromium.connectOverCDP(process.env.AGENTTOMO_CDP || 'http://127.0.0.1:19222');
 page=await browser.newPage({viewport:{width:1512,height:1000}});
 page.on('pageerror',error=>pageErrors.push(String(error)));
 page.on('websocket',socket=>socket.on('framereceived',({payload})=>{if(typeof payload!=='string'||!payload.startsWith('{'))return;const frame=JSON.parse(payload);if(frame.type==='session')session=frame;if(frame.type==='trace')events.push(frame.event);}));
 await page.goto(output.match(/http:\/\/\S+/)[0]);
 await until(async()=>await page.locator('#status').textContent()==='Connected','sandbox ready',60000);
 assert.equal(session.agent_available,true,'real external Agent discovered');
 assert.equal(await page.locator('[data-observation="graph"]').count(),0);
 assert.equal(await page.locator('#graph-scroll').count(),0);
 assert.equal((await page.request.get(new URL('/assets/behavior-graph.mjs',page.url()).href)).status(),404);
 await page.locator('[data-workspace="samples"]').click();
 await page.locator('.sample-choice').last().waitFor();
 assert.equal(await page.locator('#workspace-panel .sample-choice').count(),3);
 assert.equal(await page.locator('#monitor-panel .sample-choice').count(),0);
 const monitorURL=await page.locator('#open-monitor').getAttribute('href');
 monitor=await browser.newPage();await monitor.goto(monitorURL);
 await until(async()=>await monitor.locator('#status').textContent()==='Following session','monitor ready',60000);
 const sampleIds = process.env.AGENTTOMO_SAMPLE_IDS?.split(',') || ['tool-leak','config-tampering','tool-result-poisoning'];
 for(const id of sampleIds) {
  await page.locator(`[data-sample-id="${id}"]`).click();
  assert.equal(await page.locator('.sample-chain li').count(),3);
  if(process.env.AGENTTOMO_TEST_ARTIFACTS){
   await mkdir(process.env.AGENTTOMO_TEST_ARTIFACTS,{recursive:true});
   await page.locator('#security-samples').screenshot({path:`${process.env.AGENTTOMO_TEST_ARTIFACTS}/${id}-detail.png`});
  }
  await page.locator('.sample-run').click();
  await until(()=>events.some(e=>e.kind==='security.sample.started'&&e.data.sample_id===id),`${id} started`);
  const start=events.find(e=>e.kind==='security.sample.started'&&e.data.sample_id===id);
  assert.deepEqual(events.filter(e=>e.kind==='security.sample.progress'&&e.data.task_id===start.data.task_id).map(e=>e.data.phase),['preparing','injecting']);
  console.log(`RUN ${id} ${start.data.task_id}`);
  assert.equal(start.data.injection.storage,'AgentTomo OverlayFS upper');
  execFileSync('docker',['exec',session.container,'test','!','-e',start.data.directory],{timeout:15000});
  assert.equal(await page.locator('.sample-run').isDisabled(),true);
  assert.equal(await page.locator('.sample-progress').getAttribute('data-phase'),'executing');
  if(process.env.AGENTTOMO_TEST_ARTIFACTS) await page.locator('#security-samples').screenshot({path:`${process.env.AGENTTOMO_TEST_ARTIFACTS}/${id}-running.png`});
  await until(()=>events.some(e=>e.kind==='security.sample.finished'&&e.data.task_id===start.data.task_id),`${id} result`);
  const result=events.find(e=>e.kind==='security.sample.finished'&&e.data.task_id===start.data.task_id);
  assert.deepEqual(events.filter(e=>e.kind==='security.sample.progress'&&e.data.task_id===start.data.task_id).map(e=>e.data.phase),['preparing','injecting','verifying']);
  assert.equal(await page.locator('.sample-progress').getAttribute('data-phase'),'completed');
  console.log(JSON.stringify({sample:id,status:result.data.status,observed:result.data.observed,error:result.data.error}));
  if (result.data.status !== 'reproduced') console.log(JSON.stringify(events.filter(e => e.kind === 'agent.message' && e.data.task_id === start.data.task_id).map(e => e.data.preview)));
  assert.equal(result.data.status,'reproduced',`${id}: actual artifact must reproduce the attack`);
  await until(()=>events.some(e=>e.kind==='agent.task.finished'&&e.data.task_id===start.data.task_id),'agent finish');
  assert.ok(events.some(e=>e.kind==='agent.model.request'&&e.data.task_id===start.data.task_id),'real model request captured');
  assert.ok(events.some(e=>e.kind==='agent.tool.call'&&e.data.task_id===start.data.task_id),'real tool invocation captured');
  assert.ok(!/Reproduced|Not reproduced/.test(await page.locator('#agent-trajectory').textContent()),'trajectory is neutral');
  await page.locator('[data-observation="findings"]').click();
  const finding=page.locator(`.finding[data-task-id="${start.data.task_id}"]`);
  await finding.waitFor();assert.equal(await finding.getAttribute('data-status'),'reproduced');
  await finding.getByRole('button',{name:'Evidence',exact:true}).click();
  assert.match(await page.locator('#event-json').textContent(),/security.sample.finished/);
  await monitor.locator('[data-observation="findings"]').click();
  await until(async()=>await monitor.locator(`.finding[data-task-id="${start.data.task_id}"][data-status="reproduced"]`).count()===1,'monitor sees same finding',30000);
  if(process.env.AGENTTOMO_TEST_ARTIFACTS){await mkdir(process.env.AGENTTOMO_TEST_ARTIFACTS,{recursive:true});await page.screenshot({path:`${process.env.AGENTTOMO_TEST_ARTIFACTS}/${id}.png`});}
  await page.getByRole('button',{name:'All samples',exact:true}).click();
 }
 await monitor.reload();await monitor.locator('[data-observation="findings"]').click();
 await until(async()=>await monitor.locator('.finding[data-status="reproduced"]').count()===sampleIds.length,'findings replay',60000);
 await page.locator('[data-sample-id="tool-leak"]').click();
 const beforeCancel = events.filter(e=>e.kind==='security.sample.started').length;
 await page.locator('.sample-run').click();
 await until(()=>events.filter(e=>e.kind==='security.sample.started').length>beforeCancel,'cancel trial starts');
 const cancelledTask = events.findLast(e=>e.kind==='security.sample.started').data.task_id;
 await page.locator('#security-samples').getByRole('button',{name:'Stop',exact:true}).click();
 await until(()=>events.some(e=>e.kind==='security.sample.finished'&&e.data.task_id===cancelledTask),'cancel verdict');
 assert.equal(events.find(e=>e.kind==='security.sample.finished'&&e.data.task_id===cancelledTask).data.status,'cancelled');
 await until(async()=>await page.locator('.sample-run').isEnabled(),'Run recovers after Stop',30000);
 for(const width of [768,390]) {await page.setViewportSize({width,height:900});assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=document.documentElement.clientWidth+1),'layout fits viewport');}
 assert.deepEqual(pageErrors,[]);
 console.log(`PASS: ${sampleIds.length} real-Agent attacks, OverlayFS isolation, observed artifacts, neutral task UI, independent monitor replay and cancellation`);
} finally {
 await page?.close().catch(()=>{});await monitor?.close().catch(()=>{});await browser?.close().catch(()=>{});
 try{process.kill(-server.pid,'SIGINT');}catch{}
 for(let i=0;i<100&&server.exitCode===null;i++)await delay(100);
 if(server.exitCode===null){try{process.kill(-server.pid,'SIGKILL');}catch{}}
 if(session?.container){try{execFileSync('docker',['rm','-f',session.container],{stdio:'ignore',timeout:15000});}catch{}}
 await rm(directory,{recursive:true,force:true});
}
