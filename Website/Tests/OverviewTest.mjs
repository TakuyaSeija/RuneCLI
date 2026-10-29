#!/usr/bin/env node
import assert from 'node:assert/strict';
import path from 'node:path';
import {writeFile} from 'node:fs/promises';
import {openBrowser} from './Browser.mjs';
const origin=process.argv[2]||'http://127.0.0.1:18766';
const output=path.resolve(process.argv[3]||'target/test-tmp/website-overview-browser');
const b=await openBrowser(output),report={layouts:[],checks:[]};
try{
 for(const [width,height] of [[320,640],[390,844],[768,1024],[1024,768],[1366,768],[1440,900],[1920,1080]]){
  await b.viewport(width,height,'reduce');await b.navigate(origin+'/Studio/');await b.wait(80);
  await b.evaluate(`document.querySelector('[data-log]').scrollTop=0`);
  // Scroll the session to sit right under the header, as a reader would.
  await b.evaluate(`(()=>{const s=document.querySelector('.stage');scrollTo(0,s.getBoundingClientRect().top+scrollY-document.querySelector('.top').offsetHeight-8)})()`);await b.wait(60);
  const r=await b.evaluate(`(()=>{const q=s=>document.querySelector(s),r=e=>e.getBoundingClientRect(),term=r(q('.term')),logo=r(q('.terminal-logo')),stage=r(q('.stage')),view=r(q('[data-terminal-viewport]')),text=e=>{const x=document.createRange();x.selectNodeContents(e);return x.getBoundingClientRect()},[l1,l2]=[...document.querySelectorAll('.hero h1 .line')].map(text),lead=text(q('.hero .lead'));return{width:innerWidth,height:innerHeight,head:q('.top').offsetHeight,document:document.documentElement.scrollWidth,term:{x:term.x,y:term.y,w:term.width,h:term.height},viewport:{w:view.width,h:view.height},stage:{top:stage.top,bottom:stage.bottom,shown:getComputedStyle(q('.stage')).display!=='none'},still:getComputedStyle(q('.session-still')).display!=='none',font:getComputedStyle(q('[data-terminal-screen]')).fontSize,transform:getComputedStyle(q('.term')).transform,side:getComputedStyle(q('.tside')).display,logo:{w:logo.width,h:logo.height},title:{l1:l1.width,l2:l2.width,l2h:l2.height,line:parseFloat(getComputedStyle(q('.hero h1')).lineHeight)},lead:lead.width,duration:+q('[data-demo]').dataset.schedule.split('|')[1],curl:document.body.textContent.split('curl -fsSL').length-1,zoom:document.querySelectorAll('[data-zoom-in],[data-zoom-out],[data-zoom-reset]').length,chapters:document.querySelectorAll('[data-chapter]').length,images:document.querySelectorAll('main img').length}})()`);
  report.layouts.push(r);assert(r.document<=width,JSON.stringify(r));assert.equal(r.curl,2,'install.sh serves Linux and macOS: one command each');assert.equal(r.zoom,0);assert.equal(r.chapters,6);assert.equal(r.images,0,'the hero is words, not a picture');assert(r.duration>60000&&r.duration<95000);
  // The title widens line by line and the words below run wider still; its second line never wraps.
  assert(r.title.l1<r.title.l2&&r.title.l2<r.lead,'title/lead proportions: '+JSON.stringify(r));assert(r.title.l2h<r.title.line*1.5,'second title line wraps: '+JSON.stringify(r));
  if(width<=600){
   // Phones show the session's own final report, still, instead of a replay too small to read.
   assert(!r.stage.shown&&r.still,'phone shows the replay: '+JSON.stringify(r));
   await b.capture('overview-'+width+'.png');continue;
  }
  assert(r.stage.shown&&!r.still,JSON.stringify(r));
  assert(Math.abs(r.term.h-r.viewport.h)<3,JSON.stringify(r));
  if(width>920){
   // One uniformly scaled 960×600 canvas whose text stays readable and whose
   // whole stage fits the screen under the header.
   assert.equal(r.font,'12px');assert(Math.abs(r.term.w/r.term.h-960/600)<.002,JSON.stringify(r));assert(Math.abs(r.logo.h/r.term.h-163.2/600)<.002);assert(Math.abs(r.logo.w/r.term.w-79.2/960)<.002);
   assert(12*r.term.w/960>=10.5,'terminal text below 10.5px: '+JSON.stringify(r));assert(r.stage.bottom<=r.height+1,'stage taller than the screen: '+JSON.stringify(r));
  }else{
   // Tablets draw the terminal at its own size, one column, no scaling.
   assert.equal(r.transform,'none');assert.equal(r.font,'12px');assert.equal(r.side,'none');
  }
  await b.capture('overview-'+width+'.png');
 }
 await writeFile(path.join(output,'report.json'),JSON.stringify(report,null,2));assert.deepEqual(b.errors,[]);console.log('PASS overview initial canvas geometry: '+report.layouts.length+' widths');
}catch(error){await writeFile(path.join(output,'failure.json'),JSON.stringify({...report,error:error.stack,browserErrors:b.errors},null,2));throw error;}finally{b.close();}
