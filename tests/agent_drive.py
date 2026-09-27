#!/usr/bin/env python3
"""Drive `tns agent` in a pty: send a message, wait, print the screen.
usage: agent_drive.py SECONDS "message" -- tns agent ...   (needs pyte: uv run --with pyte)"""
import os, pty, select, struct, fcntl, termios, sys, time
import pyte
secs = float(sys.argv[1]); msg = sys.argv[2]; cmd = sys.argv[sys.argv.index("--") + 1:]
pid, fd = pty.fork()
if pid == 0:
    fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
    os.environ["TERM"] = "xterm-256color"; os.execvp(cmd[0], cmd)
scr = pyte.Screen(100, 30); st = pyte.ByteStream(scr); raw = bytearray()
answered = 0
def pump(t):
    global answered
    end = time.monotonic() + t
    while True:
        left = end - time.monotonic()
        if left <= 0: return
        r, _, _ = select.select([fd], [], [], left)
        if not r: continue
        try: d = os.read(fd, 65536)
        except OSError: return
        if not d: return
        raw.extend(d); st.feed(d)
        if os.environ.get("ANSWER") and any("allow " in l for l in scr.display):
            answered += 1; print("[driver] permission prompt seen, answering", os.environ["ANSWER"], flush=True)
            os.write(fd, os.environ["ANSWER"].encode()); pump(0.5)
pump(3)
try:
    for ch in msg: os.write(fd, ch.encode()); pump(0.02)
    os.write(fd, b"\r")
    if os.environ.get("INTERRUPT_AFTER"):
        pump(float(os.environ["INTERRUPT_AFTER"])); print("[driver] sending Esc", flush=True); os.write(fd, b"\x1b"); pump(secs)
    else:
        pump(secs)
except OSError:
    print("program exited early; raw tail:", repr(bytes(raw[-600:])))
print("\n".join(l.rstrip() for l in scr.display if l.strip()))
os.write(fd, b"\x04"); pump(2)
try: os.kill(pid, 15)
except OSError: pass
