"""Integration helpers for real Konsole, desktop profile, mouse, and screenshot.
Only the child Konsole/session this fixture creates receives input.
"""
import ctypes
import ctypes.util
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import time

spec=importlib.util.spec_from_file_location('live',Path(__file__).with_name('RuneLiveProviderPTY.py'))
live=importlib.util.module_from_spec(spec)
spec.loader.exec_module(live)


def desktop_environment():
    wanted={'DISPLAY','WAYLAND_DISPLAY','XDG_RUNTIME_DIR','DBUS_SESSION_BUS_ADDRESS','XAUTHORITY'}
    for proc in Path('/proc').glob('[0-9]*/comm'):
        try:
            if proc.read_text().strip()!='konsole':continue
            pairs=(entry.split(b'=',1) for entry in (proc.parent/'environ').read_bytes().split(b'\0') if b'=' in entry)
            env={k.decode():v.decode() for k,v in pairs if k.decode() in wanted}
            if env.get('DBUS_SESSION_BUS_ADDRESS'):return env
        except OSError:pass
    raise RuntimeError('No accessible desktop Konsole environment')


def main():
    provider=live.Provider()
    home,project=live.HOME,live.PROJECT
    (home/'rune.toml').write_text('[ui]\nmouse=true\nsidebar=true\n[host]\nidle_lifetime="1s"\n[providers]\ndefault="fake"\n[[providers.list]]\nname="fake"\n'+f'base_url="http://127.0.0.1:{provider.port}/v1"\n'+'auth={header="Authorization",prefix="Bearer "}\n[[providers.list.models]]\nid="fake-model"\nthinking=false\ncontext=65536\n')
    desktop=desktop_environment()
    env=dict(os.environ,**desktop,RUNE_HOME=str(home),QT_QPA_PLATFORM='xcb')
    provider.queue(live.text('Konsole fixture ready.'))
    # xcb is a real Konsole on the owner's desktop, without stealing input from
    # a pre-existing terminal. XSendEvent later targets this new X window only.
    log=(live.ROOT/'konsole.log').open('w')
    proc=subprocess.Popen(['konsole','--separate','--hide-menubar','--hide-tabbar','--profile','Console','-p','TerminalColumns=120','-p','TerminalRows=40','-e',live.BIN,'-project',str(project)],env=env,stdout=log,stderr=log)
    service=f'org.kde.konsole-{proc.pid}'
    import dbus
    bus=dbus.SessionBus(private=True) if os.environ.get('DBUS_SESSION_BUS_ADDRESS') else dbus.bus.BusConnection(desktop['DBUS_SESSION_BUS_ADDRESS'])
    end=time.monotonic()+15
    while time.monotonic()<end:
        if bus.name_has_owner(service):break
        time.sleep(.05)
    assert bus.name_has_owner(service),'Konsole service not ready'
    session=dbus.Interface(bus.get_object(service,'/Sessions/1'),'org.kde.konsole.Session')
    def text():return str(session.getAllDisplayedText(False))
    def until(predicate,label,timeout=15):
        end=time.monotonic()+timeout
        while time.monotonic()<end:
            if predicate():return
            time.sleep(.03)
        raise AssertionError(label+'\n'+text())
    until(lambda:'fake-model' in text(),'Konsole first frame')
    metadata={'root':str(live.ROOT),'service':service,'konsole_pid':proc.pid,'rune_pid':int(session.foregroundProcessId()),'desktop':desktop}
    (live.ROOT/'metadata.json').write_text(json.dumps(metadata,indent=2)+'\n')
    (Path(__file__).resolve().parents[2]/'target/ui-verification-konsole-root.txt').write_text(str(live.ROOT)+'\n')
    print('READY',json.dumps(metadata),flush=True)
    # This runner remains alive to answer local provider requests; its lifetime
    # is controlled by the explicit fixture stop file, not a live user process.
    try:
        while proc.poll() is None and not (live.ROOT/'stop').exists():
            time.sleep(.2)
    finally:
        if proc.poll() is None:
            session.sendText('\x03\x03')
            try:proc.wait(timeout=10)
            except subprocess.TimeoutExpired:proc.terminate();proc.wait(timeout=5)
        provider.server.shutdown()
        log.close()
        print('Konsole fixture closed',flush=True)


if __name__=='__main__':
    main()
