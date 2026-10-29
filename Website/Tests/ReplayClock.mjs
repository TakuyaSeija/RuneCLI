// Test-only requestAnimationFrame scheduler installed by CDP before any page
// script. The production schedule remains 2.5×; tests drive its entire timeline
// without introducing a speed override or a test API into shipped JavaScript.
export const replayClock = `(() => {
  let now=0,serial=0;
  window.__recordedSteps=[];window.__recordedFolds=new Map();
  const capture=()=>{for(const node of document.querySelectorAll('[data-step]'))__recordedSteps[+node.dataset.step]=node;const encoded=document.querySelector('[data-demo]')?.dataset.schedule;if(encoded&&__recordedSteps.filter(Boolean).length===encoded.split('|')[6].split(';').length){for(const fold of document.querySelectorAll('[data-fold]'))__recordedFolds.set(fold,[...fold.querySelector('[data-fold-rows]').children].map(row=>row.textContent));observer.disconnect();}};
  const observer=new MutationObserver(capture);observer.observe(document,{childList:true,subtree:true});
  const queue=new Map();
  window.requestAnimationFrame=callback=>{queue.set(++serial,callback);return serial};
  window.cancelAnimationFrame=id=>queue.delete(id);
  window.__advanceReplay=milliseconds=>{
    const initial=[...queue.values()];queue.clear();for(const callback of initial)callback(now);
    let remaining=Math.max(0,milliseconds);
    do {
      const delta=Math.min(remaining,1000);now+=delta;remaining-=delta;
      const callbacks=[...queue.values()];queue.clear();for(const callback of callbacks)callback(now);
    } while(remaining>0);
    return now;
  };
})();`;
