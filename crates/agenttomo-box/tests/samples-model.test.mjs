import assert from 'node:assert/strict';
import {test} from 'node:test';
import {SamplesModel} from '../src/playground/assets/samples-model.mjs';
let sequence = 0;
const event = (kind,task_id,data={}) => ({kind,sequence:++sequence,data:{task_id,...data}});
test('a completed report finding can open the original write even if the trace arrives after the verdict', () => {
 const model = new SamplesModel();
 const start = event('security.sample.started','trial',{sample_id:'tool-leak',status:'running'});
 const write = event('file.report_marker','trial',{matched:true,pid:42,process_instance_id:7});
 const cleared = event('file.report_marker','trial',{matched:false,pid:43,process_instance_id:8});
 const finish = event('security.sample.finished','trial',{sample_id:'tool-leak',status:'reproduced',observed:{diagnostic_token:''},report_evidence:[write.data,cleared.data]});
 model.add(start);
 model.add(write);
 assert.equal(model.findingCount,0);
 model.add(finish);
 model.add(cleared);
 model.add(write);
 assert.equal(model.findingCount,1);
 assert.equal(model.reportEvent('trial',true),write);
 assert.equal(model.reportEvent('trial',false),cleared);
 assert.equal(model.reportEvent('other',true),undefined);
});
test('Findings counts only finished reproduced impacts, once per run on replay', () => {
 const model = new SamplesModel();
 const sample = {sample_id:'tool-leak'};
 const start = event('security.sample.started','protected',{...sample,status:'running',injection:{matches_baseline:false}});
 const finish = event('security.sample.finished','protected',{...sample,status:'not_reproduced',observed:{diagnostic_token:''}});
 model.add(start);
 assert.equal(model.findingCount,0,'injection alone is setup evidence');
 model.add(finish);
 model.add(start);
 model.add(finish);
 assert.equal(model.findingCount,0,'an empty token is a negative impact verdict');
 assert.equal(model.runs.get('protected').status,'not_reproduced');
 const positive = event('security.sample.finished','both',{...sample,status:'reproduced',injection:{matches_baseline:false}});
 model.add(positive);
 model.add(positive);
 assert.equal(model.findingCount,1);
 model.add(event('security.sample.finished','clean',{...sample,status:'not_reproduced',injection:{matches_baseline:true}}));
 model.add(event('security.sample.finished','legacy',{...sample,status:'not_reproduced',injection:{before:'trusted',after:'replacement'}}));
 model.add(event('security.sample.finished','old-positive',{...sample,status:'reproduced'}));
 model.add(event('security.sample.finished','failed-before-check',{...sample,status:'error'}));
 for (const status of ['cancelled','error']) {
  model.add(event('security.sample.finished',status,{...sample,status,injection:{matches_baseline:false}}));
 }
 assert.equal(model.findingCount,2);
 model.add(event('security.sample.started','interrupted',{...sample,status:'running',injection:{matches_baseline:false}}));
 model.end();
 assert.equal(model.findingCount,2);
});
test('runs stay independent, replay is idempotent and agent completion is not an attack verdict', () => {
 const model=new SamplesModel();
 model.add(event('security.sample.started','first',{sample_id:'tool-leak',status:'running'}));
 model.add(event('security.sample.started','second',{sample_id:'tool-leak',status:'running'}));
 model.add(event('agent.task.finished','first',{exit_code:0}));
 assert.equal(model.runs.get('first').status,'running');
 const result=event('security.sample.finished','first',{sample_id:'tool-leak',status:'reproduced',observed:{diagnostic_token:'FAKE'}});
 model.add(result); model.add(result);
 assert.equal(model.runs.size,2);
 assert.equal(model.runs.get('first').status,'reproduced');
 assert.equal(model.runs.get('second').status,'running');
});
test('interrupted streams cannot leave a sample looking as if it is still executing', () => {
 const model=new SamplesModel();
 model.add(event('security.sample.started','first',{sample_id:'tool-leak',status:'running'}));
 model.end();
 assert.equal(model.runs.get('first').status,'interrupted');
});
test('progress follows observed phases and a finished run cannot restart on a late progress event', () => {
 const model = new SamplesModel();
 model.add(event('security.sample.progress','trial',{sample_id:'tool-leak',status:'running',phase:'preparing'}));
 model.add(event('security.sample.progress','trial',{sample_id:'tool-leak',status:'running',phase:'injecting'}));
 model.add(event('security.sample.started','trial',{sample_id:'tool-leak',status:'running',injection:{path:'/home/trial/report.py'}}));
 assert.equal(model.runs.get('trial').phase,'executing');
 const verification = event('security.sample.progress','trial',{sample_id:'tool-leak',status:'running',phase:'verifying'});
 model.add(verification);
 assert.equal(model.runs.get('trial').injection.path,'/home/trial/report.py');
 model.add(event('security.sample.finished','trial',{sample_id:'tool-leak',status:'not_reproduced',exit_code:0}));
 assert.equal(model.runs.get('trial').phase,'completed');
 model.add(verification);
 model.add(event('security.sample.progress','trial',{sample_id:'tool-leak',status:'running',phase:'verifying'}));
 assert.equal(model.runs.get('trial').status,'not_reproduced');
});
test('failed or interrupted runs retain their last phase instead of completing the progress bar', () => {
 const model = new SamplesModel();
 for (const status of ['error','cancelled','interrupted']) {
  model.add(event('security.sample.progress',status,{sample_id:'tool-leak',status:'running',phase:'injecting'}));
  if (status === 'interrupted') model.end();
  else model.add(event('security.sample.finished',status,{sample_id:'tool-leak',status}));
  assert.equal(model.runs.get(status).phase,'injecting');
  assert.equal(model.runs.get(status).status,status);
 }
});
