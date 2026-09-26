#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["pyte>=0.8.2"]
# ///
"""Drive a terminal program in a pty, type a command char by char, and measure
how long each keystroke takes to show up on screen."""
import argparse, fcntl, os, pty, select, signal, struct, subprocess, sys, termios, threading, time
import pyte
from pyte import graphics

# pyte names the 16 palette colours ("brightred") when set with SGR 30-37/90-97
# but uses hex ("ff0000") when the same palette entry is set with 38;5;n.  Both
# select the same terminal colour, so compare them by palette index.
_NAMES = ["black", "red", "green", "brown", "blue", "magenta", "cyan", "white"]
_CANON = {n: graphics.FG_BG_256[i] for i, n in enumerate(_NAMES)}
_CANON.update({"bright" + n: graphics.FG_BG_256[i + 8] for i, n in enumerate(_NAMES)})


def canon(c):
    return _CANON.get(c, c)


def spawn(argv, cols, rows):
    pid, fd = pty.fork()
    if pid == 0:
        fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        os.environ["TERM"] = "xterm-256color"
        os.execvp(argv[0], argv)
    return pid, fd


class Term:
    def __init__(self, cols, rows):
        self.screen = pyte.Screen(cols, rows)
        self.stream = pyte.ByteStream(self.screen)
        self.raw = bytearray()

    def feed(self, b):
        self.raw += b
        self.stream.feed(b)

    def before_cursor(self):
        s = self.screen
        return s.display[s.cursor.y][: s.cursor.x]

    def cursor_row(self):
        return self.screen.display[self.screen.cursor.y].rstrip()

    def frame(self):
        """Cursor row with attributes, plus cursor position: what the user sees."""
        s = self.screen
        line = s.buffer[s.cursor.y]
        cells = tuple((c.data, canon(c.fg) if c.data.strip() else "", canon(c.bg), c.bold, c.underscore, c.reverse)
                      for c in (line[x] for x in range(s.columns)))
        return (s.cursor.x, cells)


def pump(fd, term, quiet, first, until=None, timeline=None):
    """Read until quiet; returns (t_first_output, t_until_true, total_bytes)."""
    t_first = t_until = None
    got = 0
    deadline = time.monotonic() + first
    while True:
        timeout = quiet if got else max(0.0, deadline - time.monotonic())
        r, _, _ = select.select([fd], [], [], timeout)
        if not r:
            return t_first, t_until, got
        try:
            data = os.read(fd, 65536)
        except OSError:
            return t_first, t_until, got
        if not data:
            return t_first, t_until, got
        now = time.monotonic()
        got += len(data)
        t_first = t_first or now
        term.feed(data)
        if until and t_until is None and until():
            t_until = now
        if timeline is not None:
            timeline.append((now, term.frame()))


def rss_sampler(root, stop, peak):
    """Sample resident set size of the process tree under `root` until stop."""
    while not stop.is_set():
        try:
            out = subprocess.run(["ps", "-axo", "pid=,ppid=,rss=,comm="], capture_output=True, text=True).stdout
        except OSError:
            return
        procs = {}
        for line in out.splitlines():
            parts = line.split(None, 3)
            if len(parts) == 4:
                procs[int(parts[0])] = (int(parts[1]), int(parts[2]), parts[3])
        tree, todo = [], [root]
        while todo:
            p = todo.pop()
            if p in procs:
                tree.append(p)
                todo += [c for c, (pp, _, _) in procs.items() if pp == p]
        total = sum(procs[p][1] for p in tree)
        main = max(((procs[p][1], procs[p][2]) for p in tree if not procs[p][2].endswith("ssh")), default=(0, "?"))
        if total > peak["tree"]:
            peak["tree"] = total
        if main[0] > peak["main"]:
            peak["main"], peak["name"] = main
        stop.wait(0.2)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cmd", required=True, help="program to run, e.g. 'ssh obl'")
    ap.add_argument("--type", action="append", help="text to type (repeatable; each is typed after the previous one ran or was cleared)")
    ap.add_argument("--gap", type=float, default=0.25, help="seconds between keys")
    ap.add_argument("--warm", type=float, default=0, help="seconds to wait after the first prompt")
    ap.add_argument("--cols", type=int, default=120)
    ap.add_argument("--rows", type=int, default=40)
    ap.add_argument("--run", action="store_true", help="press enter after each --type and show the output")
    ap.add_argument("--quiet-screen", action="store_true", help="do not dump the screen after running")
    ap.add_argument("--diff", action="store_true", help="show how the frame at echo time differs from the final frame")
    ap.add_argument("--rss", action="store_true", help="report peak memory of the program (largest non-ssh process, and whole tree)")
    args = ap.parse_args()
    if not args.type:
        args.type = ["arelay --help"]

    cols, rows = args.cols, args.rows
    pid, fd = spawn(args.cmd.split(), cols, rows)
    term = Term(cols, rows)
    stop, peak = threading.Event(), {"tree": 0, "main": 0, "name": "?"}
    if args.rss:
        threading.Thread(target=rss_sampler, args=(pid, stop, peak), daemon=True).start()
    pump(fd, term, quiet=0.5, first=20)
    t0 = time.monotonic()
    while time.monotonic() - t0 < args.warm:
        pump(fd, term, quiet=0.3, first=max(0.1, args.warm - (time.monotonic() - t0)))
    print("prompt row: %r" % term.cursor_row())

    for text in args.type:
        text = text.encode("latin-1", "backslashreplace").decode("unicode_escape")
        print("=== typing %r" % text)
        results = []
        typed = ""
        for ch in text:
            typed += ch
            want = typed
            t_send = time.monotonic()
            os.write(fd, ch.encode())
            timeline = []
            tf, tu, got = pump(fd, term, quiet=0.15, first=2.0,
                               until=lambda: term.before_cursor().endswith(want), timeline=timeline)
            echo = (tu - t_send) * 1000 if tu else None
            first = (tf - t_send) * 1000 if tf else None
            final = None
            if timeline:
                last = timeline[-1][1]
                t_final = timeline[-1][0]
                for t, fr in reversed(timeline):
                    if fr != last:
                        break
                    t_final = t
                final = (t_final - t_send) * 1000
            results.append((ch, first, echo, final))
            if args.diff and timeline and tu is not None:
                at_echo = next(fr for t, fr in timeline if t >= tu)
                last = timeline[-1][1]
                if at_echo != last:
                    print("  %r: frame at echo != final frame; cursor %d vs %d" % (ch, at_echo[0], last[0]))
                    for x, (a, b) in enumerate(zip(at_echo[1], last[1])):
                        if a != b:
                            print("    col %3d: echo=%r final=%r" % (x, a, b))
            time.sleep(max(0.0, args.gap - (time.monotonic() - t_send)))
        pump(fd, term, quiet=0.3, first=1.0)
        print("final row : %r" % term.cursor_row())
        ok = term.before_cursor().endswith(text)
        print("typed text present at cursor: %s" % ok)
        print("%-4s %10s %10s %12s" % ("key", "first ms", "echo ms", "final ms"))
        echoes, finals = [], []
        for ch, first, echo, final in results:
            print("%-4r %10s %10s %12s" % (ch, "%.0f" % first if first else "-", "%.0f" % echo if echo else "-",
                                           "%.0f" % final if final is not None else "-"))
            if echo is not None:
                echoes.append(echo)
            if final is not None:
                finals.append(final)
        for name, xs in (("char echo", echoes), ("final frame", finals)):
            if xs:
                xs.sort()
                print("%-12s median %4.0f ms   p90 %4.0f ms   max %4.0f ms   (n=%d)" % (
                    name + ":", xs[len(xs) // 2], xs[int(len(xs) * 0.9)], xs[-1], len(xs)))
        if args.run:
            os.write(fd, b"\r")
            pump(fd, term, quiet=0.5, first=5.0)
            if not args.quiet_screen:
                print("--- screen after enter ---")
                for line in term.screen.display:
                    if line.strip():
                        print(line.rstrip())
        else:
            os.write(fd, b"\x15")
            pump(fd, term, quiet=0.2, first=1.0)
    if args.rss:
        time.sleep(0.5)
        stop.set()
        print("peak rss: %s %.1f MB, process tree %.1f MB" % (peak["name"].split("/")[-1], peak["main"] / 1024, peak["tree"] / 1024))
    os.write(fd, b"exit\r")
    pump(fd, term, quiet=0.5, first=5.0)
    tail = term.raw[-400:].decode("utf-8", "replace")
    if "tns:" in tail:
        print(tail[tail.rfind("tns:"):].strip())
    try:
        os.kill(pid, signal.SIGTERM)
    except OSError:
        pass


if __name__ == "__main__":
    main()
