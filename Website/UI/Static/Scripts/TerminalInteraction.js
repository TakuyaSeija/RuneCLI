/* Pointer interaction belongs to the terminal surface, not browser link UI.
   Native details remains the no-script/keyboard fallback. The enhancement lets
   the whole drawn step hit area toggle it, while a drag copies content only. */
(() => {
  "use strict";
  const root=document.querySelector("[data-demo]"),term=root?.querySelector(".term");
  if(!term)return;
  const log=root.querySelector("[data-log]"),side=root.querySelector("[data-side]");
  let gesture=null,suppressClick=false;
  const scopeOf=node=>[log,side,root.querySelector("[data-permission-body]")].find(surface=>surface?.contains(node));
  const excluded=node=>node.closest(".step-dot,.cut-row,legend,.sh,.st dt,.mark,.counts,.answers,.hint,.jump-latest,.tbar,.permission-simulation");
  const cancel=()=>{if(gesture?.drag)getSelection()?.removeAllRanges();gesture=null;};
  term.addEventListener("pointerdown",event=>{
    suppressClick=false;
    if(event.button!==0||event.pointerType==="touch"||event.target.closest("button"))return;
    const surface=scopeOf(event.target);if(!surface)return;
    gesture={x:event.clientX,y:event.clientY,surface,drag:false};suppressClick=false;
  });
  addEventListener("pointermove",event=>{
    if(!gesture||!(event.buttons&1))return;
    if(Math.hypot(event.clientX-gesture.x,event.clientY-gesture.y)>3)gesture.drag=true;
  },{passive:true});
  addEventListener("pointerup",event=>{
    if(!gesture||event.button!==0)return;
    const current=gesture;gesture=null;
    if(!current.drag)return;
    suppressClick=true;
    const selection=getSelection();
    if(!selection||selection.isCollapsed){selection?.removeAllRanges();return;}
    const selected=selection.getRangeAt(0),parts=[];
    const walker=document.createTreeWalker(current.surface,NodeFilter.SHOW_TEXT,{acceptNode:node=>{
      if(!node.textContent||!node.parentElement?.getClientRects().length||excluded(node.parentElement))return NodeFilter.FILTER_REJECT;
      for(let p=node.parentElement;p&&p!==current.surface;p=p.parentElement){if(p.hidden||getComputedStyle(p).display==="none")return NodeFilter.FILTER_REJECT;}
      return selected.intersectsNode(node)?NodeFilter.FILTER_ACCEPT:NodeFilter.FILTER_REJECT;
    }});
    let node,lastRow=null;
    while((node=walker.nextNode())){
      const row=node.parentElement.closest(".content-row,.user-text,.step-title,.bullet-row,.task-row,.demo-file,dd,p,li,pre")||node.parentElement;
      if(lastRow&&lastRow!==row)parts.push("\n");
      const first=node===selected.startContainer?selected.startOffset:0;
      const last=node===selected.endContainer?selected.endOffset:node.textContent.length;
      parts.push(node.textContent.slice(first,last));lastRow=row;
    }
    const copied=parts.join("");
    // The gesture is always cleared, including clipboard refusal. No fake copy
    // success is reported on an insecure origin where the browser denies it.
    selection.removeAllRanges();
    if(copied&&navigator.clipboard?.writeText)navigator.clipboard.writeText(copied).catch(()=>{});
    else if(copied){
      const listener=e=>{e.clipboardData?.setData("text/plain",copied);e.preventDefault();};
      document.addEventListener("copy",listener,{once:true});
      try{document.execCommand("copy");}finally{document.removeEventListener("copy",listener);selection.removeAllRanges();}
    }
  });
  root.addEventListener("click",event=>{
    if(suppressClick){suppressClick=false;event.preventDefault();event.stopPropagation();return;}
    if(event.target.closest("button,a")||!log.contains(event.target))return;
    let fold=event.target.closest("details");
    const block=event.target.closest(".demo-step,.demo-user");
    if(!fold)fold=block?.querySelector("details");
    if(!fold)return;
    event.preventDefault();
    root.dispatchEvent(new Event("demo-disclosure-before"));
    const open=!fold.open;
    if(block?.dataset.kind==="tool")for(const item of block.querySelectorAll("details"))item.open=open;
    else fold.open=open;
    root.dispatchEvent(new CustomEvent("demo-disclosure-after",{detail:block||fold}));
  },true);
  addEventListener("resize",cancel);term.addEventListener("wheel",cancel,{passive:true});
  document.addEventListener("keydown",event=>{
    if((event.key==="Enter"||event.key===" ")&&event.target.closest?.("summary")&&log.contains(event.target))root.dispatchEvent(new Event("demo-disclosure-before"));
    if(event.ctrlKey&&event.key==="ArrowDown"&&term.contains(document.activeElement)){
      event.preventDefault();root.querySelector("[data-jump]").click();
    }
  });
})();
