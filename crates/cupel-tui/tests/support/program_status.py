"""Exercise terminal-attached TUI/plain status through real PTYs, without Rex/network."""

import errno
import fcntl
import os
from pathlib import Path
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import termios
import time

executable, test_name, root, frontend, scenario = sys.argv[1:]
master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
original = termios.tcgetattr(slave)
child = subprocess.Popen(
    [executable, "--exact", test_name, "--nocapture"],
    stdin=slave, stdout=slave, stderr=slave, cwd=root,
    env={**os.environ, "TERM": "xterm-256color", "CUPEL_TUI_SIGNAL_CHILD": root,
         "CUPEL_STATUS_FRONTEND": frontend, "CUPEL_STATUS_SCENARIO": scenario},
)
output = bytearray()


def reports():
    return [dict(pair.split(b"=", 1) for pair in body.split(b":"))
            for body in re.findall(rb"\x1b\]7501;([^\x1b]*)\x1b\\", output)]


def read_output():
    if select.select([master], [], [], 0.02)[0]:
        try:
            data = os.read(master, 65536)
        except OSError as error:
            if error.errno == errno.EIO:
                return
            raise
        output.extend(data)
        if b"\x1b[6n" in data:
            os.write(master, b"\x1b[1;1R")


def wait_for(condition):
    deadline = time.monotonic() + 15
    while not condition():
        assert time.monotonic() < deadline, "timeout"
        assert child.poll() is None, f"early exit: {child.returncode}"
        read_output()


try:
    wait_for(lambda: reports() and reports()[-1][b"state"] == b"idle")
    os.write(master, b"run\n" if frontend == "plain" else b"run\r")
    wait_for(lambda: any(report[b"state"] == b"working" for report in reports()))
    if scenario == "question":
        wait_for(lambda: reports()[-1][b"state"] == b"blocked")
        assert reports()[-1][b"kind"] == b"question"
        os.write(master, b"\r")
    if scenario == "cancel":
        wait_for(lambda: Path(root, "shell.pid").exists())
        os.kill(child.pid, signal.SIGTERM)
        expected = b"idle"
    else:
        expected = b"error" if scenario == "error" else b"done"
        wait_for(lambda: reports()[-1][b"state"] == expected)
        os.write(master, b"quit\n" if frontend == "plain" else b"\x04")
    deadline = time.monotonic() + 15
    while child.poll() is None:
        assert time.monotonic() < deadline, "did not exit"
        read_output()
    read_output()
    assert reports()[-1][b"state"] == expected, reports()
    assert all(report[b"app"] == b"cupel" for report in reports())
    assert len({report[b"id"] for report in reports()}) == 1
    if scenario == "question":
        states = [report[b"state"] for report in reports()]
        blocked = states.index(b"blocked")
        assert b"working" in states[blocked + 1:-1], states
    expected_exit = 143 if scenario == "cancel" else int(frontend == "plain" and scenario == "error")
    assert child.returncode == expected_exit, child.returncode
    if frontend == "tui":
        assert termios.tcgetattr(slave) == original, "terminal stayed in raw mode"
        assert b"\x1b[?1049l" in output, "alternate screen not restored"
except Exception:
    print(bytes(output).decode(errors="replace"), file=sys.stderr)
    raise
finally:
    if child.poll() is None:
        child.kill()
    child.wait()
    if scenario == "cancel" and Path(root, "shell.pid").exists():
        try:
            os.killpg(int(Path(root, "shell.pid").read_text().strip()), signal.SIGKILL)
        except ProcessLookupError:
            pass
    os.close(master)
    os.close(slave)
