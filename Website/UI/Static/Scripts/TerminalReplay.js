/* The only replay state machine. What a step shows is recorded; arrival speed,
   reading pauses and approvals are labelled presentation of an excerpt. */
export function readSchedule(encoded) {
  const fields = encoded.split('|');
  if (fields.length !== 13) throw new Error('Invalid replay schedule');
  const rows = index => fields[index] ? fields[index].split(';').map(row => row.split(',').map(Number)) : [];
  return {end:+fields[1], typeStart:+fields[2], typeEnd:+fields[3], active:+fields[4], hold:+fields[5], steps:rows(6), turns:rows(7), plans:rows(8), files:rows(9).map(row=>row[0]), clock:rows(10), counts:rows(11), chapters:rows(12).map(row=>row[0])};
}
export const latest = (rows, at) => {
  let lo=0,hi=rows.length;
  while(lo<hi){const m=(lo+hi)>>>1;if(rows[m][0]<=at)lo=m+1;else hi=m;}
  return lo-1;
};
/* The recorded moment a playback moment shows. Two keyframes at one playback
   moment are a cut: the excerpt skips recorded time there, it never runs it. */
export function sourceAt(clock, now) {
  const i=latest(clock,now);
  if(i<0)return 0;
  const a=clock[i],b=clock[i+1];
  if(!b||b[0]===a[0])return a[1];
  return a[1]+(b[1]-a[1])*Math.min(1,(now-a[0])/(b[0]-a[0]));
}
export function phase(row, now, tool) {
  const [start,dwell,,,gateEnd,streamEnd,runAt,,gateAt]=row;
  if(now<start)return 'future';
  if(now<streamEnd)return 'writing';
  if(!tool)return 'done';
  if(gateAt && now>=gateAt && now<gateEnd)return 'asking';
  if(now<runAt)return 'queued';
  return now<start+dwell ? 'running' : 'done';
}
export const initialState = () => ({elapsed:0, completeHold:0, paused:false, visible:true, hidden:false, reduced:false, explicitMotion:false, last:null});
export const canAdvance = state => state.visible && !state.hidden && !state.paused && (!state.reduced || state.explicitMotion);
export const wantsPlay = (state,end) => !state.paused && (!state.reduced || state.explicitMotion) && !(state.reduced && state.elapsed>=end);
export function reduce(state, event, schedule) {
  const next={...state};
  switch(event.type){
    case 'play': next.paused=false;next.explicitMotion=true;if(next.elapsed>=schedule.end){next.elapsed=0;next.completeHold=0;}break;
    case 'pause':next.paused=true;break;
    case 'replay':next.elapsed=0;next.completeHold=0;next.paused=false;next.explicitMotion=true;break;
    case 'visibility':next.visible=event.visible;next.hidden=event.hidden;break;
    case 'motion':next.reduced=event.reduced;next.explicitMotion=false;next.elapsed=event.reduced?schedule.end:0;next.completeHold=0;break;
    case 'approval':next.elapsed=event.at;break;
    case 'seek':next.elapsed=Math.max(0,Math.min(schedule.end,event.at));next.completeHold=0;break;
    case 'frame':{
      if(!canAdvance(next)){next.last=null;return next;}
      const delta=next.last===null?0:Math.max(0,event.now-next.last);next.last=event.now;
      if(next.elapsed>=schedule.end){
        if(!next.reduced)next.completeHold+=delta;
        if(next.completeHold>=schedule.hold){next.elapsed=0;next.completeHold=0;}
      }else next.elapsed=Math.min(schedule.end,next.elapsed+delta);
      return next;
    }
  }
  next.last=null;
  return next;
}
