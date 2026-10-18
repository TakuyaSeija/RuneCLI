"""The terminal and the processes the PTY fixtures drive, the same on every OS.

A fixture talks to a window as a terminal does — bytes in, bytes out, a size —
so what it proves holds wherever the terminal is a real one: a pseudo-terminal
on Linux and macOS, a ConPTY on Windows (what Windows Terminal hosts a program
in), through `pywinpty` (CI installs it; nothing of Rune's needs it). Output is
read by a thread on every OS, so a fixture never blocks on a quiet window.

The processes a fixture started — a window, the session hosts it launched —
are found by what they carry: their command line or their `RUNE_HOME`. Linux
reads `/proc`; elsewhere `psutil` (installed by CI) reads the same facts.
"""
import os, queue, signal, subprocess, sys, threading, time

WINDOWS = sys.platform == "win32"
LINUX = sys.platform.startswith("linux")


class Terminal:
    """A program running in a terminal of `rows` x `cols`."""

    def __init__(self, argv, env, rows, cols, cwd=None):
        self.rows, self.cols = rows, cols
        self._chunks = queue.Queue()
        if WINDOWS:
            from winpty import PtyProcess

            self._proc = PtyProcess.spawn([str(a) for a in argv], cwd=cwd, env=env,
                                          dimensions=(rows, cols))
            self.pid = self._proc.pid
            reader = self._read_conpty
        else:
            import fcntl, pty, struct, termios

            self.master, slave = pty.openpty()
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
            self._proc = subprocess.Popen(
                [str(a) for a in argv], stdin=slave, stdout=slave, stderr=slave,
                env=env, cwd=cwd, start_new_session=True,
            )
            os.close(slave)
            self.pid = self._proc.pid
            reader = self._read_pty
        threading.Thread(target=reader, daemon=True).start()

    def _read_pty(self):
        while True:
            try:
                data = os.read(self.master, 65536)
            except OSError:
                break
            if not data:
                break
            self._chunks.put(data)

    def _read_conpty(self):
        while True:
            try:
                text = self._proc.read(65536)
            except EOFError:
                break
            except Exception:
                if not self._proc.isalive():
                    break
                time.sleep(0.01)
                continue
            if text:
                self._chunks.put(text.encode("utf-8", "surrogateescape") if isinstance(text, str) else text)

    def read(self):
        """Everything written since the last read; `b""` when nothing was."""
        out = bytearray()
        while True:
            try:
                out.extend(self._chunks.get_nowait())
            except queue.Empty:
                return bytes(out)

    def write(self, data):
        if WINDOWS:
            self._proc.write(data.decode("utf-8", "surrogateescape"))
        else:
            os.write(self.master, data)

    def resize(self, rows, cols):
        self.rows, self.cols = rows, cols
        if WINDOWS:
            self._proc.setwinsize(rows, cols)
        else:
            import fcntl, struct, termios

            fcntl.ioctl(self.master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
            try:
                os.killpg(self.pid, signal.SIGWINCH)
            except OSError:
                pass

    def alive(self):
        return self._proc.isalive() if WINDOWS else self._proc.poll() is None

    def wait(self, timeout):
        """True once the program ended within `timeout` seconds."""
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            if not self.alive():
                return True
            time.sleep(0.05)
        return not self.alive()

    def kill(self):
        if WINDOWS:
            try:
                self._proc.terminate(force=True)
            except Exception:
                pass
        else:
            self._proc.kill()
            self._proc.wait(5)


def processes():
    """Every process of this user that can be read: (pid, argv bytes, environ bytes)."""
    if LINUX:
        from pathlib import Path

        for entry in Path("/proc").iterdir():
            if not entry.name.isdigit():
                continue
            try:
                yield int(entry.name), (entry / "cmdline").read_bytes(), (entry / "environ").read_bytes()
            except OSError:
                continue
        return
    import psutil

    for proc in psutil.process_iter():
        try:
            argv = "\0".join(proc.cmdline()).encode()
        except (psutil.Error, OSError):
            continue
        try:
            environ = "\0".join(f"{k}={v}" for k, v in proc.environ().items()).encode()
        except (psutil.Error, OSError):
            environ = b""
        yield proc.pid, argv, environ


def holding(*texts):
    """Pids of the processes whose command line or environment holds any of `texts`."""
    wanted = [str(t).encode() for t in texts]
    me = os.getpid()
    return [pid for pid, argv, environ in processes()
            if pid != me and any(w in argv or w in environ for w in wanted)]


def hosts(home):
    """The detached session hosts of the Rune home `home`."""
    marker = f"RUNE_HOME={home}".encode()
    return [pid for pid, argv, environ in processes() if b"-detached-host" in argv and marker in environ]


def kill(pid):
    """End `pid` at once (SIGKILL; TerminateProcess on Windows)."""
    try:
        os.kill(pid, signal.SIGTERM if WINDOWS else signal.SIGKILL)
    except OSError:
        pass


def stop(pid):
    """Ask `pid` to end (SIGTERM; TerminateProcess on Windows, which has no asking)."""
    try:
        os.kill(pid, signal.SIGTERM)
    except OSError:
        pass


# Whether a running program can become another build in place (`exec`), as
# Rune's `platform::REPLACES_IN_PLACE` says: not on Windows.
REPLACES_IN_PLACE = not WINDOWS


def running_image(pid):
    """An identity of the file `pid` runs — its inode — that changes when the
    process `exec`s a build installed over the old one."""
    if LINUX:
        return os.stat(f"/proc/{pid}/exe").st_ino
    out = subprocess.run(["lsof", "-a", "-p", str(pid), "-d", "txt", "-Fi"],
                         capture_output=True, text=True).stdout
    return next((int(line[1:]) for line in out.splitlines() if line.startswith("i")), None)
