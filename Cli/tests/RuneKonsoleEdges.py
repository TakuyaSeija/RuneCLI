"""Real Konsole edge cases after RuneKonsoleVerify, on its isolated session."""
import importlib.util
from pathlib import Path
import fcntl
import json
import os
import struct
import subprocess
import termios
import time

spec=importlib.util.spec_from_file_location('verify',Path(__file__).with_name('RuneKonsoleVerify.py'))
v=importlib.util.module_from_spec(spec);spec.loader.exec_module(v)
env=dict(os.environ,**v.META['desktop'])
workspace=Path(__file__).resolve().parents[2]


def geometry():
    tty=os.readlink(f'/proc/{v.META["rune_pid"]}/fd/0')
    with open(tty,'rb',buffering=0) as f:
        return struct.unpack('HHHH',fcntl.ioctl(f,termios.TIOCGWINSZ,b'\0'*8))[:2]


def resize(width,height):
    old=geometry()
    script=v.ROOT/'resize-own-window.js'
    script.write_text(f'for (const w of workspace.windowList()) {{ if (w.pid === {v.META["konsole_pid"]}) {{ w.tile = null; w.frameGeometry = {{x:100,y:80,width:{width},height:{height}}}; }} }}')
    label=f'RuneFixtureResize-{v.META["konsole_pid"]}'
    idx=int(v.bus.call_blocking('org.kde.KWin','/Scripting','org.kde.kwin.Scripting','loadScript','ss',(str(script),label)))
    assert idx>=0
    try:
        v.bus.call_blocking('org.kde.KWin',f'/Scripting/Script{idx}','org.kde.kwin.Script','run','',())
        if (old[1]*8,old[0]*15)!=(width,height):
            v.wait(lambda:geometry()!=old or abs(geometry()[1]*8-width)<30,'Konsole geometry changes')
        v.send('\x0c')
        return geometry()
    finally:
        v.bus.call_blocking('org.kde.KWin','/Scripting','org.kde.kwin.Scripting','unloadScript','s',(label,))


def main():
    # Session now exists; draft getter is replaced with its authoritative
    # durable composer. Do not connect to any user session/home.
    composer=next((v.ROOT/'Home').glob('projects/*/sessions/1/composer.json'))
    def input_text():return json.loads(composer.read_text())['text']
    v.draft_text=input_text
    v.send('\x1b[H'+'\x1b[3~'*len(input_text()))
    v.wait(lambda:input_text()=='' and 'ask, or / for commands' in v.text(),'empty session composer')
    resize(1000,620)
    long='\n'.join(f'Edge-row-{n:02}' for n in range(25))
    v.send('\x1b[200~'+long+'\x1b[201~')
    v.wait(lambda:input_text()==long and 'Edge-row-24' in v.text(),'long input')
    assert sum('Edge-row-' in row for row in v.lines())==11,v.text()
    v.screenshot('10-edge-input-11')
    resize(520,190)
    v.wait(lambda:'Edge-row-24' in v.text(),'short terminal reflow')
    rows,cols=geometry()
    assert sum('Edge-row-' in row for row in v.lines())<=max(1,rows-5),v.text()
    v.screenshot('11-edge-input-short')
    resize(1000,620)
    v.wait(lambda:'Edge-row-24' in v.text(),'restore terminal')
    v.send('\x1b[H'+'\x1b[3~'*len(long))
    v.wait(lambda:input_text()=='' and 'ask, or / for commands' in v.text(),'clear long input')
    rows,cols=geometry();width=cols-6
    glyph='👨\u200d💻'
    sample='x'*(width-2)+glyph+'Q'
    v.send('\x1b[200~'+sample+'\x1b[201~')
    v.wait(lambda:input_text()==sample and glyph in v.text(),'Unicode edge input')
    editable=[row for row in v.lines() if row.startswith('│ › ') or row.startswith('│   ')]
    assert any(glyph in row for row in editable),editable
    assert not any(row.rstrip().endswith('👨\u200d') for row in editable),editable
    v.screenshot('12-edge-unicode-wrap')
    qrow=next(n for n,row in enumerate(v.lines()) if row.startswith('│   Q'))
    v.click(qrow,4);v.send('X')
    v.wait(lambda:input_text()==sample[:-1]+'XQ','Unicode wrap click source offset')
    v.screenshot('13-edge-unicode-caret')
    v.send('\x1b[H'+'\x1b[3~'*len(input_text()))
    v.wait(lambda:input_text()=='' and 'ask, or / for commands' in v.text(),'clear Unicode input')
    v.send('/context\r');v.wait(lambda:'SYSTEM PROMPT' in v.text(),'context after Unicode')
    v.send('\x0f');v.wait(lambda:'TokOut' in v.text(),'context split toggle on')
    v.screenshot('14-edge-context-split')
    before=v.text()
    for _ in range(14):v.s.sendMouseEvent(65,20,12,0)
    v.wait(lambda:v.text()!=before,'context split scroll')
    resize(740,500);v.wait(lambda:'CONTEXT' in v.text() or '<' in v.text(),'context resize')
    assert not any(row.startswith('    │') for row in v.lines()),v.text()
    v.screenshot('15-edge-context-resized')
    v.send('\x1b');v.wait(lambda:'Konsole fixture ready.' in v.text() or '16 lines cut' in v.text(),'context close after resize')
    (v.ROOT/'VerifiedKonsoleEdges.json').write_text(json.dumps({'result':'PASS','checks':['real window resize large/short','input 11 and short-screen clamp','joined glyph intact at wrap','caret source offset after glyph wrap','context split/scroll/resize no gutter']},indent=2)+'\n')
    print('PASS real Konsole resize/split/Unicode/caret edges',v.ROOT)


if __name__=='__main__':main()
