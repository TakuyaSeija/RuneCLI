import assert from 'node:assert/strict';
import path from 'node:path';
import {writeFile} from 'node:fs/promises';
import {openBrowser} from './Browser.mjs';
const origin=process.argv[2]||'http://127.0.0.1:18766',output=path.resolve(process.argv[3]||'target/test-tmp/website-replay-real-performance');
const b=await openBrowser(output),report={runs:[]};
try{
 await b.viewport(1366,768,'reduce');await b.navigate(origin+'/Studio/');
 assert.equal(await b.evaluate(`document.querySelectorAll('[data-step]').length`),await b.evaluate(`document.querySelector('[data-demo]').dataset.schedule.split('|')[6].split(';').length`));
 await b.evaluate(`document.querySelector('.stage').scrollIntoView({block:'center',behavior:'instant'})`);
 for(let run=0;run<3;run++){
  const result=await b.evaluate(`new Promise(resolve=>{
    const root=document.querySelector('[data-demo]'),start=performance.now();document.querySelector('[data-demo-replay]').click();const reset=performance.now()-start;
    const gaps=[],long=[];let last=null;const first=performance.now();
    const observer=new PerformanceObserver(list=>{for(const e of list.getEntries())long.push(e.duration)});observer.observe({entryTypes:['longtask']});
    const tick=now=>{if(last!==null)gaps.push(now-last);last=now;if(now-first<1800){requestAnimationFrame(tick);return;}observer.disconnect();gaps.sort((a,b)=>a-b);resolve({resetMs:reset,frames:gaps.length,median:gaps[Math.floor(gaps.length/2)],p95:gaps[Math.floor(gaps.length*.95)],longTasks:long,elapsed:+root.dataset.elapsed,dom:document.querySelectorAll('.term *').length})};requestAnimationFrame(tick);
  })`);
  report.runs.push(result);assert(result.frames>30&&result.elapsed>1200,JSON.stringify(result));assert(result.p95<80&&result.resetMs<250,JSON.stringify(result));
 }
 assert.deepEqual(b.errors,[]);await writeFile(path.join(output,'report.json'),JSON.stringify(report,null,2));console.log('PASS real-clock Replay performance: '+JSON.stringify(report.runs));
}catch(error){await writeFile(path.join(output,'failure.json'),JSON.stringify({...report,error:error.stack},null,2));throw error;}finally{b.close();}
