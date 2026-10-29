#!/usr/bin/env node
import assert from 'node:assert/strict';
import path from 'node:path';
import {writeFile} from 'node:fs/promises';
import {openBrowser} from './Browser.mjs';
import {replayClock} from './ReplayClock.mjs';
const origin=process.argv[2]||'http://127.0.0.1:18766',output=path.resolve(process.argv[3]||'target/test-tmp/website-terminal-parity');
const b=await openBrowser(output),report={checks:[]};
const advance=ms=>b.evaluate(`__advanceReplay(${ms})`);
const q=selector=>`document.querySelector(${JSON.stringify(selector)})`;
const elapsed=()=>b.evaluate(`+${q('[data-demo]')}.dataset.elapsed`);
const point=async(selector,fx=.5,fy=.5)=>b.evaluate(`(()=>{const r=${q(selector)}.getBoundingClientRect();return{x:r.x+r.width*${fx},y:r.y+r.height*${fy}}})()`);
try{
 await b.send('Page.addScriptToEvaluateOnNewDocument',{source:replayClock});await b.viewport(1366,768,'no-preference');await b.navigate(origin+'/Studio/');await b.wait(80);
 // The session sits below the hero; its clock runs only while it is on screen.
 await b.evaluate(`${q('.stage')}.scrollIntoView({block:'center',behavior:'instant'})`);await b.wait(120);await advance(300);
 await b.evaluate(`${q('[data-demo-replay]')}.click()`);await advance(1100);
 const started=await elapsed();assert(started>=1000);await b.evaluate(`${q('[data-demo-pause]')}.click()`);await advance(1000);assert.equal(await elapsed(),started);await b.evaluate(`${q('[data-demo-pause]')}.click()`);
 const data=await b.evaluate(`(()=>{const s=${q('[data-demo]')}.dataset.schedule.split('|');return{end:+s[1],rows:s[6].split(';').map(r=>r.split(',').map(Number))}})()`);
 assert(data.end>60000&&data.end<95000);
 // Every shown step keeps its own reading-paced timing. The test clock
 // advances the same RAF callbacks, not a hidden production mode.
 let gates=0;
 const offers=[];
 for(const row of data.rows.filter(r=>r[3])){
  await advance(Math.max(0,row[8]+10-await elapsed()));assert.equal(await b.evaluate(`${q('[data-permission]')}.hidden`),false);gates++;
  // Ask mode as the engine asks: a command offers no "always", an edit offers its tool's grant.
  offers.push(await b.evaluate(`(()=>{const a=${q('[data-answer=always]')};return a.hidden?'':a.textContent})()`));
  assert.equal(await b.evaluate(`${q('[data-answer=once]')}.getAttribute('aria-current')`),'true');
  await b.evaluate(`${q('[data-answer=once]')}.click()`);await advance(150);assert.equal(await b.evaluate(`${q('[data-permission]')}.hidden`),true);
 }
 assert.equal(gates,4);assert.deepEqual(offers,['Always allow create_file','','Always allow patch_file','']);report.checks.push('Reading-paced excerpt, explicit Pause, four simulated approvals with the engine\'s own answers');
 await advance(Math.max(0,data.end-500-await elapsed()));await b.wait(60);
 const before=await elapsed();
 await b.evaluate(`${q('[data-side]')}.scrollTop=60;${q('[data-side]')}.dispatchEvent(new WheelEvent('wheel',{deltaY:60,bubbles:true}));${q('[data-log]')}.scrollTop=0`);await b.wait(60);await advance(200);
 assert((await elapsed())>before);assert.equal(await b.evaluate(`${q('[data-demo]')}.dataset.playback`),'playing');assert.equal(await b.evaluate(`${q('[data-log]')}.scrollTop`),0);
 report.checks.push('Sidebar/transcript wheel and reading do not pause, reader scroll is not stolen');
 // Smart heading changes from PLAN to FILES as that section enters the window.
 await b.evaluate(`${q('[data-side]')}.scrollTop=0`);await b.wait(40);
 assert.equal(await b.evaluate(`${q('[data-side-plan-heading]')}.hidden`),false);
 await b.evaluate(`${q('[data-side]')}.scrollTop=${q('[data-side]')}.scrollHeight`);await b.wait(60);
 assert.equal(await b.evaluate(`${q('[data-side-files-heading]')}.hidden`),false);
 assert.equal(await b.evaluate(`getComputedStyle(${q('[data-side-files-heading]')},'::after').height`),'1px');
 report.checks.push('SESSIONS pinned, smart PLAN/FILES heading and continuous rule, above/below markers');
 // Click the blank right half of the Thinking row, not its word, then verify
 // no rectangular browser outline is painted after a mouse click.
 const thought='.thought-fold';await b.evaluate(`${q(thought)}.scrollIntoView({block:'center',behavior:'instant'})`);await b.wait(30);
 const hit=await point(thought+'>summary',.9,.5);await b.send('Input.dispatchMouseEvent',{type:'mousePressed',button:'left',clickCount:1,...hit});await b.send('Input.dispatchMouseEvent',{type:'mouseReleased',button:'left',clickCount:1,...hit});await b.wait(40);
 assert.equal(await b.evaluate(`${q(thought)}.open`),true);assert.equal(await b.evaluate(`getComputedStyle(${q(thought+'>summary')}).cursor`),'default');assert.equal(await b.evaluate(`getComputedStyle(${q(thought+'>summary')}).outlineStyle`),'none');
 const time=await elapsed();await advance(100);assert((await elapsed())>time);
 await b.capture('thinking-open.png');
 const bash='[data-call=terminal_use]';await b.evaluate(`${q(bash+' .step-title')}.scrollIntoView({block:'center',behavior:'instant'})`);await b.wait(40);
 await b.click(bash+' .step-title');assert.equal(await b.evaluate(`[...${q(bash)}.querySelectorAll('details')].every(e=>e.open)`),true);
 await b.click(bash+' .step-title');assert.equal(await b.evaluate(`[...${q(bash)}.querySelectorAll('details')].every(e=>!e.open)`),true);
 report.checks.push('Whole Thinking/tool area click, grouped Bash command/output, normal cursor, no mouse focus box, open/close does not pause');
 // Native browser selection on actual scaled content; release must copy only
 // text and clear the highlight, without toggling the containing disclosure.
 await b.evaluate(`${q('[data-demo-pause]')}.click()`);await b.evaluate(`${q('[data-call=create_file]:not([data-failed=true]) details')}.open=true`);await b.wait(50);
 const row='[data-call=create_file]:not([data-failed=true]) .content-row:nth-child(2)';await advance(0);await b.wait(60);await b.evaluate(`${q('.stage')}.scrollIntoView({block:'center',behavior:'instant'});${q(row)}.scrollIntoView({block:'center',behavior:'instant'})`);await advance(0);await b.wait(60);await b.evaluate(`${q(row)}.scrollIntoView({block:'center',behavior:'instant'})`);
 await b.evaluate(`window.__clipboardCopies=[];window.__selectionEvents=[];for(const name of ['pointerdown','pointerup','selectionchange'])document.addEventListener(name,e=>__selectionEvents.push({type:e.type,tag:e.target.tagName,selection:getSelection().toString()}),true);Object.defineProperty(navigator,'clipboard',{configurable:true,value:{writeText:text=>{__clipboardCopies.push(text);return Promise.resolve()}}})`);
 const a=await point(row,.005,.5),z={x:a.x+130,y:a.y};assert(a.y>80&&a.y<740,JSON.stringify({dragPoint:a}));
 await b.send('Input.dispatchMouseEvent',{type:'mousePressed',button:'left',buttons:1,clickCount:1,...a});
 for(let i=1;i<=8;i++){await b.send('Input.dispatchMouseEvent',{type:'mouseMoved',button:'left',buttons:1,x:a.x+(z.x-a.x)*i/8,y:a.y});await b.wait(20);}
 await b.send('Input.dispatchMouseEvent',{type:'mouseReleased',button:'left',clickCount:1,...z});await b.wait(60);
 assert.equal(await b.evaluate(`getSelection().toString()`),'');const copied=await b.evaluate(`__clipboardCopies`);assert(copied.length>0&&copied[0].length>0,JSON.stringify(await b.evaluate(`({events:__selectionEvents,hit:document.elementFromPoint(${a.x},${a.y})?.outerHTML.slice(0,300),row:${q(row)}.textContent})`)));assert(!copied[0].includes('lines cut'));assert(copied[0].includes('import'));report.checks.push('Scaled drag selection auto-copies content and clears highlight, no accidental fold');
 await b.evaluate(`${q('[data-log]')}.scrollTop=0`);await b.wait(50);
 assert.equal(await b.evaluate(`${q('[data-jump]')}.textContent`),'Press Ctrl+↓ Jump to latest');assert.equal(await b.evaluate(`getComputedStyle(${q('[data-jump]')}).borderTopWidth`),'0px');assert.equal(await b.evaluate(`getComputedStyle(${q('[data-jump]')}).borderRadius`),'0px');
 await b.evaluate(`${q('[data-jump]')}.click()`);await b.wait(40);assert(await b.evaluate(`${q('[data-log]')}.scrollTop>0`));report.checks.push('Jump label is borderless on the transcript last row and returns to latest');
 await b.evaluate(`${q('[data-demo-pause]')}.click()`);await advance(data.end-await elapsed()+500);
 assert.equal(await b.evaluate(`${q('.term')}.dataset.demoStage`),'complete');assert.equal(await b.evaluate(`${q('[data-f=tin]')}.textContent`),'14,939,074');assert.equal(await b.evaluate(`${q('[data-files-count]')}.textContent`),'64');
 await b.evaluate(`${q('[data-demo-pause]')}.click()`);await advance(8000);assert.equal(await b.evaluate(`${q('.term')}.dataset.demoStage`),'complete');await b.evaluate(`${q('[data-demo-replay]')}.click()`);await advance(data.end+9000);assert((await elapsed())<10000);report.checks.push('Complete record and final accounting, explicit paused completion, seven-second loop');
 assert.deepEqual(b.errors,[]);await writeFile(path.join(output,'report.json'),JSON.stringify(report,null,2));console.log('PASS terminal parity: '+report.checks.length+' groups');
}catch(error){await writeFile(path.join(output,'failure.json'),JSON.stringify({...report,error:error.stack,browserErrors:b.errors},null,2));throw error;}finally{b.close();}
