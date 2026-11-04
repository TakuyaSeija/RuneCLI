import {readSchedule,latest,phase,initialState,canAdvance,wantsPlay,reduce,sourceAt} from './TerminalReplay.js';
import {TextStream,fraction,duration,prefixAt,outputBatches} from './TerminalStream.js';
import {TranscriptViewport,FoldMeasurements} from './TerminalViewport.js';
import {SidebarViewport} from './TerminalSidebar.js';

/* A frame visits only new or active steps. Settled source is left alone; Replay
   hides its containers, not fifty thousand syntax nodes one character at a time. */
const root=document.querySelector('[data-demo]');
if(root){
  const q=s=>root.querySelector(s),show=(n,on)=>{if(n&&n.hasAttribute('hidden')===on)n.toggleAttribute('hidden',!on);};
  const text=(n,value)=>{if(n&&n.textContent!==String(value))n.textContent=value;};
  const attr=(n,key,value)=>{if(n.dataset[key]!==String(value))n.dataset[key]=String(value);};
  const term=q('.term'),screen=q('[data-terminal-screen]'),log=q('[data-log]'),side=q('[data-side]');
  const schedule=readSchedule(root.dataset.schedule),media=matchMedia('(prefers-reduced-motion: reduce)'),narrow=matchMedia('(max-width: 920px)');
  const pause=q('[data-demo-pause]'),permission=q('[data-permission]'),composer=q('[data-composer]');
  const fields=Object.fromEntries([...root.querySelectorAll('[data-f]')].map(n=>[n.dataset.f,n]));
  const ui={typed:q('[data-typed]'),placeholder:q('[data-placeholder]'),user:q('[data-scene=user]'),spin:q('[data-spin]'),word:q('[data-word]'),rule:q('[data-rule]')};
  const files=[...root.querySelectorAll('.demo-file')],tasks=[...root.querySelectorAll('.demo-plan li')];
  const prompt=q('[data-user-text]').textContent,context=(+root.dataset.context).toLocaleString('en-US'),fmt=n=>n.toLocaleString('en-US');
  const folds=new FoldMeasurements(root),sidebar=new SidebarViewport(root);
  const chapters=[...root.querySelectorAll('[data-chapter]')];
  let state=initialState(),frame=0,geometryFrame=0,gate=-1,paintedGate=-1,account=-2,plan=-2,fileKey='',ready=false,next=0,previous=null,geometryDirty=true,chapter=-2;
  const pending=new Set();
  const viewport=new TranscriptViewport(log,()=>show(q('[data-jump]'),!viewport.follow&&gate<0&&log.scrollHeight>log.clientHeight));
  const views=[...root.querySelectorAll('[data-step]')].map((node,index)=>({
    node,index,row:schedule.steps[index],kind:node.dataset.kind,tool:node.dataset.kind==='tool',phase:null,streams:null,parts:[...node.querySelectorAll('[data-stream]')],
    body:node.querySelector('.step-body'),results:[...node.querySelectorAll('.box-out,.plain-output,.tasks-fold')],box:node.querySelector('.demo-box'),
    timer:node.querySelector('[data-tool-time]'),resultNote:node.querySelector('[data-result-note]'),argNote:node.querySelector('[data-argument-note]'),
    added:node.querySelector('[data-added]'),removed:node.querySelector('[data-removed]'),words:node.querySelector('[data-words]'),word:node.querySelector('.t-word'),
    output:node.dataset.call==='terminal_use'?[...node.querySelectorAll('.box-out .content-row:not(.tool-notice)')]:[],notices:[...node.querySelectorAll('.box-out .tool-notice')],outputShown:-1,
    countRows:[...node.querySelectorAll('.content-row')].filter(r=>!r.closest('.gate-source')&&(node.dataset.call==='create_file'||r.dataset.op==='+'||r.dataset.op==='-')).map(r=>({op:r.dataset.op||'+',size:Array.from(r.textContent).length+1})),
  }));
  let timeline=q('[data-timeline]');
  const activity=v=>!!v&&!['final','notification'].includes(v.kind);
  const sourceElapsed=()=>sourceAt(schedule.clock,state.elapsed);
  function playbackUI(){
    const playing=wantsPlay(state,schedule.end);show(pause.querySelector('[data-icon=play]'),!playing);show(pause.querySelector('[data-icon=pause]'),playing);
    const label=playing?'Pause the replay':'Play the replay';if(pause.getAttribute('aria-label')!==label){pause.setAttribute('aria-label',label);pause.setAttribute('aria-pressed',String(!playing));pause.title=playing?'Pause':'Play';}
    attr(root,'playback',playing?'playing':state.paused?'paused':'complete');attr(root,'elapsed',Math.round(state.elapsed));attr(root,'advancing',canAdvance(state));
  }
  function resetProjection(){
    const empty=timeline.cloneNode(false);timeline.replaceWith(empty);timeline=empty;
    for(const view of views)view.phase=null;
    next=0;previous=null;pending.clear();gate=-1;paintedGate=-1;account=plan=-2;fileKey='';geometryDirty=true;viewport.reset();sidebar.reset();
  }
  function streams(view){
    if(!view.streams)view.streams=view.parts.map(element=>({kind:element.dataset.stream,stream:new TextStream(element)}));
    return view.streams;
  }
  function argumentsAt(view,progress){
    const parts=streams(view),bodySize=view.countRows.reduce((n,r)=>n+r.size,0),total=parts.reduce((n,s)=>n+s.stream.length,0)+bodySize;
    let left=prefixAt(total,progress),changed=false;
    for(const {stream} of parts){changed=stream.reveal(stream.snap(left))||changed;left-=stream.length;}
    let added=0,removed=0;for(const row of view.countRows){if(left<=0)break;if(row.op==='+')added++;else removed++;left-=row.size;}
    if(view.added)text(view.added,'+'+(progress>=1?+view.added.dataset.added:Math.min(+view.added.dataset.added,added)));
    if(view.removed)text(view.removed,'−'+(progress>=1?+view.removed.dataset.removed:Math.min(+view.removed.dataset.removed,removed)));
    const target=parts.find(s=>s.kind==='target');show(view.argNote,!!target&&target.stream.shown>=target.stream.length);
    return changed;
  }
  function paintOutput(view,done){
    if(!view.batches)view.batches=outputBatches(view.output.map(n=>n.textContent));
    const progress=fraction(state.elapsed,view.row[6],view.row[0]+view.row[1]);
    const batch=done?view.batches.length:Math.min(view.batches.length,Math.floor(progress*(view.batches.length+1)));
    const count=batch?view.batches[batch-1]:0;
    if(count===view.outputShown)return false;
    for(let i=0;i<view.output.length;i++)show(view.output[i],i<count);
    for(const notice of view.notices)show(notice,done);
    view.outputShown=count;for(const result of view.results)show(result,done||count>0);
    for(const result of view.results)folds.invalidate(result);
    return true;
  }
  function paintStep(view){
    const {node,row,tool}=view,current=phase(row,state.elapsed,tool),changedPhase=view.phase!==current,done=current==='done';
    if(changedPhase){
      attr(node,'phase',current);attr(node,'state',done?'done':'work');show(view.body,current!=='asking');show(view.resultNote,done);
      if(tool){for(const result of view.results)show(result,done);if(node.dataset.call!=='terminal_use')show(view.box,done);}
      for(const fold of node.querySelectorAll('[data-fold]'))folds.invalidate(fold);
      geometryDirty=true;
    }
    if(tool){
      if(current==='writing'||changedPhase){
        if(done&&!view.streams){if(view.added)text(view.added,'+'+view.added.dataset.added);if(view.removed)text(view.removed,'−'+view.removed.dataset.removed);show(view.argNote,true);}
        else geometryDirty=argumentsAt(view,fraction(state.elapsed,row[0],row[5]))||geometryDirty;
      }
      if(view.output.length){geometryDirty=paintOutput(view,done)||geometryDirty;for(const result of view.results)show(result,done||view.outputShown>0);}
      if(changedPhase)for(const notice of view.notices)show(notice,done);
      const elapsed=current==='running'?row[7]*fraction(state.elapsed,row[6],row[0]+row[1]):done?row[7]:0;
      show(view.timer,elapsed>=1000||current==='running');text(view.timer,duration(elapsed/1000));attr(node,'pulse',Math.floor(state.elapsed/608)%2===0?'bright':'faint');
    }else if(view.kind!=='notification'){
      if(done&&!view.streams){}else for(const {stream} of streams(view))geometryDirty=stream.reveal(stream.snap(prefixAt(stream.length,fraction(state.elapsed,row[0],row[5]))))||geometryDirty;
      if(view.kind==='think'){text(view.word,done?'Thought':'Thinking');text(view.words,fmt(done?row[2]:Math.round(row[2]*fraction(state.elapsed,row[0],row[5]))));} // The CLI's own count, reached as the thought ends.
    }
    view.phase=current;if(done)pending.delete(view);if(current==='asking')gate=view.index;
  }
  function paintSteps(){
    gate=-1;
    while(next<views.length&&views[next].row[0]<=state.elapsed){
      const view=views[next++];timeline.append(view.node);show(view.node,true);attr(view.node,'connect',false);
      if(previous)attr(previous.node,'connect',activity(previous)&&activity(view));previous=view;
      view.phase=null;view.outputShown=-1;pending.add(view);geometryDirty=true;
    }
    let active=null;
    for(const view of pending){paintStep(view);if(view.phase!=='done')active=view;}
    return active;
  }
  function paintPanel(){
    const at=latest(schedule.turns,state.elapsed),row=schedule.turns[at]||[0,0,0,0,0,0,0,0];
    if(account!==at){account=at;text(fields.ctx,fmt(row[1])+' / '+context);text(fields.tin,fmt(row[2]));text(fields.tout,fmt(row[3]));text(fields.cin,fmt(row[4]));text(fields.spend,'$'+(row[5]/10000).toFixed(4));}
    // Activity uses the very same source clock as the live footer, never the
    // timestamp of the last request that happened to settle.
    const seconds=state.elapsed>=schedule.end?schedule.active:sourceElapsed()/1000;
    // Steps are the recorded session's count at this moment, not the excerpt's.
    const counted=latest(schedule.counts,state.elapsed),steps=counted<0?0:schedule.counts[counted][1];
    text(fields.activity,duration(seconds)+' · '+steps+(steps===1?' step':' steps')+' · 1 turn');
    const pi=latest(schedule.plans,state.elapsed),key=schedule.files.map(at=>Number(state.elapsed>=at)).join('');
    if(plan!==pi||fileKey!==key){
      sidebar.before();
      if(plan!==pi){plan=pi;const p=schedule.plans[pi];show(q('[data-demo-plan-list]'),!!p);if(p){text(q('[data-plan-count]'),p[1]+'/'+p[2]);tasks.forEach((n,i)=>attr(n,'s',i<p[1]?'done':i===p[1]?'doing':'todo'));}}
      if(fileKey!==key){fileKey=key;let count=0,added=0,removed=0;files.forEach((file,i)=>{const on=key[i]==='1';show(file,on);if(on){count++;added+=+file.querySelector('.ok').textContent;removed+=+(file.querySelector('.bad')?.textContent.replace('−','')||0);}});text(q('[data-files-count]'),count);text(q('[data-files-added]'),'+'+added);text(q('[data-files-removed]'),'−'+removed);show(q('[data-demo-files]'),count>0);}
      sidebar.after();
    }
  }
  function paintGate(){
    const gated=gate>=0;if(permission.hidden===gated)geometryDirty=true;show(permission,gated);show(composer,!gated);
    if(gated&&paintedGate!==gate){paintedGate=gate;const step=views[gate].node,always=q('[data-answer=always]');text(q('[data-permission-title]'),step.dataset.gateTitle);
      // A command offers no standing grant; an edit offers its tool's (Ask mode).
      show(always,!!step.dataset.gateAlways);if(step.dataset.gateAlways)text(always,'Always allow '+step.dataset.gateAlways);for(const n of q('[data-permission-answers]').querySelectorAll('button'))n.setAttribute('aria-current',String(n.dataset.answer==='once'));q('[data-permission-body]').replaceChildren(...[...step.querySelector('.gate-source').children].map(n=>n.cloneNode(true)));q('[data-permission-body]').scrollTop=0;text(q('[data-permission-hint]'),'Illustration only · automatically continues');}
    const why=gated?' waiting on simulated approval':' asks before actions run; classifier is inactive in ask mode';
    if(q('[data-msg]').lastChild?.textContent!==why){const mode=document.createElement('b');mode.className='ok';mode.textContent='['+root.dataset.mode+']';q('[data-msg]').replaceChildren(mode,document.createTextNode(why));}
  }
  function paintChapters(){
    if(!chapters.length)return;
    const at=latest(schedule.chapters.map(start=>[start]),state.elapsed);
    if(chapter!==at){chapter=at;chapters.forEach((n,i)=>{if(i===at)n.setAttribute('aria-current','step');else n.removeAttribute('aria-current');});}
    chapters.forEach((n,i)=>{const bar=n.querySelector('progress'),p=Math.round(fraction(state.elapsed,schedule.chapters[i],schedule.chapters[i+1]??schedule.end)*1000)/1000;if(bar&&bar.value!==p)bar.value=p;});
  }
  function paint(){
    viewport.before();
    const typing=state.elapsed<schedule.typeEnd;text(ui.typed,typing?prompt.slice(0,Math.floor(prompt.length*fraction(state.elapsed,schedule.typeStart,schedule.typeEnd))):'');show(ui.placeholder,!typing||state.elapsed<=schedule.typeStart);show(ui.user,!typing);
    const active=paintSteps();paintGate();paintPanel();const working=!typing&&state.elapsed<schedule.end;
    show(ui.spin,working);show(ui.word,working);show(ui.rule,working);
    if(working){text(ui.spin,'⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏'[Math.floor(state.elapsed/96)%10]);const words={terminal_use:'Running',create_file:'Writing',patch_file:'Patching',inspect_file:'Reading',web_search:'Looking up',search_text:'Searching',process_control:'Controlling',manage_process:'Controlling',track_tasks:'Planning'};text(ui.word,(active?.kind==='think'?'Thinking':words[active?.node.dataset.call]||'Working')+' ['+duration(sourceElapsed()/1000)+']');}
    attr(term,'demoStage',state.elapsed>=schedule.end?'complete':gate>=0?'permission':working?'running':'idle');
    if(geometryDirty){folds.measure();viewport.restore();geometryDirty=false;}playbackUI();paintChapters();
  }
  function geometry(changed=null){if(!ready)return;viewport.before();if(changed instanceof Element)for(const fold of [changed,...changed.querySelectorAll('[data-fold]')])folds.invalidate(fold);folds.measure();panelUI();sidebar.paint();viewport.restore();}
  function queueGeometry(){if(!geometryFrame)geometryFrame=requestAnimationFrame(()=>{geometryFrame=0;geometry();});}
  function dispatch(event){const before=state.elapsed;state=reduce(state,event,schedule);if(state.elapsed<before||event.type==='replay')resetProjection();if(ready)paint();sync();}
  function tick(now){frame=0;const before=state.elapsed;state=reduce(state,{type:'frame',now},schedule);if(state.elapsed<before)resetProjection();paint();if(canAdvance(state)&&(!state.reduced||state.elapsed<schedule.end))frame=requestAnimationFrame(tick);}
  function sync(){if(frame)cancelAnimationFrame(frame);frame=0;state={...state,last:null};playbackUI();if(ready&&canAdvance(state)&&(!state.reduced||state.elapsed<schedule.end))frame=requestAnimationFrame(tick);}
  pause.addEventListener('click',()=>dispatch({type:wantsPlay(state,schedule.end)?'pause':'play'}));q('[data-demo-replay]').addEventListener('click',()=>dispatch({type:'replay'}));q('[data-jump]').addEventListener('click',()=>viewport.jump());
  for(const button of chapters)button.addEventListener('click',()=>{viewport.follow=true;viewport.anchor=null;dispatch({type:'seek',at:+button.dataset.at});});
  root.addEventListener('demo-disclosure-before',()=>{viewport.follow=false;viewport.capture();});root.addEventListener('demo-disclosure-after',event=>geometry(event.detail));root.addEventListener('toggle',event=>{if(event.target.matches('details'))geometry(event.target);},true);
  q('[data-permission-answers]').addEventListener('click',event=>{const button=event.target.closest('[data-answer]');if(!button||gate<0)return;for(const n of q('[data-permission-answers]').querySelectorAll('button'))n.setAttribute('aria-current',String(n===button));if(button.dataset.answer==='refuse'){dispatch({type:'pause'});text(q('[data-permission-hint]'),'Simulation paused. Play continues the original recording; nothing executes.');}else dispatch({type:'approval',at:schedule.steps[gate][4]});});
  const panelOpen=()=>term.dataset.panel==='show'||(!narrow.matches&&term.dataset.panel!=='hide');
  function panelUI(){const open=panelOpen();q('[data-demo-panel]').setAttribute('aria-expanded',String(open));q('[data-demo-panel]').setAttribute('aria-label',open?'Hide session panel':'Show session panel');}
  q('[data-demo-panel]').addEventListener('click',()=>{viewport.before();term.dataset.panel=panelOpen()?'hide':'show';geometry();queueGeometry();});
  // Full screen is the browser's own when it grants one; otherwise the window covers the page. Either way the
  // browser's own exit (Esc, its controls) and the button land in the same state through `expanded`.
  const fullscreenElement=()=>document.fullscreenElement||document.webkitFullscreenElement||null;
  function expanded(on){viewport.before();root.classList.toggle('is-expanded',on);document.body.classList.toggle('demo-expanded',on);const button=q('[data-demo-expand]');button.setAttribute('aria-pressed',String(on));button.setAttribute('aria-label',on?'Exit full screen':'Full screen');button.title=on?'Exit full screen':'Full screen';geometry();queueGeometry();}
  function expand(on){
    if(!on){if(fullscreenElement()===root)(document.exitFullscreen||document.webkitExitFullscreen).call(document);else expanded(false);return;}
    const request=root.requestFullscreen||root.webkitRequestFullscreen;
    if(!request||!(document.fullscreenEnabled||document.webkitFullscreenEnabled)){expanded(true);return;}
    try{const asked=request.call(root);if(asked&&asked.catch)asked.catch(()=>expanded(true));}catch{expanded(true);}
  }
  const followFullscreen=()=>{const on=fullscreenElement()===root;if(on!==root.classList.contains('is-expanded'))expanded(on);};
  document.addEventListener('fullscreenchange',followFullscreen);document.addEventListener('webkitfullscreenchange',followFullscreen);
  q('[data-demo-expand]').addEventListener('click',()=>expand(!root.classList.contains('is-expanded')));document.addEventListener('keydown',event=>{if(event.key==='Escape'&&root.classList.contains('is-expanded')&&!fullscreenElement()){expanded(false);q('[data-demo-expand]').focus();}});narrow.addEventListener('change',queueGeometry);
  document.addEventListener('visibilitychange',()=>dispatch({type:'visibility',visible:state.visible,hidden:document.hidden}));
  if('IntersectionObserver'in window)new IntersectionObserver(([entry])=>dispatch({type:'visibility',visible:entry.isIntersecting,hidden:document.hidden}),{threshold:0}).observe(q('[data-terminal-viewport]'));
  if('ResizeObserver'in window)new ResizeObserver(queueGeometry).observe(screen);media.addEventListener('change',()=>dispatch({type:'motion',reduced:media.matches}));
  const start=()=>{ready=true;state=reduce(state,{type:'motion',reduced:media.matches},schedule);resetProjection();show(q('[data-replay-controls]'),true);show(q('[data-view-controls]'),true);paint();sync();};
  if(document.fonts)document.fonts.ready.then(start);else start();
}
