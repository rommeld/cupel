"""Opt-in, billable terminal smoke check: CUPEL_LIVE_AGENTS=1 python3 this.py binary."""

import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import struct
import subprocess
import sys
import tempfile
import termios
import time


def main():
    if os.environ.get("CUPEL_LIVE_AGENTS") != "1":
        raise SystemExit("Set CUPEL_LIVE_AGENTS=1 to allow billable Fireworks calls")
    binary = str(Path(sys.argv[1]).resolve())
    source = Path(os.environ.get("CUPEL_HOME", str(Path.home() / ".cupel")))
    key = json.loads((source / "settings.json").read_text())["providers"]["fireworks"]
    with tempfile.TemporaryDirectory(prefix="cupel-m8-pty-") as directory:
        root = Path(directory).resolve()
        home, project = root / "home", root / "project"
        home.mkdir(mode=0o700)
        project.mkdir()
        settings = {
            "providers": {"fireworks": key},
            "checkForUpdates": False,
            "agents": {
                "terminal-probe": {
                    "provider": "fireworks",
                    "model": "accounts/fireworks/models/minimax-m3p1-flash",
                    "thinkingLevel": "off",
                    "description": "Replies to a terminal smoke test.",
                    "tools": [],
                    "maxTurns": 1,
                }
            },
        }
        path = home / "settings.json"
        path.touch(mode=0o600)
        path.write_text(json.dumps(settings))
        # Explicitly restrict this disposable project; no trust popup is needed.
        (home / "project-trust.json").write_text(json.dumps({str(project): "restricted"}))
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 140, 0, 0))
        original = termios.tcgetattr(slave)
        child = subprocess.Popen(
            [binary, "--model", "accounts/fireworks/models/minimax-m3p1-flash", "--thinking", "off"],
            cwd=project,
            stdin=slave,
            stdout=slave,
            stderr=slave,
            env={**os.environ, "CUPEL_HOME": str(home), "TERM": "xterm-256color"},
        )
        output = bytearray()

        def read():
            if select.select([master], [], [], 0.05)[0]:
                try:
                    data = os.read(master, 65536)
                except OSError as error:
                    if error.errno != errno.EIO:
                        raise
                    return
                output.extend(data)
                if b"\x1b[6n" in data:
                    os.write(master, b"\x1b[1;1R")

        def wait(condition, description):
            deadline = time.monotonic() + 120
            while not condition():
                assert time.monotonic() < deadline, f"timeout: {description}"
                assert child.poll() is None, f"cupel exited during {description}"
                read()

        def send(text):
            os.write(master, b"\x1b[200~" + text.encode() + b"\x1b[201~")
            # Paste refreshes autocomplete. Close it before Enter submits.
            time.sleep(0.1)
            os.write(master, b"\x1b")
            time.sleep(0.1)
            os.write(master, b"\r")

        def exchanges():
            for path in (home / "sessions").glob("*/*.jsonl"):
                for line in path.read_text().splitlines():
                    if "[agent exchange] terminal-probe" in line and "TERMINAL_M8_OK" in line:
                        return True
            return False

        try:
            wait(lambda: b"\x1b[?2004h" in output, "terminal initialization")
            # Let the first frame finish before sending input.
            for _ in range(10):
                read()
            assert not termios.tcgetattr(slave)[3] & termios.ICANON
            send("/agent terminal-probe")
            wait(lambda: b"esc cancels" in output, "armed input title")
            os.write(master, b"\x1b")
            wait(lambda: b"no longer gets your next prompt" in output, "disarming notice")
            send("/agent terminal-probe Reply with only TERMINAL_M8_OK.")
            wait(exchanges, "persisted real-provider helper answer")
            send("/quit")
            deadline = time.monotonic() + 15
            while child.poll() is None:
                assert time.monotonic() < deadline, "cupel did not quit"
                read()
            read()
            assert child.returncode == 0
            assert termios.tcgetattr(slave) == original, "terminal was not restored"
            assert b"\x1b[?1049l" in output, "alternate screen was not released"
            print("PASS real terminal: arming title, Esc, live helper, persistence, quit and restoration")
        finally:
            if child.poll() is None:
                child.kill()
            child.wait()
            os.close(master)
            os.close(slave)


if __name__ == "__main__":
    main()
