#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = ["pyte>=0.8.2"]
# ///
"""Typing latency of tns and mosh-client over the same delayed loopback link.

    cargo build --release --locked
    TNS_BIN=target/release/tns python3 tests/bench_latency.py [DELAY_MS ...]

Each client talks to its own real mosh-server running the same bash, through
the UDP proxy of native_regressions.py (DELAY_MS each way). A key's latency
is the time until the cursor line of a terminal emulator fed with the
client's output shows it. Nothing leaves this machine.
"""

import os
from pathlib import Path
import pty
import re
import select
import statistics
import subprocess
import sys
import tempfile
import time

import pyte

sys.path.insert(0, str(Path(__file__).resolve().parent))
import native_regressions as native

TEXT = "grep -rn zylophone"  # nothing a fresh cache or history knows
GAP = .12  # s between keys: brisk typing


class Mosh:
    """mosh-client on a pty, with the interface of native_regressions.Client."""

    def __init__(self, root, delay, predict):
        self.home = root / "home"
        self.home.mkdir()
        (self.home / ".bashrc").write_text("PS1='$ '\n")
        (self.home / "proxy.py").write_text(native.PROXY)
        (self.home / "delay").write_text(str(delay))
        env = dict(os.environ, HOME=str(self.home), LANG=native.utf8_locale(), TERM="xterm-256color",
                   BASH_SILENCE_DEPRECATION_WARNING="1", MOSH_SERVER_NETWORK_TMOUT="30")
        started = subprocess.run(["mosh-server", "new", "-i", "127.0.0.1", "-c", "256", "--", "bash", "--rcfile", str(self.home / ".bashrc"), "-i"],
                                 capture_output=True, text=True, env=env, cwd=self.home, stdin=subprocess.DEVNULL)
        port, key = re.search(r"MOSH CONNECT (\d+) (\S+)", started.stdout).groups()
        (self.home / "server.pid").write_text(re.search(r"pid = (\d+)", started.stderr).group(1))
        proxy = subprocess.Popen([sys.executable, str(self.home / "proxy.py"), port, str(self.home)], stdout=subprocess.PIPE, start_new_session=True)
        port = proxy.stdout.readline().decode().strip()
        self.screen = pyte.Screen(80, 24)
        self.stream = pyte.ByteStream(self.screen)
        self.output = bytearray()
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            native.fcntl.ioctl(0, native.termios.TIOCSWINSZ, native.struct.pack("HHHH", 24, 80, 0, 0))
            os.environ.update(env, MOSH_KEY=key, MOSH_PREDICTION_DISPLAY=predict)
            os.execvp("mosh-client", ["mosh-client", "127.0.0.1", port])

    pump = native.Client.pump
    wait_for = native.Client.wait_for
    text = native.Client.text
    line = native.Client.line
    send = native.Client.send
    close = native.Client.close

    def log(self):
        return ""

    def wait_prompt(self, anchors=0):
        self.wait_for(lambda: self.line() == "$", what="prompt")
        self.pump(.3)


def typing(c, text):
    """Latency of each key of `text`, typed at GAP intervals."""
    sent, seen = [], []
    start = time.monotonic()
    while len(seen) < len(text):
        now = time.monotonic()
        if len(sent) < len(text) and now >= start + len(sent) * GAP:
            os.write(c.fd, text[len(sent)].encode())
            sent.append(time.monotonic())
        c.pump(.002)
        # A key counts once everything typed up to it stands before the
        # cursor. (Typed ahead of a prompt, the tty may also have echoed the
        # first keys in front of it: that is the remote's doing.)
        before_cursor = c.screen.display[c.screen.cursor.y][:c.screen.cursor.x]
        visible = next((k for k in range(len(sent), 0, -1) if before_cursor.endswith(text[:k])), 0)
        seen.extend([time.monotonic()] * (visible - len(seen)))
        if now > start + 20:
            raise AssertionError(f"stuck at {c.line()!r}")
    return [(b - a) * 1000 for a, b in zip(sent, seen)]


def measure(make, delay):
    with tempfile.TemporaryDirectory(prefix="tns-bench-") as tmp:
        c = make(Path(tmp), delay)
        try:
            c.wait_prompt()
            c.pump(1.0)
            # 1. a fresh line; 2. the first key at the next prompt; 3. typing
            # ahead, right after Enter.
            fresh = typing(c, TEXT)
            c.pump(1.0)
            c.send(b"\x15", 1.0)
            c.send(b"true", .5)
            anchors = c.log().count("anchor=")
            c.send(b"\r")
            c.wait_prompt(anchors)
            c.pump(.5)
            first = typing(c, "e")
            c.pump(1.0)
            c.send(b"\x15", 1.0)
            c.send(b"true", .5)
            c.send(b"\r")
            ahead = typing(c, TEXT)
            c.pump(1.0)
            c.send(b"\x15", .5)
            return fresh, first, ahead
        finally:
            c.close()


def main():
    delays = [int(a) for a in sys.argv[1:]] or [10, 30, 150]
    clients = {
        "mosh (adaptive)": lambda root, delay: Mosh(root, delay, "adaptive"),
        "mosh (always)": lambda root, delay: Mosh(root, delay, "always"),
        "tns": lambda root, delay: native.Client(root, delay=delay),
        "tns, previews off": lambda root, delay: native.Client(root, delay=delay, args=["--no-shell-prediction", "--no-tui-prediction"]),
    }
    print(f"typing {TEXT!r} at {GAP * 1000:.0f} ms per key; latencies in ms (median / worst)")
    for delay in delays:
        print(f"\nround trip {2 * delay} ms")
        print(f"  {'':20}{'fresh line':>16}{'first key, new prompt':>24}{'typed right after Enter':>26}")
        for name, make in clients.items():
            fresh, first, ahead = measure(make, delay)
            cell = lambda v: f"{statistics.median(v):.0f} / {max(v):.0f}"
            print(f"  {name:20}{cell(fresh):>16}{cell(first):>24}{cell(ahead):>26}")


if __name__ == "__main__":
    main()
