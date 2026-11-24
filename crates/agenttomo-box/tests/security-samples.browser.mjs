// Opt-in: real host Agent + real model + normal Playground UI. No model fixture.
// Uses only the sample's disposable files and fake credential. Requires a live
// login, proxy and existing test browser; ordinary unit suites never call a model.
import assert from 'node:assert/strict';
import {spawn,execFileSync} from 'node:child_process';
import {mkdtemp,rm,mkdir,writeFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {fileURLToPath} from 'node:url';
const {chromium}=await import(process.env.AGENTTOMO_PLAYWRIGHT_MODULE || 'playwright');
const repo=fileURLToPath(new URL('../../..',import.meta.url));
const directory=await mkdtemp(`${tmpdir()}/agenttomo-security-e2e-`);
const overlayRoot=process.env.AGENTTOMO_TEST_OVERLAY_ROOT!=='0';
const agent=process.env.AGENTTOMO_TEST_AGENT || 'codex';
const config=process.env.AGENTTOMO_TEST_CONFIG ? ['--config',process.env.AGENTTOMO_TEST_CONFIG] : [];
const server=spawn(`${repo}/target/debug/agenttomo-box`,['playground',...config,'--listen','127.0.0.1:0','--image','agenttomo-playground:24.04','--capture-http',...(overlayRoot?['--overlay-root']:[]),'--output-dir',directory],{stdio:['ignore','pipe','pipe'],detached:true});
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
 const url = new URL(output.match(/http:\/\/\S+/)[0]);
 url.searchParams.set('agent',agent);
 await page.goto(url.href);
 await until(async()=>await page.locator('#status').textContent()==='Connected','sandbox ready',60000);
 assert.equal(session.agent_available,true,'real external Agent discovered');
 assert.equal(session.agent.id,agent,'requested Agent selected');
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
 const completed = new Set();
 for(const id of sampleIds) {
  await page.locator(`[data-sample-id="${id}"]`).click();
  assert.equal(await page.locator('.sample-chain li').count(),3);
  if(process.env.AGENTTOMO_TEST_ARTIFACTS){
   await mkdir(process.env.AGENTTOMO_TEST_ARTIFACTS,{recursive:true});
   await page.locator('#security-samples').screenshot({path:`${process.env.AGENTTOMO_TEST_ARTIFACTS}/${id}-detail.png`});
  }
  const firstEvent=events.length;
  assert.ok(await page.locator('.sample-run').isEnabled(),`${agent}: Run is enabled`);
  await page.locator('.sample-run').click();
  await until(()=>events.slice(firstEvent).some(e=>e.kind==='security.sample.started'&&e.data.sample_id===id),`${id} started`);
  const start=events.slice(firstEvent).find(e=>e.kind==='security.sample.started'&&e.data.sample_id===id);
  assert.deepEqual(events.filter(e=>e.kind==='security.sample.progress'&&e.data.task_id===start.data.task_id).map(e=>e.data.phase),['preparing','injecting']);
  console.log(`RUN ${id} ${start.data.task_id}`);
  assert.equal(start.data.injection.storage,overlayRoot?'AgentTomo OverlayFS upper':'Disposable container filesystem');
  assert.equal(start.data.injection.matches_baseline,false,`${id}: installed tool must differ from its trusted baseline`);
  execFileSync('docker',['exec',session.container,'test',...(overlayRoot?['!','-e']:['-d']),start.data.directory],{timeout:15000});
  assert.equal(await page.locator('.sample-run').isDisabled(),true);
  assert.equal(await page.locator('.sample-progress').getAttribute('data-phase'),'executing');
  assert.equal(await page.locator('#finding-count').textContent(),String(completed.size),'setup alone adds no finding');
  if(process.env.AGENTTOMO_TEST_ARTIFACTS) await page.locator('#security-samples').screenshot({path:`${process.env.AGENTTOMO_TEST_ARTIFACTS}/${id}-running.png`});
  await until(()=>events.some(e=>e.kind==='security.sample.finished'&&e.data.task_id===start.data.task_id),`${id} result`);
  const result=events.find(e=>e.kind==='security.sample.finished'&&e.data.task_id===start.data.task_id);
  assert.deepEqual(events.filter(e=>e.kind==='security.sample.progress'&&e.data.task_id===start.data.task_id).map(e=>e.data.phase),['preparing','injecting','verifying']);
  assert.equal(await page.locator('.sample-progress').getAttribute('data-phase'),'completed');
  console.log(JSON.stringify({sample:id,status:result.data.status,observed:result.data.observed,error:result.data.error}));
  assert.equal(result.data.injection.matches_baseline,false,'finished record retains pre-execution evidence');
  assert.equal(result.data.exit_code,0,`${id}: Agent run must complete`);
  assert.equal(result.data.status,'reproduced',`${id}: the sample must have observed impact`);
  if(id==='tool-leak') {
   const match=result.data.report_evidence?.[0];
   assert.equal(match?.matched,true,'a task-scoped report write must contain this trial’s marker');
   await until(()=>events.some(e=>e.kind==='file.report_marker' && e.data.task_id===start.data.task_id &&
    e.data.path===result.data.proof_path && e.data.process_instance_id===match.process_instance_id &&
    e.data.matched===true && e.data.captured_unix_ns===match.captured_unix_ns),'corresponding process event');
  } else assert.ok(result.data.observed,'positive verdict has an observed artifact');
  if(!overlayRoot && result.data.observed != null) assert.deepEqual(JSON.parse(execFileSync('docker',['exec',session.container,'cat',result.data.proof_path],{encoding:'utf8',timeout:15000})),result.data.observed,'the container file matches the reported artifact');
  completed.add(start.data.task_id);
  await until(()=>events.some(e=>e.kind==='agent.task.finished'&&e.data.task_id===start.data.task_id),'agent finish');
  assert.ok(events.some(e=>e.kind==='agent.model.request'&&e.data.task_id===start.data.task_id),'real model request captured');
  assert.ok(events.some(e=>e.kind==='agent.tool.call'&&e.data.task_id===start.data.task_id),'tool invocation for reproduced impact captured');
  if(id!=='tool-leak') assert.ok(events.some(e=>e.kind==='process.exec'&&e.data.agent?.task_id===start.data.task_id
   && /(?:^|\/)python3(?:\.\d+)?$/.test(e.data.argv?.[0] || '')&&e.data.argv?.[1]===start.data.injection.path),
   `${id}: the Agent must actually execute the installed Python tool`);
  assert.ok(!/Reproduced|Not reproduced/.test(await page.locator('#agent-trajectory').textContent()),'trajectory is neutral');
  await page.locator('[data-observation="findings"]').click();
  const cardSelector=`.finding[data-task-id="${start.data.task_id}"][data-status="reproduced"]`;
  const finding=page.locator(cardSelector);
  await finding.waitFor();
  assert.equal(await finding.locator('.finding-verdict').textContent(),'实际影响 · Reproduced');
  if(result.data.observed !== undefined) assert.deepEqual(JSON.parse(await finding.locator('.finding-proof').textContent()),result.data.observed);
  else assert.ok(result.data.final_artifact_error);
  assert.equal(await page.locator('#finding-count').textContent(),String(completed.size));
  await finding.getByRole('button',{name:'Evidence',exact:true}).click();
  assert.match(await page.locator('#event-json').textContent(),/security.sample.finished/);
  await monitor.locator('[data-observation="findings"]').click();
  await monitor.locator(cardSelector).waitFor();
  assert.equal(await monitor.locator('#finding-count').textContent(),String(completed.size));
  if(process.env.AGENTTOMO_TEST_ARTIFACTS){await mkdir(process.env.AGENTTOMO_TEST_ARTIFACTS,{recursive:true});await page.screenshot({path:`${process.env.AGENTTOMO_TEST_ARTIFACTS}/${id}-${start.data.task_id}.png`});}
  await page.getByRole('button',{name:'All samples',exact:true}).click();
 }
 await monitor.reload();await monitor.locator('[data-observation="findings"]').click();
 for (const taskId of completed) {
  await monitor.locator(`.finding[data-task-id="${taskId}"][data-status="reproduced"]`).waitFor();
 }
 assert.equal(await monitor.locator('#finding-count').textContent(),String(completed.size));
 await page.locator('[data-sample-id="tool-leak"]').click();
 const beforeCancel = events.filter(e=>e.kind==='security.sample.started').length;
 await page.locator('.sample-run').click();
 await until(()=>events.filter(e=>e.kind==='security.sample.started').length>beforeCancel,'cancel trial starts');
 const cancelledTask = events.findLast(e=>e.kind==='security.sample.started').data.task_id;
 await page.locator('#security-samples').getByRole('button',{name:'Stop',exact:true}).click();
 await until(()=>events.some(e=>e.kind==='security.sample.finished'&&e.data.task_id===cancelledTask),'cancel verdict');
 assert.equal(events.find(e=>e.kind==='security.sample.finished'&&e.data.task_id===cancelledTask).data.status,'cancelled');
 await until(async()=>await page.locator('.sample-run').isEnabled(),'Run recovers after Stop',30000);
 assert.equal(await page.locator('#finding-count').textContent(),String(completed.size),'cancelled run adds no finding');
 for(const width of [768,390]) {await page.setViewportSize({width,height:900});assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=document.documentElement.clientWidth+1),'layout fits viewport');}
 assert.deepEqual(pageErrors,[]);
 console.log(`PASS: ${completed.size} reproduced impacts verified from final artifacts, disposable workspace isolation, monitor replay and cancellation`);
} finally {
 try {
  if(process.env.AGENTTOMO_TEST_ARTIFACTS){
   await mkdir(process.env.AGENTTOMO_TEST_ARTIFACTS,{recursive:true});
   await writeFile(`${process.env.AGENTTOMO_TEST_ARTIFACTS}/events.json`,JSON.stringify(events));
   await writeFile(`${process.env.AGENTTOMO_TEST_ARTIFACTS}/server-stderr.log`,errors);
  }
 } finally {
  await page?.close().catch(()=>{});await monitor?.close().catch(()=>{});await browser?.close().catch(()=>{});
  try{process.kill(-server.pid,'SIGINT');}catch{}
  for(let i=0;i<100&&server.exitCode===null;i++)await delay(100);
  if(server.exitCode===null){try{process.kill(-server.pid,'SIGKILL');}catch{}}
  if(session?.container){try{execFileSync('docker',['rm','-f',session.container],{stdio:'ignore',timeout:15000});}catch{}}
  await rm(directory,{recursive:true,force:true});
 }
}
