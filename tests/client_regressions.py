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
import sys
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
        home = Path(os.environ['HOME'])
        with (home / 'shell-input').open('ab') as received:
            received.write(data)
        while (home / 'hold-shell-echo').exists():
            time.sleep(.005)
        if (home / 'rewrite-shell').exists():
            os.write(1, b'\r\x1b[2K$ rewritten')
            continue
        time.sleep(.10)
        if data == b'\x03':
            delay = Path(os.environ['HOME']) / 'reset-delay'
            if delay.exists():
                time.sleep(float(delay.read_text()))
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
    # The test acknowledges client receipt, not just the event producer's write.
    while not (home / 'event-observed').exists():
        time.sleep(.005)
    # Split inside the prompt's SGR prefix: neither fragment alone is a
    # complete prompt. Splitting '$' and ' ' with a sleep instead assumes the
    # producer wakes within the client's 120 ms quiet window; CI need not do so.
    screen(b'\x1b[0m\x1b[0')  # complete zero-width marker, then partial SGR
    while not (home / 'finish-prompt').exists():
        time.sleep(.005)
    screen(b'm$ ')
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
    def __init__(self, root, scenario=None, wait_anchor=True, ssh=False, extra_args=()):
        self.home = root / "home"
        self.home.mkdir()
        mock = root / "bin"
        mock.mkdir()
        # bin/check may use a venv while PATH's python3 is a different version.
        # Run both fixtures with the same interpreter as the test harness.
        for name, source in (("ssh", SSH), ("mosh", MOSH)):
            script = mock / name
            script.write_text(source.replace("#!/usr/bin/env python3", "#!" + sys.executable, 1))
            script.chmod(0o755)
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
            os.environ.update(PATH=str(mock) + ":" + os.environ["PATH"], HOME=str(self.home), TERM="xterm-256color")
            os.environ["TNS_CLIENT_SCENARIO"] = scenario or ""
            os.execv(str(BIN), [str(BIN), "--ssh" if ssh or not scenario else "--mosh-client", "--probes", "0", "--history", "0", "--debug", *extra_args, "fake-host"])
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

    def reset_prompt(self):
        anchors = self.log().count("anchor=")
        self.send(b"\x03", 0)
        # A remote echo/reset and the client's quiet timer must both finish
        # before typing; a fixed send delay can cancel the pending anchor.
        self.wait_for(lambda: self.log().count("anchor=") > anchors)

    def idle_cpu(self):
        def cpu():
            text = subprocess.check_output(["ps", "-p", str(self.pid), "-o", "time="], text=True).strip()
            parts = text.split(":")
            return sum(float(part) * 60**i for i, part in enumerate(reversed(parts)))
        before = cpu()
        self.pump(.5)
        return cpu() - before

    def finish(self):
        self.send(b"\x04", 0)
        pattern = (
            rb"tns: \d+ keys, (\d+) predicted \((\d+) confirmed, (\d+) mispredicted\), "
            rb"(\d+) unpredicted, learned (\d+) live"
        )
        self.wait_for(lambda: re.search(pattern, self.output) is not None)
        return tuple(map(int, re.search(pattern, self.output).groups()))

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
        self.client.reset_prompt()
        self.client.send(b"a")
        self.assertEqual(self.client.finish(), (0, 0, 0, 4, 2))

    def assert_predicted_burst(self):
        for key in (b"a", b"b", b"c"):
            self.client.send(key)
        self.client.reset_prompt()
        self.client.send(b"ab", .7)
        self.client.send(b"c")
        self.assertEqual(self.client.finish(), (3, 3, 0, 3, 3))

    def test_predicted_burst_keeps_normal_acknowledgments(self):
        self.assert_predicted_burst()

    def test_predicted_burst_waits_for_delayed_reset(self):
        # Exceed the old 300 ms send budget, without changing key echoes/acks.
        (self.client.home / "reset-delay").write_text(".35")
        self.assert_predicted_burst()

    def test_mixed_predicted_prefix_and_uncached_suffix_recovers(self):
        self.client.send(b"a")
        self.client.reset_prompt()
        self.client.send(b"ab", .7)
        self.client.send(b"c")
        predicted, _, _, unpredicted, learned = self.client.finish()
        self.assertEqual((predicted, unpredicted, learned), (1, 3, 2))

    def test_uncached_typing_is_previewed_before_echo_and_forwarded_exactly_once(self):
        for key in (b"a", b"b"):
            self.client.send(key)
        hold = self.client.home / "hold-shell-echo"
        hold.touch()
        self.client.send(b"cd", .03)
        self.client.wait_for(lambda: self.client.log().count("shell literal edit preview") == 2)
        self.client.wait_for(lambda: (self.client.home / "shell-input").read_bytes().startswith(b"abc"))
        # The PTY's authoritative echo is held; these bytes are local paint.
        self.assertIn(b"cd", self.client.output)
        hold.unlink()
        self.client.wait_for(lambda: (self.client.home / "shell-input").read_bytes() == b"abcd")
        self.client.pump(.7)
        self.client.finish()
        self.assertIn(b"shell 2 previewed (2 matched, 0 discarded)", self.client.output)

    def test_uncached_preview_expires_without_echo(self):
        for key in (b"a", b"b"):
            self.client.send(key)
        hold = self.client.home / "hold-shell-echo"
        hold.touch()
        self.client.send(b"c", .03)
        self.client.wait_for(lambda: "shell literal edit preview" in self.client.log())
        self.client.wait_for(lambda: "shell preview expired" in self.client.log())
        self.assertLess(self.client.idle_cpu(), .15)
        hold.unlink()
        self.client.pump(.3)
        self.client.finish()
        self.assertIn(b"shell 1 previewed (0 matched, 1 discarded)", self.client.output)

    def test_rewritten_command_discards_literal_preview(self):
        for key in (b"a", b"b"):
            self.client.send(key)
        (self.client.home / "rewrite-shell").touch()
        self.client.send(b"c", .3)
        self.client.finish()
        self.assertIn(b"shell 1 previewed (0 matched, 1 discarded)", self.client.output)

    def test_literal_shell_preview_can_be_disabled(self):
        tmp = tempfile.TemporaryDirectory(prefix="tns-shell-opt-out-")
        self.addCleanup(tmp.cleanup)
        client = Client(Path(tmp.name), extra_args=["--no-shell-prediction"])
        self.addCleanup(client.close)
        for key in (b"a", b"b", b"c"):
            client.send(key)
        client.finish()
        self.assertNotIn("shell literal edit preview", client.log())
        self.assertIn(b"shell 0 previewed (0 matched, 0 discarded)", client.output)


class PromptRegressions(unittest.TestCase):
    def client(self, scenario, wait_anchor=True, ssh=False):
        tmp = tempfile.TemporaryDirectory(prefix="tns-prompt-regression-")
        self.addCleanup(tmp.cleanup)
        client = Client(Path(tmp.name), scenario, wait_anchor, ssh)
        self.addCleanup(client.close)
        return client

    def assert_prompt(self, client):
        self.assertIn("anchor=(0, 2)", client.log())
        self.assertEqual(client.log().count("anchor="), 1)
        client.send(b"a")
        self.assertNotIn("anchor=(0, 3)", client.log())
        self.assertEqual(client.finish(), (0, 0, 0, 1, 1))

    def test_event_after_prompt_screen(self):
        self.assert_prompt(self.client("screen-first"))

    def test_event_before_split_prompt_screen(self):
        client = self.client("event-first", wait_anchor=False)
        client.wait_for(lambda: "event prompt" in client.log())
        self.assertNotIn("anchor=", client.log())
        (client.home / "event-observed").touch()
        client.wait_for(lambda: b"\x1b[0m" in client.output)
        # Force distinct PTY reads, even if the runner pauses either process.
        # The incomplete prefix must not anchor, even past the quiet window.
        # Exact quiet-window restart arithmetic for two rendered frames is
        # covered by prompt_event_before_screen_waits_for_quiet_output in Rust.
        client.pump(.2)
        self.assertNotIn("anchor=", client.log())
        self.assertNotIn(b"$", client.output)
        (client.home / "finish-prompt").touch()
        client.wait_for(lambda: "anchor=" in client.log())
        self.assert_prompt(client)

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
