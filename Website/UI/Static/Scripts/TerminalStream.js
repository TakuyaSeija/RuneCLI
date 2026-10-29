/* Source text stays escaped and lossless. Only changed prefix leaves are
   updated; provider text arrives a whole word at a time and terminal stdout
   in complete PTY-like row batches, never a typewriter. Arrival is simulated. */
const firstAfter=(items,value,key=x=>x)=>{let lo=0,hi=items.length;while(lo<hi){const mid=(lo+hi)>>>1;if(key(items[mid])<=value)lo=mid+1;else hi=mid;}return lo;};
export class TextStream {
  constructor(root) {
    this.root=root;this.leaves=[];this.blocks=[];this.wordStarts=[];this.length=0;this.shown=-1;
    const walker=document.createTreeWalker(root,NodeFilter.SHOW_TEXT);let node,lastBlock=null,inWord=false;
    const blocks=new Map();
    while((node=walker.nextNode())){
      if(!node.textContent.trim()&&node.parentElement===root)continue;
      const source=Array.from(node.textContent),block=node.parentElement.closest('p,h3,pre,li,.content-row,.task-row');
      if(block!==lastBlock)inWord=false;
      if(block&&!blocks.has(block)){blocks.set(block,this.length);this.blocks.push({node:block,start:this.length});}
      for(let i=0;i<source.length;i++){if(/\s/u.test(source[i]))inWord=false;else if(!inWord){this.wordStarts.push(this.length+i);inWord=true;}}
      this.leaves.push({node,source,start:this.length,end:this.length+source.length});this.length+=source.length;lastBlock=block;
    }
  }
  reveal(count) {
    count=Math.max(0,Math.min(this.length,Math.floor(count)));if(count===this.shown)return false;
    const low=Math.max(0,Math.min(this.shown,count)),high=Math.max(this.shown,count);
    let at=firstAfter(this.leaves,low,leaf=>leaf.end);
    for(;at<this.leaves.length&&this.leaves[at].start<=high;at++){
      const leaf=this.leaves[at];let end=Math.max(0,Math.min(leaf.source.length,count-leaf.start));
      const boundary=i=>i===0||i===leaf.source.length||(!/[\p{M}\u200d\ufe0f]/u.test(leaf.source[i])&&leaf.source[i-1]!=='\u200d');
      while(end>0&&!boundary(end))end--;
      const value=leaf.source.slice(0,end).join('');if(leaf.node.textContent!==value)leaf.node.textContent=value;
    }
    // The first projection must clear future leaves, which still hold SSR text.
    if(this.shown<0)for(const leaf of this.leaves.slice(at))leaf.node.textContent='';
    for(const block of this.blocks){const hidden=count<=block.start&&count<this.length;if(block.node.hidden!==hidden)block.node.hidden=hidden;}
    this.shown=count;return true;
  }
  words(){return firstAfter(this.wordStarts,this.shown-1);}
  // The longest prefix that ends before an unfinished word.
  snap(count){if(count>=this.length)return this.length;const i=firstAfter(this.wordStarts,count)-1;return i<0?0:this.wordStarts[i];}
}

export const fraction=(now,start,finish)=>finish<=start?Number(now>=finish):Math.max(0,Math.min(1,(now-start)/(finish-start)));
// How much of a source is due; streams snap it to whole words.
export const prefixAt=(length,progress)=>progress>=1?length:Math.max(0,Math.floor(length*progress));
export function outputBatches(rows){
  const batches=[];let size=0,start=0;
  for(let i=0;i<rows.length;i++){
    size+=rows[i].length;
    if(i-start>=5||size>=1024){batches.push(i+1);start=i+1;size=0;}
  }
  if(start<rows.length)batches.push(rows.length);
  return batches;
}
export const duration=seconds=>{const n=Math.max(0,Math.floor(seconds));return n<60?n+'s':n<3600?Math.floor(n/60)+'m '+String(n%60).padStart(2,'0')+'s':Math.floor(n/3600)+'h '+String(Math.floor(n%3600/60)).padStart(2,'0')+'m';};
