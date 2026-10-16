"""Submit/next-command stress with actual PTY and an observing session client.
Run: python3 -I Cli/tests/RuneSubmitStressPTY.py path/to/rune [iterations]
No owner's data is used; failed runs retain every protocol event and screen.
"""
import importlib.util
import json
import os
from pathlib import Path
import signal
import socket
import sys
import threading
import time

spec = importlib.util.spec_from_file_location('live', Path(__file__).with_name('RuneLiveProviderPTY.py'))
live = importlib.util.module_from_spec(spec)
spec.loader.exec_module(live)
ITERATIONS = int(sys.argv[2]) if len(sys.argv) > 2 else 60


def cleanup():
    for path in Path('/proc').glob('[0-9]*/cmdline'):
        try:
            args=path.read_bytes()
            if str(live.HOME).encode() in args or str(live.PROJECT).encode() in args:
                os.kill(int(path.parent.name),signal.SIGTERM)
        except OSError: pass


def main():
    provider=live.Provider()
    (live.HOME/'rune.toml').write_text('[host]\nidle_lifetime="1s"\n[providers]\ndefault="fake"\n[[providers.list]]\nname="fake"\n'+f'base_url="http://127.0.0.1:{provider.port}/v1"\n'+'auth={header="Authorization",prefix="Bearer "}\n[[providers.list.models]]\nid="fake-model"\ncontext=65536\nthinking=false\n')
    w=live.Window(['-project',str(live.PROJECT)],rows=46,cols=160)
    recording=None
    try:
        live.until(lambda:w.shows('fake-model'),'first paint',w)
        live.ask(w,provider,'seed',live.text('Seed done.'),expect='Seed done.')
        until=live.HOME/'run'/'session_1.sock'
        live.until(lambda:until.exists(),'session socket',w)
        client=socket.socket(socket.AF_UNIX)
        directory=os.open(until.parent,os.O_RDONLY|os.O_DIRECTORY)
        try:
            client.connect(f'/proc/self/fd/{directory}/{until.name}')
        finally:
            os.close(directory)
        key=(live.HOME/'run'/'session_1.key').read_text().strip()
        client.sendall((json.dumps({'cmd':'session.hello','id':70001,'key':key})+'\n').encode())
        recording=(live.ROOT/'events.jsonl').open('w',buffering=1)
        def observe():
            with client.makefile() as reader:
                for line in reader:
                    try:event=json.loads(line)
                    except ValueError:event={'raw':line}
                    recording.write(json.dumps({'at':time.monotonic(),'event':event})+'\n')
        threading.Thread(target=observe,daemon=True).start()
        # Recreate the old failed scenario: tiny two-tool turns followed by
        # a slash panel immediately when the fast final answer is drawn.
        w.type('/mode auto');w.send(b'\r')
        live.until(lambda:'Auto' in w.screen.text(),'auto mode',w)
        for name in ['a.txt','b.txt']:(live.PROJECT/name).write_text('alpha\n')
        # Observe without issuing commands: all real window command IDs and
        # composer revisions remain visible in the event stream.
        for n in range(ITERATIONS):
            prompt=f'probe-{n:03}-text'
            answer=f'Completed probe {n:03}.'
            before='alpha' if n%2==0 else 'ALPHA'
            after='ALPHA' if n%2==0 else 'alpha'
            provider.queue(live.tool('patch_file',{'action':'a','path':'a.txt','old_str':before,'new_str':after}),live.tool('patch_file',{'action':'b','path':'b.txt','old_str':before,'new_str':after}),live.text(answer))
            w.type(prompt)
            w.send(b'\r')
            live.until(lambda:w.shows(answer),answer,w)
            w.type('/undo')
            w.send(b'\r')
            live.until(lambda:w.shows('UNDO') and w.shows('a.txt') and w.shows('b.txt'),f'undo after probe {n}',w,timeout=8)
            w.send(b'\x1b')
            live.until(lambda:w.shows(answer) and not w.shows('alt+s show diff'),'undo closes',w)
            composer=list(live.HOME.glob('projects/*/sessions/1/composer.json'))[0]
            state=json.loads(composer.read_text())
            if state.get('text') not in ('','/context'):
                raise AssertionError(f'probe {n}: old prompt resurrected in composer: {state}')
            print('ok probe',n,flush=True)
        rows=live.transcript()
        user=[r['content'] for r in rows if r.get('role')=='user']
        assert user==['seed']+[f'probe-{n:03}-text' for n in range(ITERATIONS)],user
        print('PASS submit/next-command stress',ITERATIONS,live.ROOT,flush=True)
    except BaseException:
        (live.ROOT/'FailedScreen.txt').write_text(w.screen.text())
        (live.ROOT/'Requests.json').write_text(json.dumps(provider.calls,indent=2))
        print('FAILED submit stress; artifacts:',live.ROOT,flush=True)
        raise
    finally:
        w.close()
        if 'client' in locals():client.close()
        cleanup()


if __name__=='__main__':
    main()
