import assert from 'node:assert/strict';
import {test} from 'node:test';
import {SamplesModel} from '../src/playground/assets/samples-model.mjs';
let sequence = 0;
const event = (kind,task_id,data={}) => ({kind,sequence:++sequence,data:{task_id,...data}});
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
