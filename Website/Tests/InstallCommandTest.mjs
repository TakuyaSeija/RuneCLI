import assert from 'node:assert/strict';
import path from 'node:path';
import {readFile} from 'node:fs/promises';
import {openBrowser} from './Browser.mjs';
const origin=process.argv[2]||'http://127.0.0.1:18766';
const fixture=process.argv[3]||'target/test-tmp/website-natural-published';
const b=await openBrowser(path.resolve('target/test-tmp/website-natural-command'));
const longURL='https://downloads.preview.runeagent.atoryn.example/Studio/install.sh';
const command='curl -fsSL '+longURL+' | sh';
const published=(await readFile(path.join(fixture,'home.html'),'utf8')).replaceAll('http://127.0.0.1:18766/Studio/install.sh',longURL);
try{
 await b.send('Fetch.enable',{patterns:[{urlPattern:origin+'/Studio/',requestStage:'Request'}]});
 const off=b.on('Fetch.requestPaused',event=>{
  const {requestId}=event.params;b.send('Fetch.fulfillRequest',{requestId,responseCode:200,responseHeaders:[{name:'Content-Type',value:'text/html; charset=utf-8'}],body:Buffer.from(published).toString('base64')});
 });
 await b.viewport(1024,768,'reduce');await b.navigate(origin+'/Studio/');
 assert(await b.evaluate(`document.querySelector('.install-note').textContent.includes('Release 0.9.1')`));
 assert.equal(await b.evaluate(`document.querySelector('#install-cmd').textContent`),command);
 await b.evaluate(`window.__copied='';Object.defineProperty(navigator,'clipboard',{configurable:true,value:{writeText:text=>{window.__copied=text;return Promise.resolve()}}})`);
 await b.click('[data-copy]');assert.equal(await b.evaluate('__copied'),command);
 // Use Chrome's own settings API in the disposable test profile, not CSS
 // zoom, pinch emulation, an extension or the owner's browser preferences.
 for(const zoom of [1,1.25,1.5,2]){
  await b.navigate('chrome://settings/appearance');
  await b.evaluate(`new Promise(resolve=>chrome.settingsPrivate.setDefaultZoom(${zoom},resolve))`);
  await b.viewport(1366,768,'reduce');await b.navigate(origin+'/Studio/');
  const r=await b.evaluate(`(()=>{const e=document.querySelector('#install-cmd');return{width:innerWidth,dpr:devicePixelRatio,scroll:e.scrollWidth,client:e.clientWidth,document:document.documentElement.scrollWidth}})()`);
  assert(Math.abs(r.dpr-zoom)<.01&&Math.abs(r.width-1366/zoom)<2,JSON.stringify(r));assert(r.scroll<=r.client+1&&r.document<=r.width,JSON.stringify(r));
 }
 await b.navigate('chrome://settings/appearance');await b.evaluate(`new Promise(resolve=>chrome.settingsPrivate.setDefaultZoom(1,resolve))`);
 for(const width of [320,390,768,1024,1366]){
  await b.viewport(width,768,'reduce');await b.navigate(origin+'/Studio/');
  const result=await b.evaluate(`(()=>{const e=document.querySelector('#install-cmd'),r=e.getBoundingClientRect(),range=document.createRange();range.selectNodeContents(e);return{scroll:e.scrollWidth,client:e.clientWidth,doc:document.documentElement.scrollWidth,width:innerWidth,all:[...range.getClientRects()].every(q=>q.right<=r.right+1&&q.bottom<=r.bottom+1)}})()`);
  assert(result.scroll<=result.client+1&&result.doc<=result.width&&result.all,JSON.stringify(result));
 }
 await b.capture('published-long-command.png');off();await b.send('Fetch.disable');
 await b.navigate(origin+'/Studio/');assert(await b.evaluate(`document.querySelector('.os-linux .install-note').textContent.includes('without installing anything')`));
 assert.deepEqual(b.errors,[]);console.log('PASS command: signed published fixture, unpublished warning, long public URL five widths, actual Chrome125–200% zoom, exact clipboard');
}finally{b.close();}
