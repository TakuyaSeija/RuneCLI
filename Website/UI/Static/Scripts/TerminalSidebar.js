/* Sticky ink must never change the document being scrolled. The source FILES
   heading keeps its row while its pinned representation is active. */
export class SidebarViewport {
  constructor(root){
    this.root=root;this.side=root.querySelector('[data-side]');this.plan=root.querySelector('[data-demo-plan-list]');this.files=root.querySelector('[data-demo-files]');
    this.heading=root.querySelector('[data-side-heading]');this.planHead=root.querySelector('[data-side-plan-heading]');this.fileHead=root.querySelector('[data-side-files-heading]');
    this.sourceHead=root.querySelector('.files-head');this.above=root.querySelector('[data-side-above]');this.below=root.querySelector('[data-side-more]');
    this.follow=false;this.expected=null;this.anchor=null;
    this.side.addEventListener('scroll',()=>{const own=this.expected!==null&&Math.abs(this.expected-this.side.scrollTop)<1;this.expected=null;if(!own){this.follow=this.atBottom();this.capture();}this.paint();},{passive:true});
    this.side.addEventListener('wheel',event=>{if(event.deltaY<0)this.follow=false;this.capture();},{passive:true});
  }
  atBottom(){return this.side.scrollHeight-this.side.clientHeight-this.side.scrollTop<3;}
  capture(){
    const top=this.side.scrollTop;
    const rows=[...this.plan.children,...this.files.querySelectorAll('.demo-file')].filter(n=>!n.hidden);
    const row=rows.find(n=>n.offsetTop+n.offsetHeight>top);
    this.anchor=row?{row,offset:row.offsetTop-top}:null;
  }
  before(){this.position=this.side.scrollTop;this.wasBottom=this.atBottom()&&this.side.scrollTop>0;this.capture();}
  after(){
    let top=this.position??this.side.scrollTop;
    if(this.wasBottom)top=this.side.scrollHeight-this.side.clientHeight;
    else if(this.anchor&&!this.anchor.row.hidden)top=this.anchor.row.offsetTop-this.anchor.offset;
    if(Math.abs(top-this.side.scrollTop)>.5){this.side.scrollTop=Math.max(0,top);this.expected=this.side.scrollTop;}
    this.paint();
  }
  paint(){
    const side=this.side;if(!side.clientHeight)return;
    const line=parseFloat(getComputedStyle(side).lineHeight),above=Math.max(0,Math.floor(side.scrollTop/line));
    const below=Math.max(0,Math.ceil((side.scrollHeight-side.clientHeight-side.scrollTop)/line));
    const show=(e,on)=>{if(e.hidden===on)e.hidden=!on;};
    const text=(e,value)=>{if(e.textContent!==value)e.textContent=value;};
    text(this.above,above?'↑ '+above+' rows above · wheel here':'');text(this.below,below?'↓ '+below+' rows below · wheel here':'');show(this.above,above>0);show(this.below,below>0);
    const hasPlan=!this.plan.hidden,hasFiles=!this.files.hidden;
    const filesActive=hasFiles&&(!hasPlan||side.scrollTop>=this.sourceHead.offsetTop+this.sourceHead.offsetHeight);
    show(this.heading,hasPlan||hasFiles);show(this.planHead,hasPlan&&!filesActive);show(this.fileHead,filesActive);
    if(filesActive&&this.fileHead.textContent!==this.sourceHead.textContent){
      const copy=this.sourceHead.firstElementChild.cloneNode(true);
      for(const n of [copy,...copy.querySelectorAll('*')])for(const a of [...n.attributes])if(a.name.startsWith('data-'))n.removeAttribute(a.name);
      this.fileHead.replaceChildren(copy);
    }
    this.sourceHead.dataset.pinned=String(filesActive);
  }
  reset(){this.follow=false;this.anchor=null;this.expected=null;this.position=0;this.side.scrollTop=0;this.paint();}
}
