#!/usr/bin/env python3
"""Real client-loop regressions using local fake SSH/mosh executables.

Run `cargo build && python3 tests/client_regressions.py`. No network, probes,
third-party Python packages, or existing user cache are used.
"""

import fcntl
import os
from pathlib import Path
import pty
import re
import select
import signal
import struct
import subprocess
import tempfile
import termios
import time
import unittest


BIN = Path(os.environ.get("TNS_BIN", Path(__file__).resolve().parents[1] / "target/debug/tns")).resolve()
SSH = r"""#!/usr/bin/env python3
import os, sys, time, tty
from pathlib import Path
if sys.argv[-2:] == ['sh', '-s']:
    script = sys.stdin.read()
    if script.strip() == 'echo $SHELL':
        print('/bin/bash')
    elif 'echo SHELL=' in script:
        print('SHELL=/bin/bash\nMOSH=/bin/true\nUTF8=1\nPKG=apt-get\nOS=Linux')
elif any('tail -n' in arg for arg in sys.argv):
    home = Path(os.environ['HOME'])
    sent = 0
    while True:
        request = home / 'event-request'
        count = int((request.read_text().strip() or '0') if request.exists() else '0')
        while sent < count:
            print('P ', flush=True)
            sent += 1
            (home / 'event-sent').write_text(str(sent))
        time.sleep(.005)
elif any('/run' in arg for arg in sys.argv):
    tty.setraw(0)
    if os.environ.get('TNS_CLIENT_SCENARIO') == 'terminal-replies':
        os.write(1, b'\x1b[0c\x1b[6n')
        replies = bytearray()
        while not replies.endswith(b'R'):
            replies.extend(os.read(0, 4096))
        (Path(os.environ['HOME']) / 'replies-received').write_bytes(replies)
    os.write(1, b'\x1b]133;A\x07$ ')
    while True:
        data = os.read(0, 4096)
        if not data or b'\x04' in data:
            break
        time.sleep(.10)
        if data == b'\x03':
            os.write(1, b'\r\n\x1b]133;A\x07$ ')
        else:
            os.write(1, data)
"""

MOSH = r"""#!/usr/bin/env python3
import os, time, tty
from pathlib import Path
tty.setraw(0)
home = Path(os.environ['HOME'])
mode = os.environ['TNS_CLIENT_SCENARIO']
events = 0
def event():
    global events
    events += 1
    (home / 'event-request').write_text(str(events))
    sent = home / 'event-sent'
    while not sent.exists() or int(sent.read_text().strip() or '0') < events:
        time.sleep(.005)
def screen(data):
    os.write(1, data)
    (home / 'screen-ready').touch()
if mode == 'terminal-replies':
    os.write(1, b'\x1b[0c\x1b[6n')
    replies = bytearray()
    while not replies.endswith(b'R'):
        replies.extend(os.read(0, 4096))
    (home / 'replies-received').write_bytes(replies)
    screen(b'$ ')
    event()
elif mode == 'event-first':
    event()
    time.sleep(.08)
    screen(b'$')
    time.sleep(.06)
    screen(b' ')
elif mode == 'stale-screen':
    screen(b'old command output')
    time.sleep(1.3)
    event()
else:
    screen(b'$ ')
    if mode != 'typing-before-event':
        time.sleep(.5 if mode == 'screen-first' else .05)
        event()
while True:
    data = os.read(0, 4096)
    if not data or b'\x04' in data:
        break
    if data == b'\r':
        event()
        time.sleep(.7)
        screen(b'\r\n$ ')
    else:
        time.sleep(.1)
        os.write(1, data)
        if mode == 'typing-before-event':
            time.sleep(.1)
            event()
"""


class Client:
    def __init__(self, root, scenario=None, wait_anchor=True, ssh=False):
        self.home = root / "home"
        self.home.mkdir()
        mock = root / "bin"
        mock.mkdir()
        (mock / "ssh").write_text(SSH)
        (mock / "ssh").chmod(0o755)
        (mock / "mosh").write_text(MOSH)
        (mock / "mosh").chmod(0o755)
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
            os.environ.update(PATH=str(mock) + ":" + os.environ["PATH"], HOME=str(self.home), TERM="xterm-256color")
            os.environ["TNS_CLIENT_SCENARIO"] = scenario or ""
            os.execv(str(BIN), [str(BIN), *(["--ssh"] if ssh or not scenario else []), "--probes", "0", "--history", "0", "--debug", "fake-host"])
        self.output = bytearray()
        if wait_anchor:
            try:
                self.wait_for(lambda: "anchor=" in self.log())
            except AssertionError:
                self.close()
                raise

    def log(self):
        path = self.home / ".cache/tns/debug.log"
        return path.read_text() if path.exists() else ""

    def wait_for(self, predicate, timeout=5):
        deadline = time.monotonic() + timeout
        while not predicate():
            if time.monotonic() >= deadline:
                raise AssertionError("client condition timed out: " + self.log() + repr(self.output))
            self.pump(.01)

    def pump(self, seconds):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            if not select.select([self.fd], [], [], max(0, end - time.monotonic()))[0]:
                break
            try:
                data = os.read(self.fd, 65536)
            except OSError:
                break
            if not data:
                break
            self.output.extend(data)

    def send(self, data, delay=.3):
        os.write(self.fd, data)
        self.pump(delay)

    def idle_cpu(self):
        def cpu():
            text = subprocess.check_output(["ps", "-p", str(self.pid), "-o", "time="], text=True).strip()
            parts = text.split(":")
            return sum(float(part) * 60**i for i, part in enumerate(reversed(parts)))
        before = cpu()
        self.pump(.5)
        return cpu() - before

    def finish(self):
        self.send(b"\x04", .5)
        match = re.search(
            rb"tns: \d+ keys, (\d+) predicted \((\d+) confirmed, (\d+) mispredicted\), "
            rb"(\d+) unpredicted, learned (\d+) live", self.output
        )
        if not match:
            raise AssertionError(bytes(self.output))
        return tuple(map(int, match.groups()))

    def close(self):
        waited, _ = os.waitpid(self.pid, os.WNOHANG)
        if not waited:
            os.kill(self.pid, signal.SIGTERM)
            os.waitpid(self.pid, 0)
        os.close(self.fd)


class BurstRegressions(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="tns-client-regression-")
        self.addCleanup(self.tmp.cleanup)
        self.client = Client(Path(self.tmp.name))
        self.addCleanup(self.client.close)

    def test_slow_typing_learns_each_key(self):
        for key in (b"a", b"b", b"c"):
            self.client.send(key)
        self.assertEqual(self.client.finish(), (0, 0, 0, 3, 3))

    def test_burst_recovers_without_learning_combined_diff(self):
        self.client.send(b"ab", .7)
        self.assertLess(self.client.idle_cpu(), .15, "client busy-polls after burst output")
        self.client.send(b"c")
        # A fresh prompt must not find an incorrectly learned a -> ab diff.
        self.client.send(b"\x03")
        self.client.send(b"a")
        self.assertEqual(self.client.finish(), (0, 0, 0, 4, 2))

    def test_predicted_burst_keeps_normal_acknowledgments(self):
        for key in (b"a", b"b", b"c"):
            self.client.send(key)
        self.client.send(b"\x03")
        self.client.send(b"ab", .7)
        self.client.send(b"c")
        self.assertEqual(self.client.finish(), (3, 3, 0, 3, 3))

    def test_mixed_predicted_prefix_and_uncached_suffix_recovers(self):
        self.client.send(b"a")
        self.client.send(b"\x03")
        self.client.send(b"ab", .7)
        self.client.send(b"c")
        predicted, _, _, unpredicted, learned = self.client.finish()
        self.assertEqual((predicted, unpredicted, learned), (1, 3, 2))


class PromptRegressions(unittest.TestCase):
    def client(self, scenario, wait_anchor=True, ssh=False):
        tmp = tempfile.TemporaryDirectory(prefix="tns-prompt-regression-")
        self.addCleanup(tmp.cleanup)
        client = Client(Path(tmp.name), scenario, wait_anchor, ssh)
        self.addCleanup(client.close)
        return client

    def assert_prompt(self, scenario):
        client = self.client(scenario)
        self.assertIn("anchor=(0, 2)", client.log())
        self.assertEqual(client.log().count("anchor="), 1)
        client.send(b"a")
        self.assertNotIn("anchor=(0, 3)", client.log())
        self.assertEqual(client.finish(), (0, 0, 0, 1, 1))

    def test_event_after_prompt_screen(self):
        self.assert_prompt("screen-first")

    def test_event_before_split_prompt_screen(self):
        self.assert_prompt("event-first")

    def test_startup_terminal_replies_do_not_count_as_typing(self):
        for ssh in (False, True):
            for fragmented in (False, True):
                with self.subTest(ssh=ssh, fragmented=fragmented):
                    client = self.client("terminal-replies", wait_anchor=False, ssh=ssh)
                    client.wait_for(lambda: b"\x1b[6n" in client.output)
                    replies = b"\x1b[?62;22c\x1b[1;1R"
                    parts = [replies[i:i + 1] for i in range(len(replies))] if fragmented else [replies]
                    for part in parts:
                        client.send(part, .01)
                    client.pump(.6)
                    client.send(b"a")
                    stats = client.finish()
                    self.assertEqual((client.home / "replies-received").read_bytes(), replies)
                    self.assertIn("anchor=(0, 2)", client.log())
                    self.assertEqual(stats, (0, 0, 0, 1, 1))

    def test_typing_before_late_event_never_becomes_anchor(self):
        client = self.client("typing-before-event", wait_anchor=False)
        client.wait_for(lambda: (client.home / "screen-ready").exists())
        client.send(b"a", .7)
        self.assertIn("event prompt", client.log())
        self.assertNotIn("anchor=", client.log())
        self.assertLess(client.idle_cpu(), .15)
        self.assertEqual(client.finish(), (0, 0, 0, 0, 0))

    def test_keyboard_escapes_before_prompt_still_cancel_anchor(self):
        for key in (b"\x1b[A", b"\x1b[1;2R", b"\x1bx"):
            with self.subTest(key=key):
                client = self.client("typing-before-event", wait_anchor=False)
                client.wait_for(lambda: (client.home / "screen-ready").exists())
                client.send(key, .7)
                stats = client.finish()
                self.assertIn("event prompt", client.log())
                self.assertNotIn("anchor=", client.log())
                self.assertEqual(stats, (0, 0, 0, 0, 0))

    def test_typing_during_event_grace_cancels_anchor(self):
        client = self.client("typing-after-event", wait_anchor=False)
        client.wait_for(lambda: "event prompt" in client.log())
        client.send(b"a", .7)
        self.assertNotIn("anchor=", client.log())
        self.assertEqual(client.finish(), (0, 0, 0, 0, 0))

    def test_stale_output_does_not_anchor_or_busy_poll(self):
        client = self.client("stale-screen", wait_anchor=False)
        client.wait_for(lambda: "event prompt" in client.log())
        self.assertLess(client.idle_cpu(), .15)
        self.assertNotIn("anchor=", client.log())
        client.send(b"a", .5)
        self.assertNotIn("anchor=", client.log())
        self.assertEqual(client.finish(), (0, 0, 0, 0, 0))

    def test_next_prompt_does_not_reuse_previous_command_screen(self):
        client = self.client("epoch")
        self.assertEqual(client.log().count("anchor="), 1)
        client.send(b"\r", .45)  # next event arrives now, screen only after .7s
        self.assertEqual(client.log().count("event prompt"), 2)
        self.assertEqual(client.log().count("anchor="), 1)
        client.wait_for(lambda: "anchor=(1, 2)" in client.log())
        client.send(b"a")
        self.assertEqual(client.finish(), (0, 0, 0, 1, 1))


if __name__ == "__main__":
    unittest.main()
