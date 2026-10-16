"""Verify the dedicated RuneKonsoleUI child using Konsole's real emulation.
All DBus input and screenshots target only the test process recorded in metadata.
"""
from pathlib import Path
import dbus
import json
import os
import re
import subprocess
import time

WORKSPACE=Path(__file__).resolve().parents[2]
ROOT=Path((WORKSPACE/'target/ui-verification-konsole-root.txt').read_text().strip())
META=json.loads((ROOT/'metadata.json').read_text())
bus=dbus.bus.BusConnection(META['desktop']['DBUS_SESSION_BUS_ADDRESS'])
s=dbus.Interface(bus.get_object(META['service'],'/Sessions/1'),'org.kde.konsole.Session')


def text():return str(s.getAllDisplayedText(False))
def lines():return text().splitlines()
def wait(predicate,label,timeout=12):
    end=time.monotonic()+timeout
    while time.monotonic()<end:
        if predicate():return
        time.sleep(.035)
    (ROOT/'FailedKonsole.txt').write_text(text())
    raise AssertionError(label+'\n'+text())
def send(value):s.sendText(value)
def find(needle):
    row=next((n for n,value in enumerate(lines()) if needle in value),None)
    assert row is not None,needle+'\n'+text()
    return row

def click(row,col):
    # Proven from the installed Konsole implementation: 0 press, 2 release.
    # Konsole itself produces the mouse encoding consumed by Rune.
    s.sendMouseEvent(0,col+1,row+1,0)
    s.sendMouseEvent(0,col+1,row+1,2)

def click_text(needle):
    row=find(needle);col=lines()[row].find(needle);click(row,col)

def x_window():
    env=dict(os.environ,**META['desktop'])
    tree=subprocess.check_output(['xwininfo','-root','-tree'],env=env,text=True)
    for wid in re.findall(r'(0x[0-9a-f]+).*\("konsole" "konsole"\)',tree):
        pid=subprocess.check_output(['xprop','-id',wid,'_NET_WM_PID'],env=env,text=True)
        if str(META['konsole_pid']) in pid:return wid
    raise AssertionError('Own Konsole X window not found')
WINDOW=x_window()

def screenshot(name):
    env=dict(os.environ,**META['desktop'])
    subprocess.run(['import','-window',WINDOW,str(ROOT/(name+'.png'))],env=env,check=True,timeout=15)
    (ROOT/(name+'.txt')).write_text(text())


def draft_text():
    path=ROOT/'Home'/'drafts.json'
    if not path.exists():return ''
    for project in json.loads(path.read_text()).get('projects',{}).values():
        for entry in project.get('entries',[]):return entry['composer']['text']
    return ''

def clear():
    # Test input only: Home + exact source-byte count Delete, not Ctrl+U on
    # just the final line. Allows multiline drafts to be tested cleanly.
    # Only caller-prepared drafts are cleared. A visible edit can precede its
    # 250 ms durable save; wait for the visible single-line value when possible.
    editable=[]
    for row in lines():
        if row.startswith('│ › '):editable=[row[4:].rsplit('│',1)[0].rstrip()]
        elif editable and row.startswith('│   '):editable.append(row[4:].rsplit('│',1)[0].rstrip())
        elif editable:break
    visible='\n'.join(editable) if editable else None
    if visible is not None and visible!='ask, or / for commands':
        wait(lambda:draft_text()==visible,'draft persistence before fixture clear')
    value=draft_text()
    send('\x1b[H'+('\x1b[3~'*len(value)))
    wait(lambda:'ask, or / for commands' in text() and draft_text()=='','clear draft')


def main():
    clear()
    send('1234');wait(lambda:'› 1234' in text(),'1234 input')
    click(find('› 1234'),6);send('X')
    wait(lambda:'› 12X34' in text() and draft_text()=='12X34','real Konsole 12|34 insertion')
    screenshot('01-caret-between')
    clear();send('\x1b[200~1\n2\n3\n\x1b[201~')
    wait(lambda:'› 1' in text() and draft_text()=='1\n2\n3\n','multiline input')
    row=find('› 1');click(row,60);send('X')
    wait(lambda:'› 1X' in text() and draft_text()=='1X\n2\n3\n','click blank -> first line end')
    screenshot('02-caret-line-end')
    clear();send('\x07');wait(lambda:'01 / MODELS' in text(),'Settings')
    click_text('01 / MODELS');wait(lambda:'PROVIDERS' in text(),'Models card click')
    click_text('Overview');wait(lambda:'01 / MODELS' in text(),'Overview tab click')
    click_text('Tools');wait(lambda:'Command timeout' in text(),'Tools tab click')
    click_text('Command timeout');wait(lambda:'Apply' in text() and 'Cancel' in text(),'editor click')
    screenshot('03-settings-editor')
    click_text('Cancel');wait(lambda:'Cancel' not in text(),'Cancel button click')
    send('\x1b\x1b');wait(lambda:'ask, or / for commands' in text(),'Settings closes')
    long='\n'.join(f'Konsole-input-{n:02}' for n in range(20))
    send('\x1b[200~'+long+'\x1b[201~');wait(lambda:draft_text()==long and 'Konsole-input-19' in text(),'large composer')
    assert sum('Konsole-input-' in row for row in lines())==11,text()
    screenshot('04-input-eleven-rows')
    send('\r');wait(lambda:'Konsole fixture ready.' in text() and '16 lines cut' in text(),'sent user fold')
    assert 'Konsole-input-19' not in text()
    screenshot('05-user-four-line-preview')
    click_text('16 lines cut');wait(lambda:'Konsole-input-19' in text() and 'lines cut' not in text(),'Open all click')
    screenshot('06-user-expanded')
    click_text('you');wait(lambda:'16 lines cut' in text(),'refold click')
    send('/context\r');wait(lambda:'SYSTEM PROMPT' in text(),'/context')
    assert all(not row.startswith('    │') for row in lines()),text()
    screenshot('07-context-source-clean')
    # Actual wheel via Konsole input path. Read context below and back.
    before=text()
    for _ in range(12):s.sendMouseEvent(65,20,12,0)
    wait(lambda:text()!=before,'context wheel down')
    screenshot('08-context-scrolled')
    send('\x1b');wait(lambda:'Konsole fixture ready.' in text(),'context closes')
    (ROOT/'VerifiedKonsole.json').write_text(json.dumps({'result':'PASS','window':WINDOW,'service':META['service'],'checks':['mouse caret between characters','blank click first-line end','Settings card/tab/editor/Cancel','11 multiline rows','4 user preview rows + expand/refold','context gutter removed','Konsole-generated wheel']},indent=2)+'\n')
    print('PASS real Konsole mouse/caret/Settings/composer/user-fold/context',ROOT)


if __name__=='__main__':main()
