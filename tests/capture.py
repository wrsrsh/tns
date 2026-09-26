#!/usr/bin/env python3
"""Capture the raw bytes a program writes to its pty while we type into it.
usage: capture.py OUT.bin COLSxROWS -- cmd args...   (keys read from $KEYS, \\x escapes ok)"""
import os, pty, select, struct, fcntl, termios, sys, time, signal

out, size = sys.argv[1], sys.argv[2]
argv = sys.argv[sys.argv.index("--") + 1:]
cols, rows = map(int, size.split("x"))
pid, fd = pty.fork()
if pid == 0:
    fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    os.environ["TERM"] = "xterm-256color"
    os.execvp(argv[0], argv)
raw = bytearray()

def on_alarm(*a):
    open(out, "wb").write(raw)
    print("deadline hit; captured %d bytes to %s" % (len(raw), out))
    os._exit(1)
signal.signal(signal.SIGALRM, on_alarm)
signal.alarm(int(os.environ.get("DEADLINE", "120")))

def pump(first, quiet):
    got = 0; deadline = time.monotonic() + first
    while True:
        t = quiet if got else max(0.0, deadline - time.monotonic())
        r, _, _ = select.select([fd], [], [], t)
        if not r: return got
        try: d = os.read(fd, 65536)
        except OSError: return -1
        if not d: return -1
        raw.extend(d); got += len(d)
        if b"\x1b[0c" in d or b"\x1b[c" in d:  # answer DA1 so fish 4 stops waiting
            os.write(fd, b"\x1b[?62;22c")

pump(20, 0.5)
keys = os.environ.get("KEYS", "").encode("latin-1", "backslashreplace").decode("unicode_escape")  # noqa
for k in keys.split("|"):
    for ch in k:
        os.write(fd, ch.encode())
        if pump(2, 0.08) < 0: break
    os.write(fd, b"\r")
    if pump(5, 0.3) < 0: break
os.write(fd, b"exit\r")
pump(3, 0.3)
open(out, "wb").write(raw)
print("captured %d bytes to %s" % (len(raw), out))
