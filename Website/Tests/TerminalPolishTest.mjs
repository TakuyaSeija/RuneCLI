import assert from 'node:assert/strict';
import path from 'node:path';
import {openBrowser} from './Browser.mjs';
const origin=process.argv[2]||'http://127.0.0.1:18766';
const b=await openBrowser(path.resolve('target/test-tmp/website-terminal-polish'));
try{
 await b.viewport(1366,768,'reduce');await b.navigate(origin+'/Studio/');
 assert.equal(await b.evaluate(`getComputedStyle(document.querySelector('[data-icon=play]')).display!=='none'`),true);
 assert.equal(await b.evaluate(`getComputedStyle(document.querySelector('[data-icon=pause]')).display`),'none');
 assert.equal(await b.evaluate(`document.querySelector('[data-scene=response]').textContent.includes(':5173')`),false);
 assert.equal(await b.evaluate(`[...document.querySelectorAll('[data-failed=true] .step-dot')].every(e=>e.textContent==='●')`),true);
 for(const theme of ['amber','violet','cyan']){
  await b.evaluate(`document.documentElement.dataset.theme='${theme}'`);
  assert.deepEqual(await b.evaluate(`[...document.querySelectorAll('.effort-label')].map(e=>getComputedStyle(e).color)`),['rgb(226, 227, 228)','rgb(226, 227, 228)']);
 }
 await b.evaluate(`document.documentElement.dataset.theme='amber'`);await b.capture('polish-desktop.png');
 await b.viewport(768);await b.navigate(origin+'/Studio/');await b.click('[data-demo-expand]');await b.wait(60);
 assert.equal(await b.evaluate(`document.querySelector('.stage').classList.contains('is-expanded')`),true);
 await b.click('[data-demo-panel]');assert.equal(await b.evaluate(`getComputedStyle(document.querySelector('.tside')).display`),'flex');await b.evaluate(`document.querySelector('[data-side]').scrollTop=0;document.querySelector('[data-side]').dispatchEvent(new Event('scroll'))`);await b.wait(50);assert.equal(await b.evaluate(`document.querySelector('[data-side-plan-heading]').hidden`),false);await b.capture('polish-mobile-panel.png');
 await b.click('[data-demo-expand]');
 await b.send('Emulation.setScriptExecutionDisabled',{value:true});await b.navigate(origin+'/Studio/',false);
 assert.equal(await b.evaluate(`document.querySelector('[data-replay-controls]').hidden`),true);
 assert.equal(await b.evaluate(`document.querySelectorAll('[data-step]').length===document.querySelector('[data-demo]').dataset.schedule.split('|')[6].split(';').length`),true);
 await b.click('[data-call=create_file] [data-fold]>summary');assert.equal(await b.evaluate(`document.querySelector('[data-call=create_file] [data-fold]').open`),true);
 assert.equal(await b.evaluate(`document.querySelectorAll('[data-copy="install-cmd"]').length`),1);await b.capture('polish-no-js.png');
 await b.send('Emulation.setScriptExecutionDisabled',{value:false});assert.deepEqual(b.errors,[]);
 console.log('PASS polish: SVG, neutral effort across themes, public redactions, scaled/mobile panel, native no-JS disclosure and single install command');
}finally{b.close();}
