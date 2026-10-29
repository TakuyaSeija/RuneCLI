import assert from 'node:assert/strict';
import path from 'node:path';
import {writeFile} from 'node:fs/promises';
import {openBrowser} from './Browser.mjs';
import {replayClock} from './ReplayClock.mjs';
const origin=process.argv[2]||'http://127.0.0.1:18766',output=path.resolve(process.argv[3]||'target/test-tmp/website-replay-regressions');
const b=await openBrowser(output),report={checks:[],performance:{},samples:[]};
const q=s=>`document.querySelector(${JSON.stringify(s)})`,advance=ms=>b.evaluate(`__advanceReplay(${ms})`);
let rows,kinds,end;
const elapsed=()=>b.evaluate(`+${q('[data-demo]')}.dataset.elapsed`);
async function seek(at){if(at<await elapsed())await b.evaluate(`${q('[data-demo-replay]')}.click()`);let left=Math.max(0,at-await elapsed());while(left>0){const batch=Math.min(20000,left);await advance(batch);left-=batch;}}
const selector=i=>`[data-step="${i}"]`;
async function wheel(surface,delta){
 const p=await b.evaluate(`(()=>{const s=${q(surface)},r=s.getBoundingClientRect();window.__wheelDone=new Promise(resolve=>{const timer=setTimeout(()=>{s.removeEventListener('scrollend',done);resolve(false)},2000);const done=()=>{clearTimeout(timer);resolve(true)};s.addEventListener('scrollend',done,{once:true})});return{x:r.x+r.width*.6,y:r.y+r.height*.6}})()`);
 await b.send('Input.dispatchMouseEvent',{type:'mouseWheel',deltaX:0,deltaY:delta,...p});assert(await b.evaluate('__wheelDone'),'native wheel did not finish scrolling');
}
const spine=()=>b.evaluate(`(()=>{const all=[...document.querySelectorAll('[data-step]:not([hidden])')],last=all.at(-1);return{last:last?.dataset.step,connect:last?.dataset.connect,line:last&&getComputedStyle(last,'::before').display}})()`);
try{
 await b.send('Page.addScriptToEvaluateOnNewDocument',{source:replayClock});await b.viewport(1366,768,'no-preference');await b.navigate(origin+'/Studio/');await b.wait(100);
 // The session sits below the hero; its clock runs only while it is on screen.
 await b.evaluate(`${q('.stage')}.scrollIntoView({block:'center',behavior:'instant'})`);await b.wait(120);await advance(15000);
 ({rows,kinds,end}=await b.evaluate(`(()=>{const s=${q('[data-demo]')}.dataset.schedule.split('|');return{end:+s[1],rows:s[6].split(';').map(r=>r.split(',').map(Number)),kinds:__recordedSteps.map(e=>({kind:e.dataset.kind,call:e.dataset.call}))}})()`));
 assert.deepEqual(await spine(),{last:(await spine()).last,connect:'false',line:'none'});await b.capture('latest-step-no-tail.png');
 const unfinished=kinds.map((k,i)=>({k,i,r:rows[i]})).filter(v=>v.k.kind==='tool'&&v.r[5]>v.r[0]+80).slice(0,9);
 for(const {i,r} of unfinished){await seek(r[0]+(r[5]-r[0])*.5);const s=await spine();assert.equal(s.connect,'false',JSON.stringify(s));assert.equal(s.line,'none',JSON.stringify(s));}
 report.checks.push('1: newest activity has explicit false spine through argument, queued and running phases');
 // Sample within one long execution: accounting does not settle between these
 // frames, but Activity must move with the footer's source clock.
 const long=kinds.findIndex((k,i)=>k.call==='terminal_use'&&rows[i][7]>20000),lr=rows[long];await seek(lr[6]+700);
 const clock=()=>b.evaluate(`({activity:${q('[data-f=activity]')}.textContent.split(' · ')[0],footer:${q('[data-word]')}.textContent.split('[')[1]?.split(']')[0]})`);
 const c1=await clock();await advance(900);const c2=await clock();assert.notEqual(c1.activity,c2.activity);assert.equal(c1.activity,c1.footer);assert.equal(c2.activity,c2.footer);
 const counts=await b.evaluate(`({shown:document.querySelectorAll('[data-kind=tool]').length,label:+${q('[data-f=activity]')}.textContent.split(' · ')[1].split(' ')[0]})`);assert(counts.label>=counts.shown,JSON.stringify(counts)); // the recorded session's count, never the excerpt's
 report.checks.push('5: Activity advances during one request and equals the footer clock, not last accounting timestamp');
 // stdout arrives as whole lines/batches. No intermediate partial row should
 // ever differ from the final source line, unlike old char interpolation.
 const command=kinds.findIndex((k,i)=>k.call==='terminal_use'&&rows[i][7]>10000&&rows[i][7]<90000),r=rows[command];await seek(r[6]);
 const full=await b.evaluate(`[...${q(selector(command))}.querySelectorAll('.box-out .content-row:not(.tool-notice)')].map(e=>e.textContent)`);
 const changes=[];let previous=0;
 for(let i=0;i<24;i++){await advance((r[0]+r[1]-r[6])/25);const visible=await b.evaluate(`[...${q(selector(command))}.querySelectorAll('.box-out .content-row:not(.tool-notice)')].filter(e=>!e.hidden).map(e=>e.textContent)`);assert.deepEqual(visible,full.slice(0,visible.length));if(visible.length!==previous)changes.push(visible.length-previous);previous=visible.length;}
 assert(changes.some(n=>n>1),JSON.stringify(changes));report.samples.push({stdoutBatchSizes:changes});
 report.checks.push('2: terminal output emits full original row batches, never per-character typewriter output');
 // The next native wheel may have changed scrollTop before its scroll event.
 // Run the frame immediately after that mutation: stale anchor must not win.
 await seek(r[0]+r[1]-2);await b.evaluate(`${q('[data-jump]')}.click()`);await wheel('[data-log]',-150);
 const before=await b.evaluate(`${q('[data-log]')}.scrollTop`);await advance(10);const after=await b.evaluate(`${q('[data-log]')}.scrollTop`);assert(Math.abs(after-before)<2,JSON.stringify({before,after}));
 await wheel('[data-log]',999999);await advance(10);const bottom=await b.evaluate(`(()=>{const e=${q('[data-log]')};return e.scrollHeight-e.clientHeight-e.scrollTop})()`);assert(bottom<3,String(bottom));
 await b.evaluate(`${q('[data-log]')}.scrollTop=0;__advanceReplay(80)`);assert.equal(await b.evaluate(`${q('[data-log]')}.scrollTop`),0);
 await b.evaluate(`${q('[data-log]')}.scrollTop=${q('[data-log]')}.scrollHeight;__advanceReplay(80)`);assert(await b.evaluate(`(()=>{const e=${q('[data-log]')};return e.scrollHeight-e.clientHeight-e.scrollTop<3})()`));
 report.checks.push('3: native wheel and queued-scroll race preserve reader/bottom through box settlement');
 // Every result-derived note must be hidden before settle, including the
 // first binary inspect while the argument path is still empty.
 // The excerpt shows no binary read; that metadata rule is the server's
 // TestReadMetadataCannotMasqueradeAsArgumentText. Here: no result before run.
 await seek(r[0]+(r[5]-r[0])*.5);
 const early=await b.evaluate(`(()=>{const e=${q(selector(command))};return{resultVisible:[...e.querySelectorAll('[data-result-note],.plain-output,.box-out')].some(n=>!n.hidden&&n.offsetParent!==null&&n.innerText.trim()),text:e.innerText}})()`);assert(!early.resultVisible,JSON.stringify(early));assert(!early.text.includes('library renders'));
 await seek(r[0]+r[1]+2);assert(await b.evaluate(`${q(selector(command))}.innerText.includes('library renders')`));assert(!await b.evaluate(`${q(selector(command))}.innerText.includes('</tool_notice>')`));
 const prose=kinds.findIndex(k=>k.kind==='prose'),pr=rows[prose];
 const againAt=pr[0]+(pr[5]-pr[0])*.4;await seek(againAt);const p1text=await b.evaluate(`${q(selector(prose)+' [data-stream=text]')}.textContent`);await seek(pr[5]);const p2text=await b.evaluate(`${q(selector(prose)+' [data-stream=text]')}.textContent`);assert(p2text.trim().startsWith(p1text.trim())&&p2text.trim().length>p1text.trim().length,JSON.stringify({againAt,p1text,p2text,elapsed:await elapsed()}));
 report.checks.push('6,7: MIME/size/count results only after execution; no raw notice markup or future-result badge');
 // Real sidebar wheel at several timeline points while it grows. The smart
 // heading and marker must not shrink the scrolled document or rebound it.
 for(const at of [r[0]+r[1]+2,end*.7,end-5]){
  await seek(at);
  // Consume the reset's own scrollend first, so the wheel's is the one awaited.
  await b.evaluate(`new Promise(resolve=>{const s=${q('[data-side]')};if(s.scrollTop===0)return resolve();s.addEventListener('scrollend',()=>resolve(),{once:true});s.scrollTop=0;setTimeout(resolve,1000)})`);await wheel('[data-side]',999999);await advance(0);
  const check=()=>b.evaluate(`(()=>{const s=${q('[data-side]')};return{top:s.scrollTop,max:s.scrollHeight-s.clientHeight,height:s.clientHeight,head:${q('.files-head')}.offsetHeight}})()`);
  const a=await check();assert(a.max-a.top<3,JSON.stringify(a));await advance(150);await b.wait(60);const z=await check();assert(z.max-z.top<3,JSON.stringify(z));assert.equal(a.height,z.height);assert(a.head>0,'pinned heading must keep source geometry');
  await wheel('[data-side]',-100);const middle=await check();await advance(100);const held=await check();assert(Math.abs(middle.top-held.top)<2,JSON.stringify({middle,held}));
 }
 report.checks.push('4: sidebar bottom reachable by native wheel, held while replay grows, no heading/hint geometry rebound');
 // Performance measures the same page with full rendered history, then Replay.
 // This is a measured local budget, not a public product benchmark.
 await b.viewport(1366,768,'reduce');await b.navigate(origin+'/Studio/');await b.wait(100);await b.evaluate(`${q('.stage')}.scrollIntoView({block:'center',behavior:'instant'})`);await b.wait(120);
 const metrics=async()=>{const {metrics}=await b.send('Performance.getMetrics');return Object.fromEntries(metrics.map(m=>[m.name,m.value]));};await b.send('Performance.enable');
 const p0=await metrics();
 const replay=await b.evaluate(`(()=>{const start=performance.now();${q('[data-demo-replay]')}.click();return performance.now()-start})()`);
 const times=await b.evaluate(`(()=>{const times=[];for(let i=0;i<40;i++){const t=performance.now();__advanceReplay(16);times.push(performance.now()-t);}return times.sort((a,b)=>a-b)})()`);
 const p1=await metrics();report.performance={replayMs:replay,median:times[20],p95:times[38],max:times[39],layouts:(p1.LayoutCount||0)-(p0.LayoutCount||0),recalc:(p1.RecalcStyleCount||0)-(p0.RecalcStyleCount||0)};
 assert(report.performance.p95<16.7,JSON.stringify(report.performance));assert(replay<250,JSON.stringify(report.performance));
 await seek(end*.8);const late=await b.evaluate(`(()=>{const times=[];for(let i=0;i<30;i++){const t=performance.now();__advanceReplay(16);times.push(performance.now()-t);}return times.sort((a,b)=>a-b)})()`);report.performance.lateP95=late[28];assert(late[28]<25,JSON.stringify(report.performance));
 await b.evaluate(`${q('[data-log]')}.scrollTop=Math.max(0,${q('[data-log]')}.scrollTop-220)`);await b.wait(30);const reading=await b.evaluate(`(()=>{const times=[];for(let i=0;i<30;i++){const t=performance.now();__advanceReplay(16);times.push(performance.now()-t);}return times.sort((a,b)=>a-b)})()`);report.performance.readingP95=reading[28];assert(reading[28]<25,JSON.stringify(report.performance));
 report.checks.push('8: long-record Replay and late frames stay in measured local frame budget without scanning settled text');
 await b.capture('regression-fixed.png');assert.deepEqual(b.errors,[]);await writeFile(path.join(output,'report.json'),JSON.stringify(report,null,2));console.log('PASS replay regression '+report.checks.length+' groups '+JSON.stringify(report.performance));
}catch(error){await writeFile(path.join(output,'failure.json'),JSON.stringify({...report,error:error.stack,errors:b.errors},null,2));throw error;}finally{b.close();}
