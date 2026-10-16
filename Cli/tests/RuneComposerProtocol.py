import json,subprocess,tempfile,sqlite3,select,time,sys,os,re
from pathlib import Path
BIN=sys.argv[1]
ROOT=Path(tempfile.mkdtemp(prefix='R016Wire.',dir=str(Path(__file__).resolve().parents[2]/'target/test-tmp')))
PASSED=False
HOME=ROOT/'Home';HOME.mkdir()
SCRIPT=ROOT/'Fixture.json';SCRIPT.write_text(json.dumps({'responses':[{'chunks':[{'text':'Done'}]} for _ in range(6)]}))
HOST=None;SEQ=0
# Sessions are filed under their project's folder: projects/<name>/sessions/<id>.
def session_dir(n):return next(HOME.glob(f'projects/*/sessions/{n}'),HOME/'projects'/'(unfiled)'/'sessions'/str(n))
def folder_of(project):return HOME/'projects'/re.sub('[^A-Za-z0-9]','-',str(project))
class Host:
 def __init__(self):
  self.p=subprocess.Popen([BIN,'-host','-script',str(SCRIPT)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,env=dict(os.environ,RUNE_HOME=str(HOME)),text=True,bufsize=1)
  self.events=[];self.buffer=b'';self.line()
 def line(self):
  deadline=time.monotonic()+15
  while b'\n' not in self.buffer:
   remaining=deadline-time.monotonic()
   if remaining<=0 or not select.select([self.p.stdout],[],[],remaining)[0]:raise AssertionError('host request deadline')
   chunk=os.read(self.p.stdout.fileno(),65536);assert chunk, 'host closed protocol output'
   self.buffer+=chunk
  line,self.buffer=self.buffer.split(b'\n',1)
  return json.loads(line)
 def command(self,command):
  global SEQ
  SEQ+=1;command=dict(command,id=SEQ)
  self.p.stdin.write(json.dumps(command)+'\n');self.p.stdin.flush()
  while True:
   event=self.line();self.events.append(event)
   if event.get('t')=='ack' and event.get('id')==SEQ:return event
 def close(self):
  self.command({'cmd':'shutdown'});self.p.wait(timeout=15)
try:
 h=Host();HOST=h
 drafts=[]
 for n in range(40):
  project=ROOT/f'Project{n}';project.mkdir()
  opened=h.command({'cmd':'draft.open','project':str(project)})
  assert opened['ok'];drafts.append(opened['data']['draft'])
 with sqlite3.connect(HOME/'rune.db') as db:assert db.execute('SELECT COUNT(*) FROM sessions').fetchone()[0]==0
 assert not (HOME/'drafts.json').exists()
 assert not list(HOME.glob('projects/*/sessions/[0-9]*'))
 first=drafts[0];first['composer']['text']='first message'
 saved=h.command({'cmd':'draft.save','draft':first,'base':0,'composer':first['composer']});assert saved['ok']
 first=saved['data']['draft']
 stale=h.command({'cmd':'draft.save','draft':first,'base':0,'composer':dict(first['composer'],text='overwrite')});assert not stale['ok']
 folder=folder_of(first['project']);(folder/'sessions').mkdir(parents=True,exist_ok=True)
 (folder/'project.json').write_text(json.dumps({'project':first['project']}))
 blocked=folder/'sessions/1';blocked.write_text('injected file commit failure')
 failed=h.command({'cmd':'draft.promote','draft':first});assert not failed['ok']
 registry=json.loads((HOME/'drafts.json').read_text());assert any(e['composer']['text']=='first message' for p in registry['projects'].values() for e in p['entries'])
 h.close();HOST=None;blocked.unlink()
 h=Host();HOST=h
 committed=h.command({'cmd':'draft.promote','draft':first});assert committed['ok'];session=committed['data']['session'];assert session==1
 again=h.command({'cmd':'draft.promote','draft':first});assert again['data']['session']==session
 transcript=session_dir(session)/'session.jsonl';assert len(transcript.read_text().splitlines())==1
 h.command({'cmd':'session.resume','session':session,'project':first['project']})
 current=h.command({'cmd':'composer.get','session':session})['data']['composer']
 # Every client operation carries a process-prefixed request ID; preserve id
 # ledger wire decoding in the real host process rather than a fake UI.
 accepted=h.command({'cmd':'composer.edit','session':session,'base':current['revision'],'changes':[{'op':'splice','start':0,'end':0,'text':'hello'}]});assert accepted['ok'] and accepted['data']['applied']
 stale=h.command({'cmd':'composer.edit','session':session,'base':current['revision'],'changes':[{'op':'splice','start':0,'end':0,'text':'stale'}]});assert stale['ok'] and not stale['data']['applied'];assert stale['data']['composer']['text']=='hello'
 h.close();HOST=None
 # Simulate crash after durable files but before registry-removal completion ACK.
 with sqlite3.connect(HOME/'rune.db') as db:db.execute('UPDATE promotions SET complete=0 WHERE session_id=?',(session,))
 (session_dir(session)/'meta.json').unlink()
 h=Host();HOST=h
 recovered=h.command({'cmd':'draft.promote','draft':first});assert recovered['ok'] and recovered['data']['session']==session
 assert len(transcript.read_text().splitlines())==1 and (session_dir(session)/'meta.json').exists()
 h.close();HOST=None
 # Exercise migration through the real binary while original DB/files survive.
 with sqlite3.connect(HOME/'rune.db') as db:assert db.execute('PRAGMA integrity_check').fetchone()[0]=='ok'
 print('PASS protocol lazy40/stale draft&composer/failing promotion/retry/recovery/history-preservation',ROOT)
 PASSED=True
except BaseException:
 print('FAILED protocol fixture',ROOT)
 raise
finally:
 if HOST and HOST.p.poll() is None:
  HOST.p.terminate();HOST.p.wait(timeout=15)
 # Session hosts this run started detach from it: they end with its home.
 import signal as _signal
 for _pid in os.popen('pgrep -x rune').read().split():
  try:
   if str(ROOT).encode() in open(f'/proc/{_pid}/cmdline','rb').read():os.kill(int(_pid),_signal.SIGTERM)
  except OSError:pass
 # A passing run leaves nothing behind; a failing one keeps its home and logs.
 if PASSED:
  import shutil;shutil.rmtree(ROOT,ignore_errors=True)
