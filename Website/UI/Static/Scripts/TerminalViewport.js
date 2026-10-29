/* Layout writes run in a transaction. Reader intent is sampled before content
   mutates, including scrollTop changes whose native scroll event is still queued. */
export class TranscriptViewport {
  constructor(log,showJump){
    this.log=log;this.follow=true;this.anchor=null;this.lastTop=log.scrollTop;this.showJump=showJump;
    log.addEventListener('scroll',()=>{this.adoptScroll();this.showJump();},{passive:true});
    log.addEventListener('wheel',event=>{this.adoptScroll();if(event.deltaY<0){this.follow=false;this.capture();}this.showJump();},{passive:true});
  }
  atBottom(){return this.log.scrollHeight-this.log.clientHeight-this.log.scrollTop<3;}
  adoptScroll(){
    if(Math.abs(this.log.scrollTop-this.lastTop)<.5)return;
    this.lastTop=this.log.scrollTop;this.follow=this.atBottom();this.capture();
  }
  capture(){
    if(this.follow){this.anchor=null;return;}
    const rect=this.log.getBoundingClientRect(),scale=rect.width/this.log.offsetWidth||1;
    const owners=[...this.log.querySelectorAll('.thead,.demo-user,[data-step]:not([hidden])')];
    const owner=owners.find(n=>n.getBoundingClientRect().bottom>rect.top);
    if(!owner){this.anchor=null;return;}
    const row=[...owner.querySelectorAll('p,h3,pre,li,.content-row,.task-row,.user-text')].find(n=>n.getClientRects().length&&n.getBoundingClientRect().bottom>rect.top)||owner;
    this.anchor={row,offset:(row.getBoundingClientRect().top-rect.top)/scale,scroll:this.log.scrollTop};
  }
  before(){this.adoptScroll();if(!this.follow&&!this.anchor)this.capture();}
  restore(){
    let target=this.log.scrollTop;
    if(this.follow)target=this.log.scrollHeight-this.log.clientHeight;
    else if(this.anchor){
      const {row,scroll,offset}=this.anchor,rect=this.log.getBoundingClientRect(),scale=rect.width/this.log.offsetWidth||1;
      target=row.getClientRects().length?this.log.scrollTop+(row.getBoundingClientRect().top-rect.top)/scale-offset:scroll;
    }
    if(Math.abs(this.log.scrollTop-Math.max(0,target))>.5)this.log.scrollTop=Math.max(0,target);
    this.lastTop=this.log.scrollTop;this.showJump();
  }
  jump(){this.follow=true;this.anchor=null;this.restore();}
  reset(){this.follow=true;this.anchor=null;this.log.scrollTop=0;this.lastTop=0;}
}

export class FoldMeasurements {
  constructor(root){this.log=root.querySelector('[data-log]');this.folds=[...root.querySelectorAll('[data-fold]')];this.dirty=new Set(this.folds);this.sources=new WeakMap();this.width=0;}
  invalidate(node){const fold=node?.closest('[data-fold]')||node?.querySelector('[data-fold]');if(fold)this.dirty.add(fold);}
  all(){for(const fold of this.folds)this.dirty.add(fold);}
  measure(){
    const width=this.log.clientWidth;if(width!==this.width){this.width=width;this.all();}
    for(const fold of this.dirty){
      this.dirty.delete(fold);
      if(!fold.getClientRects().length)continue;
      const content=fold.querySelector('[data-fold-rows]'),line=parseFloat(getComputedStyle(content).lineHeight);
      let source=this.sources.get(fold);
      if(!source){source=[...content.children];this.sources.set(fold,source);}
      if(source.length&&content.children.length!==source.length)content.replaceChildren(...source);
      const count=Math.max(0,Math.round(content.scrollHeight/line)-4);
      if(fold.dataset.cut!==String(count)){fold.dataset.cut=String(count);fold.querySelector('[data-fold-label]').textContent=count?'… '+count.toLocaleString('en-US')+' lines cut. Click to open all':'';}
      if(!fold.open&&source.length>4){
        const top=content.getBoundingClientRect().top,scale=content.getBoundingClientRect().width/content.offsetWidth||1;
        const shown=source.filter(row=>!row.hidden&&row.getBoundingClientRect().top<top+line*4*scale);
        content.replaceChildren(...shown);
      }
      this.dirty.delete(fold);
    }
  }
}
