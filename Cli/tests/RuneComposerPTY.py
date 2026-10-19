import os,subprocess,time,json,sqlite3,tempfile,sys,socket
from pathlib import Path
sys.path.insert(0,str(Path(__file__).resolve().parent))
import RuneTerminal
ROOT=Path(tempfile.mkdtemp(prefix='R016.',dir=str(Path(__file__).resolve().parents[2]/'target/test-tmp')))
PASSED=False
HOME=ROOT/'Home'; HOME.mkdir()
PROJECT=ROOT/'Project'; PROJECT.mkdir()
BIN=sys.argv[1] if len(sys.argv)>1 else str(Path(__file__).resolve().parents[2]/'target/debug'/('rune.exe' if RuneTerminal.WINDOWS else 'rune'))
(HOME/'fixture.json').write_text(json.dumps({'responses':[{'chunks':[{'text':'Fixture completed.'}]} for _ in range(10)]}))
(HOME/'rune.toml').write_text('[host]\nidle_lifetime = "1s"\n[ui]\nsidebar = true\n[providers]\ndefault = "fixture"\n[[providers.list]]\nname = "fixture"\nbase_url = "http://127.0.0.1:1"\napi = "openai"\nauth = { header = "Authorization", prefix = "Bearer " }\n[[providers.list.models]]\nid = "test"\nthinking = false\ncontext = 32768\n[[providers.list.models]]\nid = "other"\nthinking = false\ncontext = 65536\n')
WINDOWS=[]
class Window:
 def __init__(self,resume=None,project=PROJECT):
  env=dict(os.environ,RUNE_HOME=str(HOME),TERM='xterm-256color',COLORTERM='truecolor')
  args=[BIN,'-project',str(project),'-script',str(HOME/'fixture.json')]
  if resume is not None:args+=['-resume',str(resume)]
  self.p=RuneTerminal.Terminal(args,env,34,120);self.output=b'';WINDOWS.append(self)
 def drain(self):self.output+=self.p.read()
 def send(self,data):self.p.write(data);self.drain()
 def close(self):
  if self.p.alive():
   self.send(b'\x03\x03')
   until(lambda:not self.p.alive(),'clean exit process',timeout=20)
  self.drain()
def until(predicate,description,timeout=15):
 end=time.monotonic()+timeout
 while time.monotonic()<end:
  for w in WINDOWS:w.drain()
  if predicate():return
  time.sleep(.025)
 raise AssertionError(description)
# Sessions are filed under their project's folder: projects/<name>/sessions/<id>.
def session_dir(n):return next(HOME.glob(f'projects/*/sessions/{n}'),HOME/'projects'/'(unfiled)'/'sessions'/str(n))
def registry():
 p=HOME/'drafts.json'
 return json.loads(RuneTerminal.read_document(p)) if p.exists() else {'projects':{}}
def entries():return next(iter(registry()['projects'].values()))['entries'] if registry()['projects'] else []
def session_count():
 with sqlite3.connect(HOME/'rune.db') as db:return db.execute('select count(*) from sessions').fetchone()[0]
def composer(session):return json.loads(RuneTerminal.read_document(session_dir(session)/'composer.json'))
try:
 a=Window();until(lambda:(HOME/'rune.db').exists(),'startup database');until(lambda:b'Select Models' in a.output or b'test' in a.output,'first paint')
 assert session_count()==0 and not list(HOME.glob('projects/*/sessions/[0-9]*'))
 a.send(b'a');until(lambda:len(entries())==1 and entries()[0]['composer']['text']=='a','draft a persisted')
 b=Window();until(lambda:b'test' in b.output,'second paint');b.send(b'\x15b');until(lambda:len(entries())==2 and entries()[-1]['composer']['text']=='b','independent b persisted')
 assert entries()[0]['composer']['text']=='a' and session_count()==0
 b.send(b'\x1b[5~\x1b[5~');until(lambda:entries()[-1]['composer']['text']=='a','PageUp draft history recalls a')
 b.send(b'\x15b\r');until(lambda:session_count()==1,'promoted first message')
 until(lambda:(session_dir(1)/'session.jsonl').exists(),'per-session transcript')
 until(lambda:'Fixture completed.' in (session_dir(1)/'session.jsonl').read_text(),'first model turn completed')
 until(lambda:not entries(),'project draft history consumed')
 a.send(b'c');until(lambda:len(entries())==1 and entries()[0]['composer']['text']=='c','peer consumed old a before next c')
 text=(session_dir(1)/'session.jsonl').read_text();assert sum(json.loads(line)['role']=='user' for line in text.splitlines())==1
 c=Window(1);until(lambda:b'Fixture completed.' in c.output,'explicit resume paint')
 b.send(b'hello');until(lambda:composer(1)['text']=='hello','shared typing persists')
 until(lambda:b'hello' in c.output,'peer receives typing without input')
 c.send(b'\x05\x7f');until(lambda:composer(1)['text']=='hell','peer delete persists')
 until(lambda:b'hell' in b.output,'sender receives peer delete')
 attached=PROJECT/'File.txt';attached.write_text('fixture attachment')
 b.send(('\x15/attach '+str(attached)+'\r').encode());until(lambda:str(attached) in composer(1)['attachments'],'attachment shared persisted')
 until(lambda:b'File.txt' in c.output,'peer attachment visible')
 c.send(b'\x15/detach all\r');until(lambda:not composer(1)['attachments'],'remote detach persisted')
 # Concurrent insertions remain accepted without echo loops.
 b.send(b'\x05\x15');until(lambda:composer(1)['text']=='','shared clear')
 # Deliberately do not wait for peer UI/ACK: clear must safely overlap
 # the next insert, including the peer's just-submitted slash-command clear.
 b.send(b'X');c.send(b'Y');until(lambda:sorted(composer(1)['text'])==['X','Y'],'concurrent inserts preserve both')
 b.send(b'\x05\x15/set temperature=0.5\r');until(lambda:composer(1)['settings'].get('temperature')==0.5,'generation setting durable')
 c.send(b'\x05\x15/model other\r');until(lambda:composer(1)['settings']['model']=='other','shared model update')
 until(lambda:b'other' in b.output,'model fanout no sender action')
 # One Enter submits after any pending composer edit automatically.
 b.send(b'\x05\x15second message\r');until(lambda:sum(json.loads(line)['role']=='user' for line in (session_dir(1)/'session.jsonl').read_text().splitlines())==2,'single Enter commits second message')
 until(lambda:composer(1)['text']=='','sent composer clears in authority')
 b.send(b'unsent-tail');until(lambda:composer(1)['text']=='unsent-tail','unsent tail durable')
 # Clearing the line to type a command is an edit like any other: the
 # session keeps what the person left, deterministically.
 b.send(b'\x15/new\r');until(lambda:b'Project draft' in b.output,'new stays draft')
 until(lambda:composer(1)['text']=='','the clear before /new is kept')
 assert session_count()==1
 # Words left in the session while this window was away are on its screen
 # when it comes back — read from what it drew after the resume, not from
 # everything it ever printed.
 c.send(b'\x05\x15unsent-tail');until(lambda:composer(1)['text']=='unsent-tail','peer tail durable')
 mark=len(b.output)
 b.send(b'\x15/resume 1\r');until(lambda:b'unsent-tail' in b.output[mark:],'resume restores target composer')
 # Invalid attachment is removed from storage as well as a restored UI.
 b.send(('\x05\x15/attach '+str(attached)+'\r').encode());until(lambda:str(attached) in composer(1)['attachments'],'reattach before restore')
 attached.unlink()
 restored=Window(1);until(lambda:b'Fixture completed.' in restored.output,'restored frontend first paint');until(lambda:not composer(1)['attachments'],'missing attachment persistently removed on restore')
 restored.send(b'\x05\x15fast-exit');restored.close();until(lambda:composer(1)['text']=='fast-exit','clean exit flush preserves exactly one edit')
 # Bare launch in another project never creates a permanent ID or inherits A input.
 other_project=ROOT/'OtherProject';other_project.mkdir()
 other=Window(project=other_project);until(lambda:b'test' in other.output or b'other' in other.output,'other project paint')
 other.send(b'project-b');until(lambda:any(v['project']==str(other_project) and any(e['composer']['text']=='project-b' for e in v['entries']) for v in registry()['projects'].values()),'project B composer isolated')
 assert session_count()==1
 a.send(b'\x05\x15project-a');until(lambda:any(v['project']==str(PROJECT) and any(e['composer']['text']=='project-a' for e in v['entries']) for v in registry()['projects'].values()),'project A draft preserved separately')
 print('PASS draft/history/promotion/generation/shared text/attachments/concurrency/settings/submit/resume/exit/isolation',ROOT)
 PASSED=True
 for w in WINDOWS:w.close()
except BaseException:
 for i,w in enumerate(WINDOWS):
  w.drain();(ROOT/f'Window{i}.log').write_bytes(w.output)
 print('FAILED PTY fixture logs:',ROOT)
 raise
finally:
 for w in WINDOWS:
  if w.p.alive():
   try:w.close()
   except Exception:w.p.kill()
 # Session hosts this run started detach from it: they end with its home.
 import signal as _signal
 for _pid in RuneTerminal.holding(ROOT):RuneTerminal.stop(_pid)
 # A passing run leaves nothing behind; a failing one keeps its home and logs.
 if PASSED:
  import shutil;shutil.rmtree(ROOT,ignore_errors=True)
