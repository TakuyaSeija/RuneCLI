"""The terminal and the processes the PTY fixtures drive, the same on every OS.

A fixture talks to a window as a terminal does — bytes in, bytes out, a size —
so what it proves holds wherever the terminal is a real one: a pseudo-terminal
on Linux and macOS, a ConPTY on Windows (what Windows Terminal hosts a program
in), opened here through the console API itself (`ctypes`): its output is read
as bytes, as Windows Terminal reads it — a library that decoded each read on
its own split a character across two reads into two replacement characters,
and every row after it moved. Output is read by a thread on every OS, so a
fixture never blocks on a quiet window.

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
            self._proc = _ConPty([str(a) for a in argv], env, rows, cols, cwd)
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
            data = self._proc.read()
            if not data:
                break
            self._chunks.put(data)

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
            self._proc.write(data)
        else:
            os.write(self.master, data)

    def resize(self, rows, cols):
        self.rows, self.cols = rows, cols
        if WINDOWS:
            self._proc.resize(rows, cols)
        else:
            import fcntl, struct, termios

            fcntl.ioctl(self.master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
            try:
                os.killpg(self.pid, signal.SIGWINCH)
            except OSError:
                pass

    def alive(self):
        return self._proc.alive() if WINDOWS else self._proc.poll() is None

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
            self._proc.kill()
        else:
            self._proc.kill()
            self._proc.wait(5)


class _ConPty:
    """A program in a Windows pseudo console (`CreatePseudoConsole`), read
    and written as bytes."""

    def __init__(self, argv, env, rows, cols, cwd):
        import ctypes
        from ctypes import wintypes as w

        k32 = ctypes.WinDLL("kernel32", use_last_error=True)
        self._k32, self._ctypes, self._w = k32, ctypes, w

        class COORD(ctypes.Structure):
            _fields_ = [("X", w.SHORT), ("Y", w.SHORT)]

        class STARTUPINFOW(ctypes.Structure):
            _fields_ = [("cb", w.DWORD), ("lpReserved", w.LPWSTR), ("lpDesktop", w.LPWSTR),
                        ("lpTitle", w.LPWSTR), ("dwX", w.DWORD), ("dwY", w.DWORD),
                        ("dwXSize", w.DWORD), ("dwYSize", w.DWORD), ("dwXCountChars", w.DWORD),
                        ("dwYCountChars", w.DWORD), ("dwFillAttribute", w.DWORD),
                        ("dwFlags", w.DWORD), ("wShowWindow", w.WORD), ("cbReserved2", w.WORD),
                        ("lpReserved2", ctypes.c_void_p), ("hStdInput", w.HANDLE),
                        ("hStdOutput", w.HANDLE), ("hStdError", w.HANDLE)]

        class STARTUPINFOEXW(ctypes.Structure):
            _fields_ = [("StartupInfo", STARTUPINFOW), ("lpAttributeList", ctypes.c_void_p)]

        class PROCESS_INFORMATION(ctypes.Structure):
            _fields_ = [("hProcess", w.HANDLE), ("hThread", w.HANDLE),
                        ("dwProcessId", w.DWORD), ("dwThreadId", w.DWORD)]

        self._COORD = COORD
        k32.CreatePipe.argtypes = [ctypes.POINTER(w.HANDLE), ctypes.POINTER(w.HANDLE), ctypes.c_void_p, w.DWORD]
        k32.CreatePseudoConsole.argtypes = [COORD, w.HANDLE, w.HANDLE, w.DWORD, ctypes.POINTER(w.HANDLE)]
        k32.CreatePseudoConsole.restype = ctypes.c_long
        k32.ResizePseudoConsole.argtypes = [w.HANDLE, COORD]
        k32.ResizePseudoConsole.restype = ctypes.c_long
        k32.ClosePseudoConsole.argtypes = [w.HANDLE]
        k32.InitializeProcThreadAttributeList.argtypes = [ctypes.c_void_p, w.DWORD, w.DWORD, ctypes.POINTER(ctypes.c_size_t)]
        k32.UpdateProcThreadAttribute.argtypes = [ctypes.c_void_p, w.DWORD, ctypes.c_size_t, ctypes.c_void_p,
                                                  ctypes.c_size_t, ctypes.c_void_p, ctypes.c_void_p]
        k32.CreateProcessW.argtypes = [w.LPCWSTR, w.LPWSTR, ctypes.c_void_p, ctypes.c_void_p, w.BOOL, w.DWORD,
                                       ctypes.c_void_p, w.LPCWSTR, ctypes.c_void_p, ctypes.c_void_p]
        k32.ReadFile.argtypes = [w.HANDLE, ctypes.c_void_p, w.DWORD, ctypes.POINTER(w.DWORD), ctypes.c_void_p]
        k32.WriteFile.argtypes = [w.HANDLE, ctypes.c_char_p, w.DWORD, ctypes.POINTER(w.DWORD), ctypes.c_void_p]
        k32.WaitForSingleObject.argtypes = [w.HANDLE, w.DWORD]
        k32.TerminateProcess.argtypes = [w.HANDLE, w.UINT]
        k32.CloseHandle.argtypes = [w.HANDLE]

        def check(ok, what):
            if not ok:
                raise ctypes.WinError(ctypes.get_last_error(), what)

        # Our ends: we write the console's input, read its output.
        in_read, self._input = w.HANDLE(), w.HANDLE()
        self._output, out_write = w.HANDLE(), w.HANDLE()
        check(k32.CreatePipe(ctypes.byref(in_read), ctypes.byref(self._input), None, 0), "CreatePipe")
        check(k32.CreatePipe(ctypes.byref(self._output), ctypes.byref(out_write), None, 0), "CreatePipe")
        self._console = w.HANDLE()
        result = k32.CreatePseudoConsole(COORD(cols, rows), in_read, out_write, 0, ctypes.byref(self._console))
        if result != 0:
            raise OSError(f"CreatePseudoConsole failed: HRESULT {result & 0xFFFFFFFF:#x}")
        # The console holds its own copies of these ends.
        k32.CloseHandle(in_read)
        k32.CloseHandle(out_write)

        size = ctypes.c_size_t()
        k32.InitializeProcThreadAttributeList(None, 1, 0, ctypes.byref(size))
        self._attributes = ctypes.create_string_buffer(size.value)
        check(k32.InitializeProcThreadAttributeList(self._attributes, 1, 0, ctypes.byref(size)),
              "InitializeProcThreadAttributeList")
        PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE = 0x00020016
        check(k32.UpdateProcThreadAttribute(self._attributes, 0, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
                                            self._console, ctypes.sizeof(w.HANDLE), None, None),
              "UpdateProcThreadAttribute")
        startup = STARTUPINFOEXW()
        startup.StartupInfo.cb = ctypes.sizeof(STARTUPINFOEXW)
        # No standard handles of this process's (CI redirects them): the
        # program's are the pseudo console's.
        startup.StartupInfo.dwFlags = 0x00000100  # STARTF_USESTDHANDLES
        startup.lpAttributeList = ctypes.cast(self._attributes, ctypes.c_void_p)
        block = "".join(f"{k}={v}\0" for k, v in env.items()) + "\0"
        environment = ctypes.create_unicode_buffer(block, len(block) + 1)
        command = ctypes.create_unicode_buffer(subprocess.list2cmdline(argv))
        info = PROCESS_INFORMATION()
        EXTENDED_STARTUPINFO_PRESENT, CREATE_UNICODE_ENVIRONMENT = 0x00080000, 0x00000400
        check(k32.CreateProcessW(None, command, None, None, False,
                                 EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                                 environment, cwd, ctypes.byref(startup), ctypes.byref(info)),
              "CreateProcessW")
        k32.CloseHandle(info.hThread)
        self._process = info.hProcess
        self.pid = info.dwProcessId

    def read(self):
        """The next bytes the console wrote; `b""` once it is closed."""
        ctypes, w = self._ctypes, self._w
        buffer = ctypes.create_string_buffer(65536)
        got = w.DWORD()
        if not self._k32.ReadFile(self._output, buffer, 65536, ctypes.byref(got), None) or not got.value:
            return b""
        return buffer.raw[:got.value]

    def write(self, data):
        ctypes, w = self._ctypes, self._w
        written = w.DWORD()
        self._k32.WriteFile(self._input, data, len(data), ctypes.byref(written), None)

    def resize(self, rows, cols):
        self._k32.ResizePseudoConsole(self._console, self._COORD(cols, rows))

    def alive(self):
        return self._k32.WaitForSingleObject(self._process, 0) == 0x102  # WAIT_TIMEOUT

    def kill(self):
        self._k32.TerminateProcess(self._process, 1)
        self._k32.ClosePseudoConsole(self._console)


def read_document(path):
    """The text of a file Rune replaces whole (a composer, the drafts). On
    Windows a file being replaced cannot be opened for a moment — Python's
    `open` is refused, as an antivirus or an indexer would be — so a refusal
    is read again, briefly; any other error is raised at once."""
    deadline = time.monotonic() + 3
    while True:
        try:
            return path.read_text(encoding="utf-8")
        except PermissionError:
            if time.monotonic() > deadline:
                raise
            time.sleep(0.02)


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
