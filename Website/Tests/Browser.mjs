import { spawn } from 'node:child_process';
import { mkdir, mkdtemp, writeFile } from 'node:fs/promises';
import path from 'node:path';

export async function openBrowser(output) {
  const temp = path.resolve('target/test-tmp');
  await mkdir(temp, { recursive:true }); await mkdir(output, { recursive:true });
  const profile = await mkdtemp(path.join(temp, 'terminal-chrome-'));
  let socket, serial = 0, log = '';
  const pending = new Map(), listeners = new Map(), errors = [];
  const browser = spawn(process.env.CHROME || 'google-chrome', ['--headless','--no-first-run','--no-default-browser-check','--disable-background-networking','--disable-component-update','--disable-sync','--disable-default-apps','--disable-dev-shm-usage','--remote-debugging-address=127.0.0.1','--remote-debugging-port=0',`--user-data-dir=${profile}`,'about:blank'], { stdio:['ignore','ignore','pipe'], env:{...process.env,TMPDIR:temp} });
  const url = await new Promise((resolve,reject) => {
    const timer = setTimeout(() => reject(new Error('No Chrome endpoint: '+log)), 20000);
    browser.once('error',reject); browser.once('exit',code=>reject(new Error('Chrome exited '+code)));
    browser.stderr.on('data',chunk => {log+=chunk;const match=log.match(/DevTools listening on (ws:\/\/[^\s]+)/);if(match){clearTimeout(timer);resolve(match[1]);}});
  });
  socket = new WebSocket(url);
  await new Promise((resolve,reject)=>{socket.addEventListener('open',resolve,{once:true});socket.addEventListener('error',reject,{once:true});});
  socket.addEventListener('message',event=>{
    const message=JSON.parse(event.data);
    if(message.id){const p=pending.get(message.id);if(!p)return;clearTimeout(p.timer);pending.delete(message.id);if(message.error)p.reject(new Error(JSON.stringify(message.error)));else p.resolve(message.result);}
    else for(const handler of listeners.get(message.method)||[])handler(message);
  });
  const raw=(method,params={},sessionId)=>new Promise((resolve,reject)=>{const id=++serial;const timer=setTimeout(()=>{pending.delete(id);reject(new Error('CDP timeout: '+method));},30000);pending.set(id,{resolve,reject,timer});socket.send(JSON.stringify({id,method,params,...(sessionId?{sessionId}:{})}));});
  const {targetId}=await raw('Target.createTarget',{url:'about:blank'});
  const {sessionId}=await raw('Target.attachToTarget',{targetId,flatten:true});
  const send=(method,params={})=>raw(method,params,sessionId);
  const evaluate=async expression=>{const r=await send('Runtime.evaluate',{expression,awaitPromise:true,returnByValue:true});if(r.exceptionDetails)throw new Error(JSON.stringify(r.exceptionDetails));return r.result.value;};
  const on=(name,handler)=>{if(!listeners.has(name))listeners.set(name,new Set());listeners.get(name).add(handler);return()=>listeners.get(name).delete(handler);};
  for(const name of ['Page','Runtime','Network','Log'])await send(name+'.enable');
  on('Runtime.exceptionThrown',m=>errors.push(m.params.exceptionDetails.text));
  on('Log.entryAdded',m=>{if(m.params.entry.level==='error')errors.push(m.params.entry.text);});
  const wait=ms=>new Promise(resolve=>setTimeout(resolve,ms));
  await send('Page.setLifecycleEventsEnabled',{enabled:true});
  const navigate=async (url,scripts=true)=>{
    let complete,fail;
    const loaded=new Promise((resolve,reject)=>{complete=resolve;fail=reject;});
    const timer=setTimeout(()=>fail(new Error('Navigation timeout: '+url)),20000);
    const offs=[on('Page.loadEventFired',()=>complete()),on('Page.navigatedWithinDocument',()=>complete()),on('Page.lifecycleEvent',e=>{if(e.params.name==='load')complete();})];
    try{const answer=await send('Page.navigate',{url});if(answer.errorText)throw new Error(answer.errorText);if(!answer.loaderId)complete();await loaded;}
    catch(error){const facts=await evaluate('({url:location.href,ready:document.readyState,resources:performance.getEntriesByType("resource").length})');throw new Error(error.message+' '+JSON.stringify(facts));}
    finally{clearTimeout(timer);for(const off of offs)off();}
    if(scripts)await evaluate('document.fonts.ready.then(()=>true)');
  };
  const viewport=async(width,height=900,motion='reduce')=>{await send('Emulation.setDeviceMetricsOverride',{width,height,deviceScaleFactor:1,mobile:false});await send('Emulation.setEmulatedMedia',{features:[{name:'prefers-reduced-motion',value:motion}]});};
  const capture=async name=>{const {data}=await send('Page.captureScreenshot',{format:'png',captureBeyondViewport:false});await writeFile(path.join(output,name),Buffer.from(data,'base64'));};
  const click=async selector=>{await evaluate(`document.querySelector(${JSON.stringify(selector)}).scrollIntoView({block:'center',behavior:'instant'})`);const p=await evaluate(`(()=>{const r=document.querySelector(${JSON.stringify(selector)}).getBoundingClientRect();return{x:r.x+r.width/2,y:r.y+r.height/2}})()`);await send('Input.dispatchMouseEvent',{type:'mousePressed',button:'left',clickCount:1,...p});await send('Input.dispatchMouseEvent',{type:'mouseReleased',button:'left',clickCount:1,...p});};
  const close=()=>{for(const p of pending.values())clearTimeout(p.timer);socket.close();browser.kill('SIGTERM');};
  return{send,evaluate,on,wait,navigate,viewport,capture,click,errors,close};
}
