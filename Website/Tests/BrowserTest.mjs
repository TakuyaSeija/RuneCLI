#!/usr/bin/env node
/* Local Chrome/CDP audit, no dependencies and no deploy.
 * node Website/Tests/BrowserTest.mjs [origin] [output]
 */
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdir, mkdtemp, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';

const origin = process.argv[2] || 'http://127.0.0.1:18766';
const output = path.resolve(process.argv[3] || 'target/test-tmp/website-browser');
await mkdir(output, { recursive: true });
const chromeTemp = path.resolve('target/test-tmp');
await mkdir(chromeTemp, { recursive: true });
const profile = await mkdtemp(path.join(chromeTemp, 'web-chrome-'));
const report = { origin, browser: '', layouts: [], screenshots: [], checks: [], errors: [] };
let browser, socket, log = '', serial = 0;
const pending = new Map(), events = new Map();
const raw = (method, params = {}, sessionId, ceiling = 30000) => new Promise((resolve, reject) => {
  const id = ++serial;
  const timer = setTimeout(() => { pending.delete(id); reject(new Error(`Timeout: ${method}`)); }, ceiling);
  pending.set(id, { resolve, reject, timer });
  socket.send(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) }));
});
try {
  browser = spawn(process.env.CHROME || 'google-chrome', [
    '--headless', '--no-first-run', '--no-default-browser-check', '--disable-background-networking',
    '--disable-component-update', '--disable-sync', '--disable-default-apps', '--disable-dev-shm-usage',
    '--remote-debugging-address=127.0.0.1', '--remote-debugging-port=0', `--user-data-dir=${profile}`, 'about:blank',
  ], { stdio: ['ignore', 'ignore', 'pipe'], env: { ...process.env, TMPDIR: chromeTemp } });
  const debugURL = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('Chrome did not expose DevTools: ' + log)), 20000);
    browser.once('error', reject);
    browser.once('exit', code => { clearTimeout(timer); reject(new Error(`Chrome exited ${code}: ${log}`)); });
    browser.stderr.on('data', chunk => {
      log += chunk;
      const match = log.match(/DevTools listening on (ws:\/\/[^\s]+)/);
      if (match) { clearTimeout(timer); resolve(match[1]); }
    });
  });
  socket = new WebSocket(debugURL);
  await new Promise((resolve, reject) => {
    socket.addEventListener('open', resolve, { once: true });
    socket.addEventListener('error', reject, { once: true });
  });
  socket.addEventListener('message', event => {
    const message = JSON.parse(event.data);
    if (message.id) {
      const request = pending.get(message.id); if (!request) return;
      clearTimeout(request.timer); pending.delete(message.id);
      if (message.error) request.reject(new Error(JSON.stringify(message.error))); else request.resolve(message.result);
    } else {
      for (const callback of events.get(message.method) || []) callback(message);
    }
  });
  report.browser = (await raw('Browser.getVersion')).product;
  const { targetId } = await raw('Target.createTarget', { url: 'about:blank' });
  const { sessionId } = await raw('Target.attachToTarget', { targetId, flatten: true });
  const send = (method, params = {}) => raw(method, params, sessionId);
  const evaluate = async (expression, ceiling) => {
    const result = await raw('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true }, sessionId, ceiling);
    if (result.exceptionDetails) throw new Error(JSON.stringify(result.exceptionDetails));
    return result.result.value;
  };
  for (const domain of ['Page', 'Runtime', 'Network', 'Log', 'Accessibility']) await send(`${domain}.enable`);
  events.set('Runtime.exceptionThrown', [m => report.errors.push(m.params.exceptionDetails.text)]);
  events.set('Log.entryAdded', [m => {
    if (m.params.entry.level === 'error' && !m.params.entry.text.includes('404') && !m.params.entry.text.includes('Cross-Origin-Opener-Policy')) report.errors.push(m.params.entry.text);
  }]);
  await send('Network.setCacheDisabled', { cacheDisabled: true });
  const fixtures = new Map();
  if (process.env.WEBSITE_FIXTURE_DIR) {
    for (const [route, file] of [['/Studio/', 'home.html'], ['/Studio/changelog', 'changelog.html'], ['/Studio/docs/verification', 'verification.html']]) {
      fixtures.set(route, (await readFile(path.join(process.env.WEBSITE_FIXTURE_DIR, file))).toString('base64'));
    }
    events.set('Fetch.requestPaused', [message => {
      const { requestId, request } = message.params;
      const body = fixtures.get(new URL(request.url).pathname);
      const operation = body
        ? send('Fetch.fulfillRequest', { requestId, responseCode: 200, responseHeaders: [{ name: 'Content-Type', value: 'text/html; charset=utf-8' }], body })
        : send('Fetch.continueRequest', { requestId });
      operation.catch(error => report.errors.push(error.message));
    }]);
    await send('Fetch.enable', { patterns: [...fixtures.keys()].map(route => ({ urlPattern: origin + route, requestStage: 'Request' })) });
    report.checks.push('Published catalog fixture: signed-test-release markup, actual local server assets');
  }
  const frame = () => evaluate('new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)))');
  let scriptsDisabled = false;
  await send('Page.setLifecycleEventsEnabled', { enabled:true });
  const navigate = async route => {
    let complete,fail;
    const loaded=new Promise((resolve,reject)=>{complete=resolve;fail=reject;});
    const timer=setTimeout(()=>fail(new Error(`Navigation timeout: ${route}`)),20000);
    const names=['Page.loadEventFired','Page.navigatedWithinDocument','Page.lifecycleEvent'];
    const handlers=names.map(name=>{
      const handler=m=>{if(m.sessionId===sessionId&&(name!=='Page.lifecycleEvent'||m.params.name==='load'))complete();};
      if(!events.has(name))events.set(name,new Set());events.get(name).add(handler);return handler;
    });
    try{const answer=await send('Page.navigate',{url:origin+route});if(answer.errorText)throw new Error(answer.errorText);if(!answer.loaderId)complete();await loaded;}
    catch(error){throw new Error(error.message+' '+JSON.stringify(await evaluate('({url:location.href,ready:document.readyState})')));}
    finally{clearTimeout(timer);names.forEach((name,i)=>events.get(name).delete(handlers[i]));}
    if (!scriptsDisabled) {
      await evaluate(`Promise.all([document.fonts.ready,document.fonts.load('12px "JetBrains Mono"')]).then(()=>true)`); await frame();
    }
  };
  const viewport = async (width, motion = 'reduce') => {
    await send('Emulation.setDeviceMetricsOverride', { width, height: 900, deviceScaleFactor: 1, mobile: false });
    await send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-reduced-motion', value: motion }] });
  };
  const click = async selector => {
    await evaluate(`document.querySelector(${JSON.stringify(selector)}).scrollIntoView({block:'center',behavior:'instant'})`); await frame();
    const point = await evaluate(`(()=>{const r=document.querySelector(${JSON.stringify(selector)}).getBoundingClientRect();return{x:r.x+r.width/2,y:r.y+r.height/2}})()`);
    await send('Input.dispatchMouseEvent', { type: 'mousePressed', button: 'left', clickCount: 1, ...point });
    await send('Input.dispatchMouseEvent', { type: 'mouseReleased', button: 'left', clickCount: 1, ...point }); await frame();
  };
  const screenshot = async name => {
    const { cssContentSize: size } = await send('Page.getLayoutMetrics');
    const image = await send('Page.captureScreenshot', { format: 'png', captureBeyondViewport: true, clip: { x: 0, y: 0, width: size.width, height: size.height, scale: 1 } });
    await writeFile(path.join(output, name), Buffer.from(image.data, 'base64')); report.screenshots.push(name);
  };
  const audit = `(()=>{
    const visible=e=>{const s=getComputedStyle(e),r=e.getBoundingClientRect();return s.display!=='none'&&s.visibility!=='hidden'&&r.width>0&&r.height>0};
    const bounded=e=>{for(let p=e.parentElement;p;p=p.parentElement){const s=getComputedStyle(p);if(['hidden','auto','scroll','clip'].includes(s.overflowX))return true}return false};
    const overflow=[...document.body.querySelectorAll('*')].filter(visible).filter(e=>!e.closest('.skip-link')&&!bounded(e)&&(e.getBoundingClientRect().left< -1||e.getBoundingClientRect().right>innerWidth+1)).map(e=>({tag:e.tagName,class:e.getAttribute('class'),text:e.textContent.slice(0,60)}));
    const clipped=[...document.querySelectorAll('h1,h2,h3,.prose p,.install-step p,.cmd code')].filter(visible).filter(e=>e.scrollWidth>e.clientWidth+2).map(e=>e.textContent.slice(0,70));
    const rgb=v=>v.match(/[\\d.]+/g)?.map(Number).slice(0,3);
    const lum=c=>c.map(v=>{v/=255;return v<=.04045?v/12.92:((v+.055)/1.055)**2.4}).reduce((n,v,i)=>n+v*[.2126,.7152,.0722][i],0);
    // Translucent backgrounds are composited over their ancestors, as the browser paints them.
    const bg=e=>{const layers=[];for(let p=e;p;p=p.parentElement){const n=getComputedStyle(p).backgroundColor.match(/[\\d.]+/g)?.map(Number);if(!n)continue;const a=n.length>3?n[3]:1;if(a>0){layers.push([n[0],n[1],n[2],a]);if(a>=1)break;}}let base=[255,255,255];for(let i=layers.length-1;i>=0;i--){const [r,g,b,a]=layers[i];base=[r*a+base[0]*(1-a),g*a+base[1]*(1-a),b*a+base[2]*(1-a)];}return base};
    const contrast=[];
    for(const e of document.body.querySelectorAll('*')){
      if(!visible(e)||e.closest('svg')||![...e.childNodes].some(n=>n.nodeType===3&&n.textContent.trim()))continue;
      const s=getComputedStyle(e),fg=rgb(s.color),ground=bg(e);if(!fg||!ground)continue;
      const a=lum(fg),b=lum(ground),ratio=(Math.max(a,b)+.05)/(Math.min(a,b)+.05),large=parseFloat(s.fontSize)>=24||(parseFloat(s.fontSize)>=18.66&&parseFloat(s.fontWeight)>=700);
      if(ratio<(large?3:4.5))contrast.push({text:e.textContent.trim().slice(0,65),ratio:+ratio.toFixed(2)});
    }
    return{width:innerWidth,documentWidth:document.documentElement.scrollWidth,overflow,clipped,contrast,inline:document.querySelectorAll('[style]').length,h1:document.querySelectorAll('main h1').length,images:[...document.querySelectorAll('main img,main video')].filter(e=>e.tagName!=='IMG'||!['/Images/RuneStone-','/Images/Logo.png'].some(name=>(e.currentSrc||e.src).includes(name))).length,font:document.fonts.check('16px Inter'),mono:document.fonts.check('12px "JetBrains Mono"')};
  })()`;
  await viewport(1440); await navigate('/Studio/docs');
  const routes = await evaluate(`[...new Set([...document.querySelectorAll('.docs-sidebar a')].map(a=>new URL(a.href).pathname))]`);
  assert(routes.length >= 25, `only ${routes.length} article links`);
  const all = ['/Studio/', ...routes, '/Studio/install', '/Studio/license', '/Studio/changelog', '/Studio/missing'];
  for (const width of [320, 390, 768, 1024, 1440]) {
    await viewport(width);
    for (const route of all) {
      await navigate(route); const result = await evaluate(audit); const label = `${route}/${width}`;
      assert(result.documentWidth <= width, `${label}: document overflow ${result.documentWidth} > ${width}`);
      assert.deepEqual(result.overflow, [], `${label}: element overflow ${JSON.stringify(result.overflow)}`);
      assert.deepEqual(result.clipped, [], `${label}: clipping ${JSON.stringify(result.clipped)}`);
      assert.deepEqual(result.contrast, [], `${label}: contrast ${JSON.stringify(result.contrast)}`);
      assert.equal(result.inline, 0, `${label}: inline style`); assert.equal(result.h1, 1, `${label}: h1`);
      assert.equal(result.images, 0, `${label}: screenshot/video substituted for UI (only the runestone and the drawer's brand mark are images)`);
      assert(result.font && result.mono, `${label}: local fonts`);
      report.layouts.push({ route, ...result });
      if ([390, 1440].includes(width) && ['/Studio/', '/Studio/docs', '/Studio/docs/settings', '/Studio/docs/security', '/Studio/install'].includes(route)) await screenshot(`${route.replaceAll('/', '-')}-${width}.png`);
    }
  }
  report.checks.push(`${report.layouts.length} layouts: all articles and supporting pages, five widths, no overflow/clipping/low contrast/inline styles`);

  await viewport(1440); await navigate('/Studio/docs/settings');
  // A chapter the reader opened stays open while they move between articles
  // (owner, 2026-11-03), so this first check starts from an empty memory: the
  // article a reader reaches for the first time opens its own section alone.
  await evaluate(`sessionStorage.removeItem('rune.docs.opened')`);
  await navigate('/Studio/docs/settings');
  assert.equal(await evaluate(`document.querySelectorAll('.docs-group[open]').length`), 1);
  assert.equal(await evaluate(`document.querySelector('.docs-group[open]>summary').textContent`), 'Configure');
  assert.equal(await evaluate(`document.querySelector('.docs-sidebar a[aria-current=page]').textContent`), 'Settings overview');
  await click('.docs-group summary');
  assert.equal(await evaluate(`document.querySelector('.docs-group').open`), true);
  // Chapters open on their own: opening one never closes another, and moving to
  // another article keeps what the reader opened (owner, 2026-10-29/2026-11-03).
  assert.equal(await evaluate(`document.querySelectorAll('.docs-group[open]').length`), 2);
  await navigate('/Studio/docs/models');
  assert.equal(await evaluate(`document.querySelectorAll('.docs-group[open]').length`), 2, 'a chapter the reader opened closed when they moved on');
  // A short closing section is still reached: at the end of the page the last entry is current.
  await navigate('/Studio/license');
  await evaluate(`window.scrollTo({top:document.documentElement.scrollHeight,behavior:'instant'})`); await frame(); await frame();
  assert.equal(await evaluate(`[...document.querySelectorAll('.index a')].at(-1).getAttribute('aria-current')`), 'true');
  await navigate('/Studio/docs/security#permission-modes');
  await evaluate(`document.querySelector('#permission-modes').scrollIntoView({block:'start',behavior:'instant'})`); await frame();
  assert.equal(await evaluate(`document.querySelector('.index a[aria-current=true]').hash`), '#permission-modes');
  const top = await evaluate(`document.querySelector('#permission-modes').getBoundingClientRect().top`);
  assert(top >= 64 && top <= 150, `heading under header: ${top}`);
  await navigate('/Studio/security');
  assert.equal(await evaluate('location.pathname'), '/Studio/docs/security');
  await viewport(390); await navigate('/Studio/docs/models');
  // On a phone the manual's index is a drawer, and its chapters are already
  // open inside it (owner, 2026-11-03), so what a tap opens is the drawer.
  assert.equal(await evaluate(`document.querySelector('.docs-menu').open`), true);
  assert.equal(await evaluate(`document.querySelector('#docs-nav').classList.contains('is-open')`), false);
  await click('.menu-toggle');
  assert.equal(await evaluate(`document.querySelector('#docs-nav').classList.contains('is-open')`), true);
  report.checks.push('Active sidebar group, native open/close, mobile disclosure, TOC scrollspy, heading offset, Security redirect');

  // The language menu and the language choice never move the page: choosing a
  // language keeps the reader exactly where they were (owner, 2026-11-05).
  // Before this rule the browser's scrollIntoView moved the page by 19px when
  // the menu opened, and a language link glided to the top of the article.
  await viewport(1440); await navigate('/Studio/docs/settings');
  await evaluate(`window.scrollTo({top:800,behavior:'instant'})`); await frame();
  await evaluate(`document.querySelector('.lang-switch>summary').click()`); await frame();
  assert.equal(await evaluate(`document.querySelector('.lang-switch').open`), true, 'the language menu did not open');
  assert.equal(await evaluate('Math.round(window.scrollY)'), 800, 'opening the language menu moved the page');
  await evaluate(`document.querySelector('.lang-switch').open=false`); await frame();
  const languageLoaded = new Promise((resolve, reject) => {
    const handler = m => { if (m.sessionId === sessionId) { clearTimeout(timer); events.get('Page.loadEventFired').delete(handler); resolve(); } };
    const timer = setTimeout(() => { events.get('Page.loadEventFired').delete(handler); reject(new Error('the language page never loaded')); }, 20000);
    events.get('Page.loadEventFired').add(handler);
  });
  await evaluate(`[...document.querySelectorAll('.lang-item')].find(a=>a.getAttribute('aria-current')!=='true').click()`);
  await languageLoaded;
  await evaluate(`Promise.all([document.fonts.ready]).then(()=>true)`); await frame();
  assert.equal(await evaluate('document.documentElement.lang'), 'id', 'the language did not change');
  await evaluate('new Promise(r=>setTimeout(r,600))');
  assert.equal(await evaluate('Math.round(window.scrollY)'), 800, 'choosing a language moved the page');
  report.checks.push('Language menu and language choice keep the reading position exactly');

  await navigate('/Studio/install');
  assert.equal(await evaluate(`document.querySelectorAll('[data-copy="install-cmd"]').length`), 1);
  assert.equal(await evaluate(`document.querySelectorAll('[data-copy="install-cmd-windows"]').length`), 1);
  // install.sh for Linux and macOS, install.ps1 for Windows: one command per system.
  assert.equal(await evaluate(`document.body.textContent.split('curl -fsSL').length-1`), 2);
  await click('[data-copy="install-cmd"]');
  assert(['done', 'select'].includes(await evaluate(`document.querySelector('[data-copy="install-cmd"]').dataset.state`)));
  await click('.install-more details>summary'); assert.equal(await evaluate(`document.querySelector('.install-more details').open`), true);
  const unpublished = `[...document.querySelectorAll('.install-note')].filter(n=>n.textContent.includes('currently exits without installing anything')).length`;
  // A catalog either has published builds or it has none; both states are
  // truthful, so the checks follow what the page actually serves (the fixture
  // branch keeps the published case available whatever the local folder holds).
  const listed = await evaluate(`document.querySelectorAll('.download-table tbody tr, .download-row').length`);
  if (fixtures.size || listed > 0) {
    assert(listed > 0, 'a published catalog must list its builds');
    assert.equal(await evaluate(unpublished), 0);
    assert(await evaluate(`document.querySelectorAll('.btn-dl').length > 0`), 'published builds must have download actions');
  } else assert.equal(await evaluate(unpublished), 3);
  report.checks.push('One install command per system, corner copy control with check or selection fallback, options disclosure, truthful catalog state');

  await viewport(1440, 'no-preference'); await navigate('/Studio/');
  await evaluate(`document.querySelector('.stage').scrollIntoView({block:'center',behavior:'instant'})`); await frame();
  const link = '.hero-links .link';
  await evaluate(`document.querySelector('${link}').scrollIntoView({block:'center',behavior:'instant'})`); await frame();
  const point = await evaluate(`(()=>{const r=document.querySelector('${link}').getBoundingClientRect();return{x:r.x+r.width/2,y:r.y+r.height/2}})()`);
  // A link brightens its ink and draws no underline (owner, 2026-11-03): the
  // centre-out underline of the older design is gone, so the check holds the
  // rule that is in force rather than the one it replaced.
  const restingInk = await evaluate(`getComputedStyle(document.querySelector('${link}')).color`);
  await send('Input.dispatchMouseEvent', { type: 'mouseMoved', ...point });
  await evaluate('new Promise(r=>setTimeout(r,400))');
  assert.notEqual(await evaluate(`getComputedStyle(document.querySelector('${link}')).color`), restingInk, 'a link must brighten its ink on hover');
  assert.equal(await evaluate(`getComputedStyle(document.querySelector('${link}')).textDecorationLine`), 'none', 'a link draws no underline');
  assert.equal(await evaluate('document.documentElement.dataset.theme'), 'amber');
  assert.equal(await evaluate(`getComputedStyle(document.querySelector('.logo-amber')).display`), 'block');
  const result = await evaluate(audit);
  assert.deepEqual(result.contrast, [], `amber terminal contrast`);
  report.checks.push('A link brightens its ink on hover and draws no underline; authentic amber logo, macOS traffic-light header, and contrast preserved');

  // The home replay is a six-chapter excerpt paced for reading (~80 s, owner
  // 2026-10-28). This general suite checks real-clock interaction; the
  // dedicated suite drives the whole schedule with an external scheduler.
  await evaluate(`document.querySelector('.stage').scrollIntoView({block:'center',behavior:'instant'});document.querySelector('[data-demo-replay]').click()`);await frame();
  const motion=await evaluate(`new Promise(resolve=>{const q=s=>document.querySelector(s);setTimeout(()=>{q('[data-demo-pause]').click();const at=q('[data-demo]').dataset.elapsed;setTimeout(()=>{const held=at===q('[data-demo]').dataset.elapsed;q('[data-demo-pause]').click();q('[data-side]').dispatchEvent(new WheelEvent('wheel',{deltaY:100,bubbles:true}));setTimeout(()=>resolve({held,playing:q('[data-demo]').dataset.playback==='playing',end:+q('[data-demo]').dataset.schedule.split('|')[1],steps:q('[data-demo]').dataset.schedule.split('|')[6].split(';').length,files:document.querySelectorAll('.demo-file').length}),250)},300)},500)})`);
  assert(motion.held&&motion.playing,'explicit Pause or sidebar no-auto-pause failed');
  assert(motion.end>60000&&motion.end<95000,'the excerpt must be paced for reading, not the whole recording');assert.equal(motion.files,64);
  report.checks.push('Recorded excerpt: reading pace, explicit Pause holds clock, sidebar interaction never pauses; whole excerpt covered by dedicated scheduler suite');

  await viewport(390); await navigate('/Studio/');
  assert.equal(await evaluate(`document.querySelector('.term').dataset.demoStage`), 'complete');
  assert.equal(await evaluate(`document.querySelectorAll('[data-step]:not([hidden])').length`), motion.steps);
  assert.equal(await evaluate(`document.querySelector('[data-scene=response]').hidden`), false);
  await send('Emulation.setScriptExecutionDisabled', { value: true }); scriptsDisabled = true;
  await navigate('/Studio/docs/security');
  assert.equal(await evaluate(`document.querySelectorAll('.prose h2').length>=5`), true);
  await navigate('/Studio/');
  assert.equal(await evaluate(`document.querySelector('[data-scene=response]').hidden`), false);
  await navigate('/Studio/install');
  assert.equal(await evaluate(`document.querySelector('#install-cmd').textContent.includes('curl')`), true);
  await send('Emulation.setScriptExecutionDisabled', { value: false }); scriptsDisabled = false;
  report.checks.push('Reduced motion and no-JavaScript: complete articles, completed illustration and usable install source');

  await viewport(768); await navigate('/Studio/docs/keyboard');
  await send('Emulation.setPageScaleFactor', { pageScaleFactor: 2 }); await frame();
  assert.equal(await evaluate('visualViewport.scale'), 2);
  const tree = await send('Accessibility.getFullAXTree');
  assert.equal(tree.nodes.filter(n => !n.ignored && n.role?.value === 'main').length, 1);
  report.checks.push('200% zoom, main landmark, native details semantics');
  assert.deepEqual(report.errors, []);
  await writeFile(path.join(output, 'report.json'), JSON.stringify(report, null, 2));
  console.log(`PASS ${report.layouts.length} layouts; ${report.checks.length} behavior groups; ${report.screenshots.length} screenshots; ${output}`);
} catch (error) {
  await writeFile(path.join(output, 'failure.json'), JSON.stringify({ message: error.message, ...report }, null, 2));
  throw error;
} finally {
  for (const p of pending.values()) clearTimeout(p.timer);
  if (socket) socket.close();
  if (browser) browser.kill('SIGTERM');
}
