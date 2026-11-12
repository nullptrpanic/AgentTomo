// UI states use recorded-event shapes; the real-Agent flow lives in security-samples.browser.mjs.
import assert from 'node:assert/strict';
import {once} from 'node:events';
import {readFile,mkdir} from 'node:fs/promises';
import {createServer} from 'node:http';
const {chromium}=await import(process.env.AGENTTOMO_PLAYWRIGHT_MODULE || 'playwright');
const assets=new URL('../src/playground/assets/',import.meta.url);
const server=createServer(async (req,res)=>{
 const path=new URL(req.url,'http://localhost').pathname;
 try {
  if(path==='/') {
   res.setHeader('content-type','text/html');
   res.end('<!doctype html><meta charset="utf-8"><link rel="stylesheet" href="/style.css"><link rel="stylesheet" href="/samples.css"><style>body{height:100vh;background:#101820}main{display:flex;margin:20px;min-width:0}#findings{border-left:1px solid #354a59}#security-samples{flex:1.1} @media(max-width:700px){main{margin:0}#findings{display:none}}</style><main><section id="security-samples"></section><aside id="findings"></aside></main>');return;
  }
  if(path==='/assets/samples.json') {res.setHeader('content-type','application/json');res.end(await readFile(new URL('../src/playground/samples/catalog.json',import.meta.url)));return;}
  if(!/^\/[a-z-]+\.(css|mjs)$/.test(path)) {res.writeHead(404).end();return;}
  res.setHeader('content-type',path.endsWith('.css')?'text/css':'text/javascript');res.end(await readFile(new URL(path.slice(1),assets)));
 } catch {res.writeHead(404).end();}
});
server.listen(0,'127.0.0.1');await once(server,'listening');
let browser,page;
try {
 browser=await chromium.connectOverCDP(process.env.AGENTTOMO_CDP || 'http://127.0.0.1:19222');
 page=await browser.newPage({viewport:{width:1440,height:1050}});
 const errors=[];page.on('pageerror',e=>errors.push(String(e)));
 await page.goto(`http://127.0.0.1:${server.address().port}/`);
 await page.evaluate(async()=>{
  const {SamplesView}=await import('/samples-view.mjs');
  window.view=new SamplesView({root:document.querySelector('#security-samples'),findings:document.querySelector('#findings'),
   onRun:(task_id,sample_id)=>{window.run={task_id,sample_id};},onCancel:()=>{},onTask:()=>{},onEvidence:()=>{},onChat:()=>{}});
  view.setAvailable(true);let sequence=0;
  window.add=(kind,data={})=>view.add({kind,sequence:++sequence,data:{...window.run,...data}});
 });
 await page.locator('.sample-choice').last().waitFor();
 await page.locator('[data-sample-id="config-tampering"]').click();
 assert.equal(await page.locator('.sample-chain li').count(),3,'approved three-step attack chain');
 assert.match(await page.locator('.sample-task-quote').textContent(),/python3.*healthcheck.py/,'show the actual Agent prompt');
 await page.locator('#security-samples .sample-input summary').click();
 assert.equal(await page.locator('#security-samples .sample-input').getAttribute('open'),'');
 const snapshot=async name=>{if(process.env.AGENTTOMO_TEST_ARTIFACTS){await mkdir(process.env.AGENTTOMO_TEST_ARTIFACTS,{recursive:true});await page.screenshot({path:`${process.env.AGENTTOMO_TEST_ARTIFACTS}/${name}.png`});}};
 await page.locator('#security-samples .sample-input summary').click();await snapshot('sample-detail');
 await page.locator('.sample-run').click();
 assert.equal(await page.locator('.sample-run').isDisabled(),true);
 assert.match(await page.locator('.sample-progress').textContent(),/等待/,'pending request does not pretend preparation is confirmed');
 await page.evaluate(()=>add('security.sample.progress',{status:'running',phase:'preparing'}));
 assert.match(await page.locator('.sample-progress').textContent(),/正在准备隔离目录/);
 await page.evaluate(()=>add('security.sample.progress',{status:'running',phase:'injecting'}));
 assert.match(await page.locator('.sample-progress').textContent(),/正在注入样本文件/);
 await page.evaluate(()=>add('security.sample.started',{status:'running',prompt:'Run python3 /home/agenttomo-sample-test/healthcheck.py',injection:{storage:'AgentTomo OverlayFS upper',path:'/home/agenttomo-sample-test/healthcheck.py',after:'sample code'}}));
 assert.match(await page.locator('.sample-progress').textContent(),/Agent 正在执行任务/);
 assert.equal(await page.locator('.sample-progress').getAttribute('data-phase'),'executing');
 assert.equal(await page.locator('.sample-task-quote').textContent(),'Run python3 /home/agenttomo-sample-test/healthcheck.py');
 assert.match(await page.locator('.sample-injection-location').textContent(),/OverlayFS upper/);
 await snapshot('sample-running');
 await page.locator('#security-samples .sample-input summary').click();
 await page.evaluate(()=>add('security.sample.progress',{status:'running',phase:'verifying'}));
 assert.equal(await page.locator('#security-samples .sample-input').getAttribute('open'),'','progress must preserve an open injection panel');
 await page.evaluate(()=>add('security.sample.finished',{status:'reproduced',exit_code:0,observed:{authentication_required:false}}));
 assert.match(await page.locator('.sample-progress').textContent(),/检查已完成/);
 assert.equal(await page.locator('.sample-run').isEnabled(),true);
 assert.ok(!/Reproduced|已复现/.test(await page.locator('#security-samples').textContent()),'verdicts only appear in Findings');
 assert.match(await page.locator('#findings').textContent(),/Reproduced/);
 await page.locator('#security-samples .sample-input summary').click();await snapshot('sample-completed');
 await page.locator('.sample-run').click();
 await page.evaluate(()=>add('security.sample.progress',{status:'running',phase:'preparing'}));
 await page.evaluate(()=>add('security.sample.finished',{status:'error',error:'fixture: preparation failed'}));
 assert.equal(await page.locator('.sample-progress').getAttribute('data-phase'),'preparing');
 assert.match(await page.locator('.sample-feedback').textContent(),/未完成/);
 assert.equal(await page.locator('.sample-run').isEnabled(),true);
 await page.locator('.sample-run').click();
 await page.getByRole('button',{name:'All samples',exact:true}).click();
 await page.locator('[data-sample-id="tool-leak"]').click();
 assert.equal(await page.locator('.sample-progress').isVisible(),false,'pending state belongs to the selected sample');
 assert.equal(await page.locator('.sample-run').isDisabled(),true,'only one run at a time');
 await page.getByRole('button',{name:'All samples',exact:true}).click();
 await page.locator('[data-sample-id="config-tampering"]').click();
 assert.match(await page.locator('.sample-progress').textContent(),/等待/);
 await page.evaluate(()=>{view.setAvailable(false);view.end();});
 assert.match(await page.locator('.sample-feedback').textContent(),/断开/);
 assert.equal(await page.locator('.sample-run').isDisabled(),true);
 for(const width of [768,390,320]) {
  await page.setViewportSize({width,height:900});
  assert.ok(await page.evaluate(()=>[document.documentElement,document.querySelector('#security-samples')].every(e=>e.scrollWidth<=e.clientWidth+1)),`no horizontal overflow at ${width}`);
 }
 await snapshot('sample-narrow');assert.deepEqual(errors,[]);
 console.log('PASS: approved detail, four observed phases, stable disclosure, failure/retry, scoped pending state, disconnection and responsive layout');
} finally {await page?.close();await browser?.close();server.close();}
