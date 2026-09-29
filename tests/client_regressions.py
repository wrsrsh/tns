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
if sys.argv[-2:] == ['sh', '-s']:
    script = sys.stdin.read()
    if script.strip() == 'echo $SHELL':
        print('/bin/bash')
    elif 'echo SHELL=' in script:
        print('SHELL=/bin/bash\nMOSH=/bin/true\nUTF8=1\nPKG=apt-get\nOS=Linux')
elif any('/run' in arg for arg in sys.argv):
    tty.setraw(0)
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


class Client:
    def __init__(self, root):
        self.home = root / "home"
        self.home.mkdir()
        mock = root / "bin"
        mock.mkdir()
        (mock / "ssh").write_text(SSH)
        (mock / "ssh").chmod(0o755)
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
            os.environ.update(PATH=str(mock) + ":" + os.environ["PATH"], HOME=str(self.home), TERM="xterm-256color")
            os.execv(str(BIN), [str(BIN), "--ssh", "--probes", "0", "--history", "0", "--debug", "fake-host"])
        self.output = bytearray()
        deadline = time.monotonic() + 5
        while "anchor=" not in self.log():
            if time.monotonic() >= deadline:
                self.close()
                raise AssertionError("client did not establish an anchor: " + self.log())
            self.pump(.05)

    def log(self):
        path = self.home / ".cache/tns/debug.log"
        return path.read_text() if path.exists() else ""

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


if __name__ == "__main__":
    unittest.main()
