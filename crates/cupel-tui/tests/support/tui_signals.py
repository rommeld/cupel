"""Drive the Rust fixture through a PTY and check real terminal restoration."""

import errno
import fcntl
import os
from pathlib import Path
import pty
import select
import signal
import struct
import subprocess
import sys
import termios
import time

executable, test_name, root, signal_name, mode = sys.argv[1:]
root = Path(root)
master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
original_termios = termios.tcgetattr(slave)


def attach_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


child = subprocess.Popen(
    [executable, "--exact", test_name, "--nocapture"],
    stdin=slave,
    stdout=slave,
    stderr=slave,
    cwd=root,
    env={**os.environ, "CUPEL_TUI_SIGNAL_CHILD": str(root), "TERM": "xterm-256color"},
    preexec_fn=attach_terminal if signal_name == "close" else None,
)
output = bytearray()
tool_pid = None


def read_output():
    if master is None:
        time.sleep(0.02)
        return
    if not select.select([master], [], [], 0.02)[0]:
        return
    try:
        data = os.read(master, 65536)
    except OSError as error:
        if error.errno == errno.EIO:
            return
        raise
    output.extend(data)
    # Crossterm may query the cursor during terminal initialization.
    if b"\x1b[6n" in data:
        os.write(master, b"\x1b[1;1R")


def wait_for(condition, description):
    deadline = time.monotonic() + 15
    while not condition():
        assert time.monotonic() < deadline, f"timeout waiting for {description}"
        assert child.poll() is None, f"child exited before {description}: {child.returncode}"
        read_output()


try:
    wait_for(lambda: b"\x1b[?2004h" in output, "terminal initialization")
    assert not termios.tcgetattr(slave)[3] & (termios.ECHO | termios.ICANON)
    if mode == "running":
        os.write(master, b"run\r")
        pid_file = root / "shell.pid"
        wait_for(lambda: pid_file.exists() and pid_file.stat().st_size > 0, "bash tool")
        tool_pid = int(pid_file.read_text().strip())
        os.kill(tool_pid, 0)

    if signal_name == "close":
        # Closing the controlling terminal delivers a real hangup and makes
        # further terminal I/O fail; cleanup must still reach session-end.
        os.close(master)
        master = None
    else:
        os.kill(child.pid, getattr(signal, signal_name))
    deadline = time.monotonic() + 15
    while child.poll() is None:
        assert time.monotonic() < deadline, "cupel did not exit on signal"
        read_output()
    read_output()
    if signal_name == "close":
        assert child.returncode in (0, 1), f"cupel exited with {child.returncode}"
    else:
        assert child.returncode == 0, f"cupel exited with {child.returncode}"
        assert termios.tcgetattr(slave) == original_termios, "terminal stayed in raw mode"
        for sequence in [b"\x1b[?1049l", b"\x1b[?1000l", b"\x1b[?1006l", b"\x1b[?2004l", b"\x1b[<1u"]:
            assert sequence in output, f"terminal restoration missing {sequence!r}"
    if tool_pid is not None:
        try:
            os.killpg(tool_pid, 0)
        except ProcessLookupError:
            tool_pid = None
        else:
            raise AssertionError("bash process group survived shutdown")
except Exception:
    print(bytes(output).decode(errors="replace"), file=sys.stderr)
    raise
finally:
    if tool_pid is not None:
        try:
            os.killpg(tool_pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    if child.poll() is None:
        child.kill()
    child.wait()
    if master is not None:
        os.close(master)
    os.close(slave)
