#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = ["pyte>=0.8.2"]
# ///
"""The built-in mosh client against a real mosh-server and a real bash.

Run `cargo build && python3 tests/native_regressions.py`. A fake `ssh` runs
its "remote" commands on this machine, so tns uploads its real hooks, starts
a real mosh-server and talks to it over loopback UDP. A small proxy in
between adds latency and outages. No network or existing user cache is used.
Skipped when mosh-server or bash is not installed.
"""

import fcntl
import os
from pathlib import Path
import pty
import re
import select
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
import unittest

import pyte


BIN = Path(os.environ.get("TNS_BIN", Path(__file__).resolve().parents[1] / "target/debug/tns")).resolve()
DELAY = 150  # ms each way through the proxy
RTT = 2 * DELAY / 1000
INSTANT = .15  # s: half the round trip, with room for a slow CI machine

# ssh HOST COMMAND runs COMMAND here. The mosh-server it starts is put behind
# the delay proxy by rewriting the port in its MOSH CONNECT line.
SSH = r"""#!/usr/bin/env python3
import os, re, subprocess, sys
from pathlib import Path
args = sys.argv[1:]
if args[:1] == ['-G']:
    print('hostname 127.0.0.1')
    raise SystemExit
while args and args[0].startswith('-'):
    del args[:2 if args[0] == '-o' else 1]
command = args[1:]
home = Path(os.environ['HOME'])
if command != ['sh', '-s']:
    follow = re.search(r"tail -n (\+1|0) -F '?([^']+)'?$", ' '.join(command))
    if not follow:
        os.execvp('sh', ['sh', '-c', ' '.join(command)])
    # The event channel: follow the file as `tail -F` would, but as late as
    # the same link would deliver it.
    import time
    delay = float((home / 'delay').read_text()) / 1000 if (home / 'delay').exists() else 0
    path = Path(follow.group(2))
    path.touch()
    with path.open() as events:
        if follow.group(1) == '0':
            events.seek(0, 2)
        due = []
        while True:
            line = events.readline()
            if line.endswith('\n'):
                due.append((time.monotonic() + delay, line))
            elif not due:
                time.sleep(.005)
            while due and (due[0][0] <= time.monotonic() or not line):
                time.sleep(max(0, due[0][0] - time.monotonic()))
                sys.stdout.write(due.pop(0)[1])
                sys.stdout.flush()
script = sys.stdin.read()
done = subprocess.run(['sh', '-s'], input=script, capture_output=True, text=True)
out = done.stdout
match = re.search(r'MOSH CONNECT (\d+) ', out)
if match:
    proxy = subprocess.Popen([sys.executable, str(home / 'proxy.py'), match.group(1), str(home)],
                             stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, start_new_session=True)
    port = proxy.stdout.readline().decode().strip()
    out = out.replace('MOSH CONNECT ' + match.group(1), 'MOSH CONNECT ' + port)
    pid = re.search(r'pid = (\d+)', done.stderr)
    (home / 'server.pid').write_text(pid.group(1) if pid else '')
sys.stdout.write(out)
sys.stderr.write(done.stderr)
raise SystemExit(done.returncode)
"""

# Forwards datagrams both ways, each held for `delay` ms; drops them while
# the `outage` file exists. A `lossy` file holds "PERCENT JITTER_MS": that
# share is dropped and the rest held up to JITTER_MS longer, which reorders.
PROXY = r"""
import heapq, os, random, select, socket, sys, time
from pathlib import Path
target = ('127.0.0.1', int(sys.argv[1]))
home = Path(sys.argv[2])
(home / 'proxy.pid').write_text(str(os.getpid()))
front = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
front.bind(('127.0.0.1', 0))
back = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
print(front.getsockname()[1], flush=True)
delay = float((home / 'delay').read_text()) / 1000 if (home / 'delay').exists() else 0
loss, jitter = map(float, (home / 'lossy').read_text().split()) if (home / 'lossy').exists() else (0, 0)
rng = random.Random(1)
client, queue, seq, idle = None, [], 0, time.monotonic()
while time.monotonic() - idle < 60:
    wait = max(0, queue[0][0] - time.monotonic()) if queue else .2
    for sock in select.select([front, back], [], [], wait)[0]:
        data, addr = sock.recvfrom(65536)
        idle = time.monotonic()
        if sock is front:
            client = addr
        if not (home / 'outage').exists() and rng.random() * 100 >= loss:
            seq += 1
            heapq.heappush(queue, (time.monotonic() + delay + rng.random() * jitter / 1000, seq, sock is front, data))
    while queue and queue[0][0] <= time.monotonic():
        _, _, to_server, data = heapq.heappop(queue)
        if to_server:
            back.sendto(data, target)
        elif client:
            front.sendto(data, client)
"""


# A full-screen program with an input line, redrawn on every key, as
# terminal applications with a composer do.
APP = r"""
import os, tty
tty.setraw(0)
text = ''
def frame():
    line = text or 'Type a question'
    out = '\x1b[2J\x1b[HDemo TUI\x1b[10;1H' + '-' * 40 + '\x1b[11;1H> ' + line + '\x1b[K\x1b[12;1H' + '-' * 40
    os.write(1, (out + '\x1b[?25h\x1b[11;%dH' % (len(text) + 3)).encode())
frame()
while True:
    data = os.read(0, 4096)
    if not data or b'\x04' in data:
        break
    for b in data:
        if b == 127:
            text = text[:-1]
        elif 32 <= b < 127:
            text += chr(b)
    frame()
os.write(1, b'\x1b[2J\x1b[H')
"""


def utf8_locale():
    names = subprocess.run(["locale", "-a"], capture_output=True, text=True).stdout.split()
    for want in ("en_US.UTF-8", "C.UTF-8"):
        for name in names:
            if name.lower().replace("-", "") == want.lower().replace("-", ""):
                return name
    return "en_US.UTF-8"


class Client:
    def __init__(self, root, delay=0, args=(), bashrc="PS1='$ '\n", lossy=None):
        self.home = root / "home"
        self.home.mkdir()
        if lossy:
            (self.home / "lossy").write_text(lossy)
        (self.home / ".bashrc").write_text(bashrc)
        (self.home / "proxy.py").write_text(PROXY)
        (self.home / "app.py").write_text(APP)
        (self.home / "delay").write_text(str(delay))
        mock = root / "bin"
        mock.mkdir()
        script = mock / "ssh"
        script.write_text(SSH.replace("#!/usr/bin/env python3", "#!" + sys.executable, 1))
        script.chmod(0o755)
        self.screen = pyte.Screen(80, 24)
        self.stream = pyte.ByteStream(self.screen)
        self.output = bytearray()
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
            os.environ.update(PATH=str(mock) + ":" + os.environ["PATH"], HOME=str(self.home), TERM="xterm-256color",
                              LANG=utf8_locale(), BASH_SILENCE_DEPRECATION_WARNING="1", MOSH_SERVER_NETWORK_TMOUT="30")
            for name in ("LC_ALL", "LC_CTYPE", "SSH_CONNECTION", "TMUX"):
                os.environ.pop(name, None)
            os.execv(str(BIN), [str(BIN), "--shell", "bash", "--probes", "0", "--history", "0", "--debug", *args, "fake-host"])

    def log(self):
        path = self.home / ".cache/tns/debug.log"
        return path.read_text() if path.exists() else ""

    def pump(self, seconds):
        end = time.monotonic() + seconds
        while True:
            left = end - time.monotonic()
            if left <= 0 or not select.select([self.fd], [], [], left)[0]:
                return
            try:
                data = os.read(self.fd, 65536)
            except OSError:
                return
            if not data:
                return
            self.output.extend(data)
            self.stream.feed(data)

    def wait_for(self, predicate, timeout=10, what="condition"):
        deadline = time.monotonic() + timeout
        while not predicate():
            if time.monotonic() >= deadline:
                raise AssertionError(f"{what} timed out\nscreen: {self.text()!r}\nlog: {self.log()}")
            self.pump(.005)

    def text(self):
        return "\n".join(line.rstrip() for line in self.screen.display).rstrip()

    def line(self):
        return self.screen.display[self.screen.cursor.y].rstrip()

    def send(self, data, delay=0):
        os.write(self.fd, data)
        self.pump(delay)

    def anchors(self):
        return self.log().count("anchor=")

    def wait_prompt(self, anchors=0):
        self.wait_for(lambda: self.anchors() > anchors, what="shell prompt")

    def run(self, command):
        # Enter has to arrive on its own to be recognized as running a command.
        anchors = self.anchors()
        self.send(command, .05)
        self.send(b"\r")
        self.wait_prompt(anchors)

    def appears(self, key, expect):
        # Seconds until typed `key` makes the cursor line read `expect`.
        start = time.monotonic()
        self.send(key)
        self.wait_for(lambda: self.line() == expect, what=f"{expect!r} after {key!r}")
        return time.monotonic() - start

    def finish(self):
        self.send(b"\x04")
        pattern = rb"tns: \d+ keys.*echo (\d+) previewed \((\d+) matched, (\d+) discarded\)"
        self.wait_for(lambda: re.search(pattern, self.output) is not None, what="exit statistics")
        return tuple(map(int, re.search(pattern, self.output).groups()))

    def server_alive(self):
        pid = (self.home / "server.pid").read_text().strip()
        return bool(pid) and subprocess.run(["kill", "-0", pid], capture_output=True).returncode == 0

    def close(self):
        # Keep reading while it exits: restoring the terminal waits for
        # its output to drain.
        waited, _ = os.waitpid(self.pid, os.WNOHANG)
        if not waited:
            os.kill(self.pid, signal.SIGTERM)
            deadline = time.monotonic() + 5
            while not os.waitpid(self.pid, os.WNOHANG)[0]:
                if time.monotonic() > deadline:
                    os.kill(self.pid, signal.SIGKILL)
                    os.waitpid(self.pid, 0)
                    break
                self.pump(.01)
        os.close(self.fd)
        for name in ("server.pid", "proxy.pid"):
            path = self.home / name
            if path.exists() and path.read_text().strip():
                subprocess.run(["kill", path.read_text().strip()], capture_output=True)


@unittest.skipUnless(shutil.which("mosh-server") and shutil.which("bash"), "needs mosh-server and bash")
class Native(unittest.TestCase):
    def client(self, **kwargs):
        tmp = tempfile.TemporaryDirectory(prefix="tns-native-")
        self.addCleanup(tmp.cleanup)
        client = Client(Path(tmp.name), **kwargs)
        self.addCleanup(client.close)
        client.wait_prompt()
        return client

    def test_session_runs_commands_and_ends_with_the_shell(self):
        c = self.client()
        self.assertEqual(c.line(), "$")
        c.run(b"echo $TERM; stty size")
        self.assertTrue(c.text().endswith("$ echo $TERM; stty size\nxterm-256color\n24 80\n$"), c.text())
        self.assertTrue(c.server_alive())
        c.finish()
        c.wait_for(lambda: not c.server_alive(), what="mosh-server exit")
        # The terminal is handed back: own screen left, application keys off.
        self.assertIn(b"\x1b[?1049h\x1b[?1h", c.output)
        self.assertIn(b"\x1b[?1l", c.output)
        self.assertTrue(c.output.rstrip().endswith(b"discarded)"))
        self.assertIn(b"\x1b[?1049l", c.output)

    def earn_trust(self, c):
        # Until the shell has been seen echoing, typing waits for the server;
        # one echo is enough for the rest of the line.
        self.assertGreater(c.appears(b"e", "$ e"), RTT * .8)
        self.assertLess(c.appears(b"c", "$ ec"), INSTANT)
        c.pump(RTT * 2)  # their acknowledgments

    def test_unfamiliar_typing_is_echoed_before_the_server_replies(self):
        c = self.client(delay=DELAY)
        self.earn_trust(c)
        shown = "$ ec"
        for key in "ho hi":
            shown += key
            self.assertLess(c.appears(key.encode(), shown.rstrip()), INSTANT, shown)
        # Backspace and the arrow keys are previewed too.
        self.assertLess(c.appears(b"\x7f", "$ echo h"), INSTANT)
        column = c.screen.cursor.x
        c.send(b"\x1bOD", .03)
        self.assertEqual(c.screen.cursor.x, column - 1)
        c.send(b"\x1bOC", .03)
        self.assertEqual(c.screen.cursor.x, column)
        self.assertLess(c.appears(b"o", "$ echo ho"), INSTANT)
        c.run(b"")
        self.assertTrue(c.text().endswith("$ echo ho\nho\n$"), c.text())
        # A new prompt of a shell that has echoed before: no waiting at all.
        self.assertLess(c.appears(b"x", "$ x"), INSTANT)
        c.pump(RTT * 2)
        c.send(b"\x15", RTT * 2)
        previewed, matched, discarded = c.finish()
        self.assertGreaterEqual(previewed, 8)
        self.assertEqual((matched, discarded), (previewed, 0))

    def test_hidden_input_is_never_previewed(self):
        c = self.client(delay=DELAY)
        self.earn_trust(c)
        c.send(b"\x15", RTT * 2)
        c.send(b"read -s -p Password: secret", RTT * 2)
        c.send(b"\r")
        c.wait_for(lambda: c.line() == "Password:", what="password prompt")
        c.pump(RTT)
        for key in b"hunter2":
            c.send(bytes([key]), .04)
            self.assertEqual(c.line(), "Password:")
        c.pump(RTT * 2)
        self.assertEqual(c.line(), "Password:")
        anchors = c.anchors()
        c.send(b"\r")
        c.wait_prompt(anchors)
        self.assertNotIn("hunter2", c.text())
        # Only the "c" of earning trust was ever drawn locally.
        self.assertEqual(c.finish(), (1, 1, 0))

    def start_app(self, c):
        c.send(f"{sys.executable} app.py".encode(), RTT)
        c.send(b"\r")
        c.wait_for(lambda: c.line() == "> Type a question", what="application")
        c.pump(RTT)

    def test_typing_inside_an_application_is_previewed_once_it_echoes(self):
        c = self.client(delay=DELAY)
        self.start_app(c)
        # No shell prompt here, so the application has to show it echoes:
        # the first key waits, the rest of the line does not.
        self.assertGreater(c.appears(b"h", "> h"), RTT * .8)
        self.assertLess(c.appears(b"e", "> he"), INSTANT)
        self.assertLess(c.appears(b"y", "> hey"), INSTANT)
        self.assertLess(c.appears(b"\x7f", "> he"), INSTANT)
        c.pump(RTT * 2)
        c.send(b"\x04")
        c.wait_for(lambda: c.line() == "$", what="shell prompt")
        previewed, matched, discarded = c.finish()
        self.assertEqual((previewed, matched, discarded), (3, 3, 0))

    def test_application_previews_can_be_disabled(self):
        c = self.client(delay=DELAY, args=["--no-tui-prediction"])
        self.start_app(c)
        for shown in ("> h", "> he", "> hey"):
            self.assertGreater(c.appears(shown[-1].encode(), shown), RTT * .8)
            c.pump(RTT)
        c.send(b"\x04")
        c.wait_for(lambda: c.line() == "$", what="shell prompt")
        self.assertEqual(c.finish(), (0, 0, 0))

    def test_previews_can_be_disabled(self):
        c = self.client(delay=DELAY, args=["--no-shell-prediction"])
        for shown in ("$ a", "$ ab", "$ abc", "$ abcd"):
            self.assertGreater(c.appears(shown[-1].encode(), shown), RTT * .8)
            c.pump(RTT)
        c.send(b"\x15", RTT * 2)
        self.assertEqual(c.finish(), (0, 0, 0))

    def test_learned_keys_are_predicted_from_the_cache(self):
        c = self.client(delay=30)
        for shown in ("$ l", "$ ls"):
            c.appears(shown[-1].encode(), shown)
            c.pump(.6)  # acknowledged, then learned
        anchors = c.anchors()
        c.send(b"\x03")
        c.wait_prompt(anchors)
        c.send(b"l", .05)
        c.send(b"s", .4)
        c.send(b"\x15", .3)
        c.finish()
        stats = re.search(rb"(\d+) predicted \((\d+) confirmed, (\d+) mispredicted\), \d+ unpredicted, learned (\d+) live", c.output)
        predicted, confirmed, mispredicted, learned = map(int, stats.groups())
        self.assertEqual((predicted, confirmed, mispredicted), (2, 2, 0))
        self.assertGreaterEqual(learned, 2)

    def test_escape_key_quits_and_ends_the_remote_session(self):
        c = self.client()
        c.send(b"\x1e", .05)
        self.assertTrue(c.server_alive())
        c.send(b".")
        c.wait_for(lambda: b"tns: 0 keys" in c.output, what="exit")
        c.wait_for(lambda: not c.server_alive(), what="mosh-server exit")
        # Ctrl-^ twice sends one to the remote instead.
        c = self.client()
        c.send(b"cat -v", .05)
        c.send(b"\r", .3)
        c.send(b"\x1e\x1e", .05)
        c.send(b"\x1e^", .05)
        c.send(b"\x1ex")
        c.wait_for(lambda: c.line() == "^^^^^^x", what="literal Ctrl-^")

    def test_terminal_resize_reaches_the_remote_and_redraws(self):
        c = self.client()
        c.run(b"echo before")
        fcntl.ioctl(c.fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
        c.screen.resize(30, 100)
        anchors = c.anchors()
        os.kill(c.pid, signal.SIGWINCH)
        c.wait_prompt(anchors)
        c.run(b"stty size")
        self.assertTrue(c.text().endswith("$ stty size\n30 100\n$"), c.text())
        self.assertIn("before", c.text())

    def test_lost_and_reordered_datagrams_do_not_corrupt_the_screen(self):
        # A quarter of all datagrams lost, the rest shuffled by up to 60 ms.
        c = self.client(delay=10, lossy="25 60")
        expected = "\n$"
        for i in range(6):
            command = f"echo line-{i} {'x' * (i * 7)}".rstrip()
            for key in command:
                c.send(key.encode(), .02)
            c.run(b"")
            expected += f" {command}\n{command[5:]}\n$"
        c.pump(1)
        self.assertEqual(c.text(), expected)
        previewed, matched, discarded = c.finish()
        self.assertEqual(discarded, 0)

    def test_silence_is_announced_and_the_session_survives_it(self):
        c = self.client()
        (c.home / "outage").touch()
        c.send(b"echo back")
        c.wait_for(lambda: "last contact" in c.screen.display[0], timeout=12, what="notice")
        self.assertIn("Ctrl-^ .", c.screen.display[0])
        (c.home / "outage").unlink()
        c.wait_for(lambda: "last contact" not in c.screen.display[0] and c.line() == "$ echo back", what="recovery")
        c.run(b"")
        self.assertTrue(c.text().endswith("$ echo back\nback\n$"), c.text())


if __name__ == "__main__":
    unittest.main()
