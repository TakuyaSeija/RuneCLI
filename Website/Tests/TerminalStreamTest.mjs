import assert from 'node:assert/strict';
import {TextStream} from '../UI/Static/Scripts/TerminalStream.js';
globalThis.NodeFilter={SHOW_TEXT:4};
function fixture(parts){
 const root={querySelectorAll:()=>[]},leaves=[];let writes=0;
 for(const [source,id] of parts){const block={id,hidden:false};const parent={closest:()=>block};let value=source;const node={parentElement:parent,get textContent(){return value;},set textContent(next){value=next;writes++;}};leaves.push(node);}
 globalThis.document={createTreeWalker:()=>{let i=0;return{nextNode:()=>leaves[i++]||null};}};
 return{root,leaves,writes:()=>writes,text:()=>leaves.map(n=>n.textContent).join('')};
}
const f=fixture([['first ',0],['second ',1],['third ',2],['fourth',3]]),s=new TextStream(f.root);
s.reveal(0);assert.equal(f.text(),'');s.reveal(8);assert.equal(f.text(),'first se');const writes=f.writes();s.reveal(9);assert.equal(f.writes()-writes,1);assert.equal(s.words(),2);
s.reveal(s.length);assert.equal(f.text(),'first second third fourth');s.reveal(4);assert.equal(f.text(),'firs');s.reveal(s.length);assert.equal(f.text(),'first second third fourth');
const unicode=fixture([['A🌏e\u0301Z',0]]),u=new TextStream(unicode.root);u.reveal(3);assert.equal(unicode.text(),'A🌏');u.reveal(u.length);assert.equal(unicode.text(),'A🌏e\u0301Z');
// Provider text arrives whole words: a due prefix never ends inside a word.
const w=fixture([['first ',0],['second ',1],['third',2]]),ws=new TextStream(w.root);
for(const [due,want] of [[0,''],[3,''],[6,'first '],[9,'first '],[13,'first second '],[17,'first second '],[18,'first second third'],[99,'first second third']]){ws.reveal(ws.snap(due));assert.equal(w.text(),want,String(due));}
console.log('PASS incremental text stream: future clearing, reset/reveal, changed-leaf writes, shown words, Unicode and whole-word arrival');
