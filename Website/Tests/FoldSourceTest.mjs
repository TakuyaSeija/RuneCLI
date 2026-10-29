import assert from 'node:assert/strict';
import path from 'node:path';
import {openBrowser} from './Browser.mjs';
import {replayClock} from './ReplayClock.mjs';
const b=await openBrowser(path.resolve('target/test-tmp/website-replay-fold-source'));
try{
 await b.send('Page.addScriptToEvaluateOnNewDocument',{source:replayClock});await b.viewport(1366,768,'reduce');await b.navigate('http://127.0.0.1:18766/Studio/');await b.wait(100);await b.evaluate('__advanceReplay(0)');
 const result=await b.evaluate(`(()=>{let checked=0,largest=0,worstMs=0;for(const [fold,expected]of __recordedFolds){if(expected.length<8||!fold.closest('[data-step]'))continue;const content=fold.querySelector('[data-fold-rows]');if(content.children.length>=expected.length)throw new Error('closed preview mounts full source');const at=performance.now();fold.open=true;document.querySelector('[data-demo]').dispatchEvent(new CustomEvent('demo-disclosure-after',{detail:fold}));worstMs=Math.max(worstMs,performance.now()-at);const actual=[...content.children].map(n=>n.textContent);if(JSON.stringify(actual)!==JSON.stringify(expected))throw new Error('opened source differs');fold.open=false;document.querySelector('[data-demo]').dispatchEvent(new CustomEvent('demo-disclosure-after',{detail:fold}));if(content.children.length>=expected.length)throw new Error('refold did not release hidden source');checked++;largest=Math.max(largest,expected.length);if(checked>=12)break;}return{checked,largest,worstMs}})()`);
 assert(result.checked>=4&&result.largest>20,JSON.stringify(result));assert.deepEqual(b.errors,[]);console.log('PASS lossless virtual folds '+JSON.stringify(result));
}finally{b.close();}
