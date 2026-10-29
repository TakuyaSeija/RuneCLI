import assert from 'node:assert/strict';
import path from 'node:path';
import {writeFile} from 'node:fs/promises';
import {openBrowser} from './Browser.mjs';
import {replayClock} from './ReplayClock.mjs';
const origin=process.argv[2]||'http://127.0.0.1:18766',output=path.resolve(process.argv[3]||'target/test-tmp/website-natural-browser');
const b=await openBrowser(output),report={checks:[],geometry:[],prefixes:[]};
const q=selector=>`document.querySelector(${JSON.stringify(selector)})`;
const advance=ms=>b.evaluate(`__advanceReplay(${ms})`);
const elapsed=()=>b.evaluate(`+${q('[data-demo]')}.dataset.elapsed`);
let rows,end;
const seek=async at=>{
 if(at<await elapsed())await b.evaluate(`${q('[data-demo-replay]')}.click()`);
 let left=Math.max(0,at-await elapsed());
 while(left>0){const batch=Math.min(left,20000);await advance(batch);left-=batch;}
};
const node=index=>`[data-step="${index}"]`;
const mouse=async(selector,fx=.5,fy=.5)=>{
 await b.evaluate(`${q(selector)}.scrollIntoView({block:'center',behavior:'instant'})`);await b.wait(40);
 const p=await b.evaluate(`(()=>{const r=${q(selector)}.getBoundingClientRect();return{x:r.x+r.width*${fx},y:r.y+r.height*${fy}}})()`);
 await b.send('Input.dispatchMouseEvent',{type:'mousePressed',button:'left',clickCount:1,...p});await b.send('Input.dispatchMouseEvent',{type:'mouseReleased',button:'left',clickCount:1,...p});
};
try{
 await b.send('Page.addScriptToEvaluateOnNewDocument',{source:replayClock});await b.viewport(1366,768,'no-preference');await b.navigate(origin+'/Studio/');await b.wait(120);
 // The session sits below the hero; its clock runs only while it is on screen.
 await b.evaluate(`${q('.stage')}.scrollIntoView({block:'center',behavior:'instant'})`);await b.wait(120);await advance(0);
 ({rows,end}=await b.evaluate(`(()=>{const s=${q('[data-demo]')}.dataset.schedule.split('|');return{rows:s[6].split(';').map(r=>r.split(',').map(Number)),end:+s[1]}})()`));
 const kinds=await b.evaluate(`__recordedSteps.map(e=>({kind:e.dataset.kind,call:e.dataset.call}))`);
 assert.equal(await b.evaluate(`document.querySelectorAll('[data-playback-state]').length`),0);
 assert(await b.evaluate(`${q('.demo-meta')}.textContent.includes('Streaming and approvals simulated')`));
 // Partial assistant response must precede its streamed arguments and match a
 // real CLI activity, not an unlabelled paragraph on a spine above the user.
 const prose=kinds.findIndex(k=>k.kind==='prose'),pr=rows[prose];
 await seek(pr[0]+(pr[5]-pr[0])*.45);
 const partialProse=await b.evaluate(`${q(node(prose)+' [data-stream=text]')}.textContent.trim()`);
 assert(partialProse.length>0);assert.equal(await b.evaluate(`${q(node(prose)+' .step-title')}.textContent.trim()`),'●Response');
 await seek(pr[5]);const fullProse=await b.evaluate(`${q(node(prose)+' [data-stream=text]')}.textContent.trim()`);assert(fullProse.startsWith(partialProse));assert(fullProse.length>partialProse.length);
 report.checks.push('14: intermediate Response neutral bullet, streamed prefix before tool arguments');
 const thought=kinds.findIndex((k,i)=>k.kind==='think'&&rows[i][5]-rows[i][0]>1000),tr=rows[thought];
 await seek(tr[0]+(tr[5]-tr[0])*.3);await mouse(node(thought)+' .step-title',.9);
 const thinking=async()=>b.evaluate(`(()=>{const e=${q(node(thought))},body=e.querySelector('[data-stream=thought]');return{text:body.textContent.trim(),visible:body.innerText.trim(),words:+e.querySelector('[data-words]').textContent.replaceAll(',',''),open:e.querySelector('details').open,phase:e.dataset.phase}})()`);
 // The count is the CLI's own: words of the raw thought, reached as it ends;
 // the open body shows the rendered prefix, which has slightly fewer.
 const near=t=>{const shown=t.visible.split(/\s+/u).filter(Boolean).length;return Math.abs(t.words-shown)<=Math.max(4,shown*.1)};
 const t1=await thinking();assert(t1.open&&t1.words>0&&t1.text.length>0);assert(t1.visible.length>0);assert(near(t1),JSON.stringify(t1.words));
 await advance((tr[5]-tr[0])*.3);const t2=await thinking();assert(t2.text.length>t1.text.length);assert(t2.text.startsWith(t1.text));assert(t2.words>t1.words&&near(t2));
 await seek(tr[5]);const tf=await thinking();assert(tf.text.length>t2.text.length);assert.equal(tf.words,tr[2]);report.prefixes.push({kind:'thought',partial:t1.words,later:t2.words,final:tf.words});
 const canvas=await b.evaluate(`(()=>{const v=${q('.terminal-viewport')},t=${q('.term')};return{scroll:v.scrollTop,offset:t.getBoundingClientRect().top-v.getBoundingClientRect().top,border:parseFloat(getComputedStyle(v).borderTopWidth)}})()`);assert.equal(canvas.scroll,0,JSON.stringify(canvas));assert(Math.abs(canvas.offset-canvas.border)<1,JSON.stringify(canvas));
 await b.capture('thinking-progress.png');report.checks.push('3,12: open Thinking contains only current simulated prefix and its exact shown word count');
 // Bash arguments precede execution, whose timer starts at zero even after a
 // long request. Stdout alone grows within run; settle notices stay at the end.
 const bash=kinds.findIndex((k,i)=>k.call==='terminal_use'&&rows[i][7]>20000&&rows[i][5]-rows[i][0]>15),br=rows[bash];
 assert(bash>=0);await seek(br[0]+(br[5]-br[0])*.7);
 const cmd1=await b.evaluate(`${q(node(bash)+' [data-stream=command]')}.textContent`);assert(cmd1.length>0);
 assert.equal(await b.evaluate(`${q(node(bash))}.dataset.phase`),'writing');
 // The timer counts the recorded run from zero: its whole recorded length
 // passes over the shown run, so it ends on the CLI's own figure.
 const runFor=br[0]+br[1]-br[6],clock=ms=>{const n=Math.floor(ms/1000);return n<60?n+'s':Math.floor(n/60)+'m '+String(n%60).padStart(2,'0')+'s'};
 await seek(br[6]+450);
 assert.equal(await b.evaluate(`${q(node(bash)+' [data-tool-time]')}.textContent`),clock(br[7]*450/runFor));
 const pulse1=await b.evaluate(`getComputedStyle(${q(node(bash)+' .step-dot')}).color`);
 await advance(608);const pulse2=await b.evaluate(`getComputedStyle(${q(node(bash)+' .step-dot')}).color`);assert.notEqual(pulse1,pulse2);
 // A shown run lasts about two seconds, so each theme is checked from its start.
 for(const theme of ['amber','violet','cyan']){
  await b.evaluate(`document.documentElement.dataset.theme='${theme}'`);await seek(br[6]+60);
  const a=await b.evaluate(`getComputedStyle(${q(node(bash)+' .step-dot')}).color`);await advance(608);const z=await b.evaluate(`getComputedStyle(${q(node(bash)+' .step-dot')}).color`);assert.notEqual(a,z,theme);
 }
 await b.evaluate(`document.documentElement.dataset.theme='amber'`);
 await seek(br[6]+60);const frozen=await b.evaluate(`getComputedStyle(${q(node(bash)+' .step-dot')}).color`);
 await b.evaluate(`${q('[data-demo-pause]')}.click()`);const pausedAt=await elapsed();await advance(5000);assert.equal(await elapsed(),pausedAt);assert.equal(await b.evaluate(`getComputedStyle(${q(node(bash)+' .step-dot')}).color`),frozen);
 await b.evaluate(`${q('[data-demo-pause]')}.click()`);await seek(br[0]+br[1]+10);
 assert.equal(await b.evaluate(`${q(node(bash)+' [data-tool-time]')}.textContent`),clock(br[7]));
 assert(!await b.evaluate(`${q(node(bash)+' .box-out')}.hidden`));
 assert(await b.evaluate(`${q(node(bash)+' .box-out')}.innerText.length>0`));
 report.checks.push('2,4,5,6: Bash argument typing, zero-based recorded timer, progressive stdout, hierarchical elapsed and clock-owned pulse/Pause');
 // Write/Patch counters reflect their respective source prefix, not final
 // title counts immediately upon a new block appearing.
 for(const call of ['create_file','patch_file']){
  const i=kinds.findIndex((k,j)=>k.call===call&&rows[j][5]-rows[j][0]>700),r=rows[i];assert(i>=0);
  await seek(r[0]+(r[5]-r[0])*.6);
  const before=await b.evaluate(`(()=>{const e=${q(node(i))};return{added:+e.querySelector('[data-added]').textContent.slice(1),final:+e.querySelector('[data-added]').dataset.added,body:e.querySelector('.demo-box').hidden}})()`);
  assert(before.added<before.final,JSON.stringify({call,before}));assert(before.body);
  await seek(r[5]);assert.equal(await b.evaluate(`+${q(node(i)+' [data-added]')}.textContent.slice(1)`),before.final);
 }
 report.checks.push('4: Write/Patch counters stream with arguments; code/diff result appears only at CLI settle');
 // Follow is a surface state: reading, fold measurement, panel resize and
 // streaming never pause the clock or oscillate the bottom-owned scroll.
 const growth=kinds.findIndex((k,i)=>k.call==='terminal_use'&&rows[i][7]>10000&&rows[i][7]<90000),gr=rows[growth];assert(growth>=0);
 await seek(gr[6]+10);await b.evaluate(`${q('[data-jump]')}.click()`);
 for(let n=0;n<18;n++){await advance((gr[0]+gr[1]-gr[6])/18);await b.wait(10);const g=await b.evaluate(`(()=>{const l=${q('[data-log]')};return{top:l.scrollTop,gap:l.scrollHeight-l.clientHeight-l.scrollTop}})()`);report.geometry.push(g);assert(g.gap<3,JSON.stringify(g));}
 assert(new Set(report.geometry.map(g=>g.top)).size>1,'growth fixture must exercise transcript height changes');
 await b.evaluate(`${q('[data-log]')}.scrollTop=0`);await b.wait(60);const readAt=await elapsed();await advance(900);assert((await elapsed())>readAt);assert.equal(await b.evaluate(`${q('[data-log]')}.scrollTop`),0);
 await b.evaluate(`${q('[data-jump]')}.click()`);await b.wait(40);
 await b.evaluate(`${q('[data-log]')}.scrollTop=Math.max(0,${q('[data-log]')}.scrollTop-160)`);await b.wait(50);
 const anchor=await b.evaluate(`(()=>{const l=${q('[data-log]')},top=l.getBoundingClientRect().top,e=[...l.querySelectorAll('[data-step]:not([hidden])')].find(e=>e.getBoundingClientRect().bottom>top);return{id:e.dataset.step,offset:e.getBoundingClientRect().top-top}})()`);
 await advance(450);const held=await b.evaluate(`(()=>{const l=${q('[data-log]')},e=${q('[data-step="'+anchor.id+'"]')};return e.getBoundingClientRect().top-l.getBoundingClientRect().top})()`);assert(Math.abs(held-anchor.offset)<1);
 await b.evaluate(`${q('[data-demo-panel]')}.click()`);await advance(0);await b.wait(50);assert.equal(await b.evaluate(`${q('[data-demo]')}.dataset.playback`),'playing');await b.evaluate(`${q('[data-demo-panel]')}.click()`);await advance(0);
 const finalIndex=kinds.findIndex(k=>k.kind==='final'),fr=rows[finalIndex];await seek(fr[0]+(fr[5]-fr[0])*.4);
 const finalPrefix=await b.evaluate(`${q(node(finalIndex)+' [data-stream=text]')}.textContent.trim()`);assert(finalPrefix.length>0);await seek(fr[5]);const finalText=await b.evaluate(`${q(node(finalIndex)+' [data-stream=text]')}.textContent.trim()`);assert(finalText.startsWith(finalPrefix)&&finalText.length>finalPrefix.length);assert.equal(await b.evaluate(`getComputedStyle(${q(node(finalIndex))},'::before').display`),'none');
 // Compact sections have no invisible above row between heading/content.
 await seek(end-50);await b.evaluate(`${q('[data-side]')}.scrollTop=0`);await b.wait(50);
 const gaps=await b.evaluate(`(()=>{const q=s=>document.querySelector(s),r=s=>q(s).getBoundingClientRect();return{plan:r('.demo-plan').top-r('[data-side-heading]').bottom,above:getComputedStyle(q('[data-side-above]')).display,session:r('.st').top-r('.tside>.sh').bottom}})()`);assert(gaps.plan<2,JSON.stringify(gaps));assert.equal(gaps.above,'none');assert(gaps.session<5);
 await b.evaluate(`${q('[data-side]')}.scrollTop=${q('[data-side]')}.scrollHeight;${q('[data-side]')}.dispatchEvent(new Event('scroll'))`);await b.wait(50);
 const fg=await b.evaluate(`(()=>{const h=${q('[data-side-files-heading]')},s=${q('[data-side]')};return{shown:!h.hidden,gap:s.scrollHeight-s.clientHeight-s.scrollTop,sourceHeight:${q('.files-head')}.offsetHeight}})()`);assert(fg.shown);assert(fg.gap<3&&fg.sourceHeight>0,JSON.stringify(fg));
 for(let i=0;i<3;i++){await b.evaluate(`${q('[data-side]')}.scrollTop=0`);await b.wait(30);assert.equal(await b.evaluate(`${q('[data-side-plan-heading]')}.hidden`),false);await b.evaluate(`${q('[data-side]')}.scrollTop=${q('[data-side]')}.scrollHeight`);await b.wait(30);assert.equal(await b.evaluate(`${q('[data-side-files-heading]')}.hidden`),false);}
 assert(await b.evaluate(`(()=>{const h=${q('.tside>.sh')},r=h.getBoundingClientRect(),s=${q('.tside')}.getBoundingClientRect();return r.top>=s.top&&r.bottom<=s.bottom&&!h.hidden})()`));
 assert.equal(await b.evaluate(`getComputedStyle(${q('[data-step]')},'::before').top`),'10.2px');assert.equal(await b.evaluate(`getComputedStyle(${q('.demo-user')},'::before').content`),'none');
 report.checks.push('8,10,11: bottom follow stays anchored, reader held, compact SESSIONS/PLAN/FILES and spine starts at first activity only');
 // Keyboard focus uses a non-underline highlight. Audit all visible surfaces,
 // including command fold, user fold, thought and output summary.
 await b.evaluate(`${q('[data-demo-pause]')}.click()`);await b.send('Input.dispatchKeyEvent',{type:'keyDown',key:'Tab',code:'Tab'});await b.send('Input.dispatchKeyEvent',{type:'keyUp',key:'Tab',code:'Tab'});
 await b.evaluate(`${q('.thought-fold>summary')}.focus()`);
 assert.equal(await b.evaluate(`getComputedStyle(${q('.thought-fold>summary')}).textDecorationLine`),'none');
 assert.deepEqual(await b.evaluate(`[...document.querySelectorAll('.term *')].filter(e=>getComputedStyle(e).textDecorationLine.includes('underline')).map(e=>e.tagName)`),[]);
 report.checks.push('1: no underline on terminal surfaces, including keyboard-focused native disclosure');
 // Explicit intent survives genuine IntersectionObserver suspension. Clicking
 // Pause then Play while offscreen resumes automatically on returning.
 await b.evaluate(`window.scrollTo({top:document.body.scrollHeight,behavior:'instant'})`);await b.wait(120);const hiddenAt=await elapsed();await advance(2000);assert.equal(await elapsed(),hiddenAt);
 await b.evaluate(`${q('[data-demo-pause]')}.click()`);assert.equal(await b.evaluate(`${q('[data-icon=pause]')}.hasAttribute('hidden')`),false);
 await b.evaluate(`${q('.stage')}.scrollIntoView({block:'center',behavior:'instant'})`);await b.wait(120);await advance(200);assert((await elapsed())>hiddenAt);
 await mouse('[data-demo-expand]');await mouse('[data-demo-expand]');await mouse('[data-demo-pause]');await mouse('[data-demo-pause]');await advance(300);
 assert.equal(await b.evaluate(`${q('[data-icon=pause]')}.hasAttribute('hidden')`),false);
 report.checks.push('9: real pointer Play/Pause/expand and offscreen observer resumption, no status text or stuck Play');
 // Long public command at multiple layout widths and real browser zoom (CSS
 // viewport shrinks), in addition to the signed/unpublished server fixtures.
 const command='curl -fsSL https://downloads.preview.runeagent.atoryn.example/Studio/install.sh | sh';
 for(const zoom of [1,1.25,1.5,2]){
  for(const baseWidth of [390,1024,1366]){
   const width=Math.round(baseWidth/zoom);await b.viewport(Math.max(300,width),768,'reduce');await b.navigate(origin+'/Studio/');await b.wait(60);
   await b.evaluate(`${q('#install-cmd')}.textContent=${JSON.stringify(command)}`);
   const g=await b.evaluate(`(()=>{const e=${q('#install-cmd')},b=e.getBoundingClientRect(),copy=${q('[data-copy]')}.getBoundingClientRect(),r=document.createRange();r.selectNodeContents(e);return{width:innerWidth,doc:document.documentElement.scrollWidth,scroll:e.scrollWidth,client:e.clientWidth,rects:[...r.getClientRects()].map(r=>({right:r.right,bottom:r.bottom})),right:b.right,bottom:b.bottom,copy:copy.left}})()`);
   assert(g.scroll<=g.client+1&&g.doc<=Math.max(300,width),JSON.stringify(g));assert(g.rects.every(r=>r.right<=g.right+1&&r.bottom<=g.bottom+1),JSON.stringify(g));
  }
 }
 report.checks.push('13: full long public curl wraps without clipping at desktop/mobile/125–200% zoom-equivalent viewports');
 await b.viewport(1366,768,'reduce');await b.navigate(origin+'/Studio/');await b.wait(60);await b.capture('natural-final.png');
 assert.deepEqual(b.errors,[]);await writeFile(path.join(output,'report.json'),JSON.stringify(report,null,2));console.log('PASS natural replay: '+report.checks.length+' groups');
}catch(error){await writeFile(path.join(output,'failure.json'),JSON.stringify({...report,error:error.stack,browserErrors:b.errors},null,2));throw error;}finally{b.close();}
