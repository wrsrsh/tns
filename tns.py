#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["pyte>=0.8.2"]
# ///
"""tns: a predictive terminal for a remote fish shell.

The real shell runs on the remote host over ssh.  Locally we keep a model of
the remote screen (pyte) and a cache of "what does the screen look like after
key K is pressed in state S".  Keystrokes are painted from the cache instantly
and reconciled when the real bytes arrive.  The cache is seeded by hidden probe
sessions that type your history into the remote shell, and it keeps learning
from every keystroke you type.
"""
import argparse
import collections
import copy
import fcntl
import json
import os
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import termios
import threading
import time
import tty
import urllib.parse

import pyte
from pyte import graphics
from pyte.screens import Char

# --------------------------------------------------------------------------- remote side

FISH_INIT = (
    'function __tns_cwd --on-variable PWD; printf "\\e]7;file://%s%s\\a" $hostname $PWD; end; '
    'function __tns_prompt --on-event fish_prompt; printf "\\e]133;A\\a"; end; '
    'function __tns_post --on-event fish_postexec; printf "\\e]7770;%s\\a" (string escape --style=url -- $argv[1]); end; '
    '__tns_cwd'
)

OSC_RE = re.compile(rb"\x1b\](\d+);([^\x07\x1b]*)(?:\x07|\x1b\\)")
ALT_RE = re.compile(rb"\x1b\[\?(?:1049|1047|47)[hl]")
RIGHT_GAP = re.compile(r" {4,}")
CACHE_DIR = os.path.expanduser("~/.cache/tns")


def fish_quote(s):
    return "'" + s.replace("\\", "\\\\").replace("'", "\\'") + "'"


def split_tail(data):
    """Hold back an incomplete trailing escape sequence."""
    i = data.rfind(b"\x1b")
    if i == -1:
        return data, b""
    tail = data[i:]
    if len(tail) == 1:
        return data[:i], tail
    c = tail[1:2]
    if c == b"[":
        for j in range(2, len(tail)):
            if 0x40 <= tail[j] <= 0x7E:
                return data, b""
        return data[:i], tail
    if c == b"]":
        if b"\x07" in tail or b"\x1b\\" in tail[2:]:
            return data, b""
        return data[:i], tail
    if c in (b"(", b")", b"#", b"%"):
        return (data, b"") if len(tail) >= 3 else (data[:i], tail)
    return data, b""


class Session:
    """One ssh pty to the remote fish, with a pyte model of its screen."""

    def __init__(self, host, cols, rows, term, cwd=None, extra_ssh=()):
        remote = "exec fish -C " + fish_quote(FISH_INIT)
        if cwd:
            remote = "cd " + fish_quote(cwd) + "; " + remote
        argv = ["ssh", "-t", "-o", "BatchMode=yes", "-o", "ControlMaster=auto",
                "-o", "ControlPath=~/.ssh/tns-%C", "-o", "ControlPersist=120",
                *extra_ssh, host, remote]
        pid, fd = pty.fork()
        if pid == 0:
            fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
            os.environ["TERM"] = term
            os.execvp(argv[0], argv)
        self.pid, self.fd = pid, fd
        self.cols, self.rows = cols, rows
        self.screen = pyte.Screen(cols, rows)
        self.stream = pyte.ByteStream(self.screen)
        self.carry = b""
        self.alt = False
        self.saved = None
        self.cwd = None
        self.events = collections.deque()

    def resize(self, cols, rows):
        self.cols, self.rows = cols, rows
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.screen.resize(rows, cols)

    def feed(self, data):
        """Update the model; return bytes safe to pass to the real terminal."""
        data = self.carry + data
        data, self.carry = split_tail(data)
        if len(self.carry) > 4096:
            data, self.carry = data + self.carry, b""
        out = bytearray()
        pos = 0
        for m in OSC_RE.finditer(data):
            code, arg = m.group(1), m.group(2)
            if code == b"7":
                try:
                    self.cwd = urllib.parse.unquote(arg.decode("utf-8", "replace").split("/", 3)[-1]) if b"//" in arg else None
                    if self.cwd is not None:
                        self.cwd = "/" + self.cwd
                except Exception:
                    pass
            elif code == b"133" and arg.startswith(b"A"):
                self.events.append(("prompt", None))
            elif code == b"7770":
                self.events.append(("exec", urllib.parse.unquote(arg.decode("utf-8", "replace"))))
                out += data[pos:m.start()]
                pos = m.end()
        out += data[pos:]
        out = bytes(out)
        # feed pyte piecewise so we can emulate the alternate screen
        p = 0
        for m in ALT_RE.finditer(out):
            self.stream.feed(out[p:m.start()])
            p = m.end()
            if m.group(0).endswith(b"h"):
                if not self.alt:
                    self.saved = (copy.deepcopy(self.screen.buffer), copy.copy(self.screen.cursor))
                    self.alt = True
            else:
                if self.alt and self.saved:
                    self.screen.buffer, self.screen.cursor = self.saved
                    self.screen.dirty.update(range(self.rows))
                self.alt = False
                self.saved = None
        self.stream.feed(out[p:])
        return out

    def read(self, n=65536):
        return os.read(self.fd, n)

    def write(self, data):
        while data:
            n = os.write(self.fd, data)
            data = data[n:]

    def close(self):
        try:
            os.close(self.fd)
        except OSError:
            pass
        try:
            os.kill(self.pid, signal.SIGHUP)
        except OSError:
            pass


# --------------------------------------------------------------------------- screen model helpers

def cell(c):
    return (c.data, c.fg, c.bg, c.bold, c.italics, c.underscore, c.strikethrough, c.reverse)


def clone_screen(screen, from_row):
    """Copy of a pyte screen sharing the rows above from_row (never modified)."""
    new = pyte.Screen(screen.columns, screen.lines)
    for y in range(screen.lines):
        new.buffer[y] = copy.copy(screen.buffer[y]) if y >= from_row else screen.buffer[y]
    new.cursor = copy.copy(screen.cursor)
    return new


DEFAULT_CELL = cell(Char(" "))


def row_cells(screen, y):
    line = screen.buffer[y]
    return [cell(line[x]) for x in range(screen.columns)]


def row_text(cells):
    return "".join(c[0] if c[0] else "" for c in cells)


def snapshot(screen, anchor):
    ay = anchor[0]
    return {"rows": {y: row_cells(screen, y) for y in range(ay, screen.lines)},
            "cursor": (screen.cursor.y, screen.cursor.x)}


def state_key(screen, anchor):
    ay, ax = anchor
    parts = []
    for y in range(ay, screen.lines):
        t = row_text(row_cells(screen, y))
        if y == ay:
            t = RIGHT_GAP.split(t[ax:], 1)[0]
        parts.append(t.rstrip())
    while parts and parts[-1] == "":
        parts.pop()
    return json.dumps([screen.columns, screen.cursor.y - ay, screen.cursor.x - ax, parts])


def make_diff(pre, screen, anchor):
    ay, ax = anchor
    cells = []
    for y in range(ay, screen.lines):
        prow = pre["rows"].get(y)
        row = row_cells(screen, y)
        for x in range(screen.columns):
            if prow is None or row[x] != prow[x]:
                if x - ax >= -ax:
                    cells.append([y - ay, x - ax, *row[x]])
    return {"cells": cells, "cur": [screen.cursor.y - ay, screen.cursor.x - ax]}


def apply_diff(screen, anchor, d):
    ay, ax = anchor
    for dy, dx, data, fg, bg, bold, it, ul, st, rv in d["cells"]:
        y, x = ay + dy, ax + dx
        if 0 <= y < screen.lines and 0 <= x < screen.columns:
            screen.buffer[y][x] = Char(data=data, fg=fg, bg=bg, bold=bold, italics=it,
                                       underscore=ul, strikethrough=st, reverse=rv)
            screen.dirty.add(y)
    screen.cursor.y = max(0, min(screen.lines - 1, ay + d["cur"][0]))
    screen.cursor.x = max(0, min(screen.columns, ax + d["cur"][1]))


FG_CODES = {v: k for k, v in graphics.FG_ANSI.items()}
FG_CODES.update({v: k for k, v in graphics.FG_AIXTERM.items()})
BG_CODES = {v: k for k, v in graphics.BG_ANSI.items()}
BG_CODES.update({v: k for k, v in graphics.BG_AIXTERM.items()})


def color_codes(val, is_bg):
    if val == "default":
        return []
    table = BG_CODES if is_bg else FG_CODES
    if val in table:
        return [str(table[val])]
    if len(val) == 6:
        try:
            r, g, b = int(val[0:2], 16), int(val[2:4], 16), int(val[4:6], 16)
            return [("48" if is_bg else "38"), "2", str(r), str(g), str(b)]
        except ValueError:
            pass
    return []


def sgr(attr):
    fg, bg, bold, it, ul, st, rv = attr
    codes = ["0"]
    if bold:
        codes.append("1")
    if it:
        codes.append("3")
    if ul:
        codes.append("4")
    if rv:
        codes.append("7")
    if st:
        codes.append("9")
    codes += color_codes(fg, False) + color_codes(bg, True)
    return "\x1b[" + ";".join(codes) + "m"


def render_row(y, cells):
    last = -1
    for x, c in enumerate(cells):
        if c != DEFAULT_CELL:
            last = x
    out = ["\x1b[%d;1H" % (y + 1)]
    cur = None
    for c in cells[: last + 1]:
        if c[1:] != cur:
            cur = c[1:]
            out.append(sgr(cur))
        out.append(c[0])
    out.append("\x1b[0m\x1b[K")
    return "".join(out)


def cup(y, x):
    return "\x1b[%d;%dH" % (y + 1, x + 1)


# --------------------------------------------------------------------------- cache

class Cache:
    def __init__(self, path):
        self.path = path
        self.lock = threading.Lock()
        self.d = {}
        self.dirty = False
        try:
            with open(path) as f:
                self.d = json.load(f)
        except Exception:
            self.d = {}

    @staticmethod
    def k(state, key):
        return state + "\x00" + key.decode("utf-8", "replace")

    def get(self, state, key):
        with self.lock:
            return self.d.get(self.k(state, key))

    def put(self, state, key, val):
        with self.lock:
            self.d[self.k(state, key)] = val
            self.dirty = True

    def has(self, state, key):
        with self.lock:
            return self.k(state, key) in self.d

    def __len__(self):
        return len(self.d)

    def save(self):
        with self.lock:
            if not self.dirty:
                return
            os.makedirs(os.path.dirname(self.path), exist_ok=True)
            tmp = self.path + ".tmp"
            with open(tmp, "w") as f:
                json.dump(self.d, f)
            os.replace(tmp, self.path)
            self.dirty = False


# --------------------------------------------------------------------------- probes (hidden calibration sessions)

def wait_quiet(sess, first=1.0, quiet=0.05):
    """Read until output has been silent for `quiet` seconds. Returns bytes seen."""
    got = 0
    deadline = time.monotonic() + first
    while True:
        timeout = quiet if got else max(0.0, deadline - time.monotonic())
        r, _, _ = select.select([sess.fd], [], [], timeout)
        if not r:
            return got
        try:
            data = sess.read()
        except OSError:
            return -1
        if not data:
            return -1
        got += len(data)
        sess.feed(data)


class Prober(threading.Thread):
    def __init__(self, client, idx, deadline):
        super().__init__(daemon=True)
        self.client = client
        self.idx = idx
        self.deadline = deadline  # after this, only prober 0 keeps running
        self.sess = None
        self.anchor = None
        self.size = None

    def log(self, msg):
        self.client.log("probe%d: %s" % (self.idx, msg))

    def spawn(self):
        if self.sess:
            self.sess.close()
        cols, rows = self.client.size
        self.size = (cols, rows)
        self.sess = Session(self.client.host, cols, rows, self.client.term, cwd=self.client.sess.cwd)
        t0 = time.monotonic()
        while time.monotonic() - t0 < 10:
            if wait_quiet(self.sess, first=2.0, quiet=0.15) < 0:
                break
            if any(e[0] == "prompt" for e in self.sess.events):
                self.sess.events.clear()
                wait_quiet(self.sess, first=0.2, quiet=0.15)
                self.anchor = (self.sess.screen.cursor.y, self.sess.screen.cursor.x)
                self.log("ready anchor=%s in %.2fs" % (self.anchor, time.monotonic() - t0))
                return True
        self.log("failed to get a prompt")
        return False

    def run(self):
        while not self.client.stopping:
            if self.idx > 0 and time.monotonic() > self.deadline:
                break
            cmd = self.client.next_probe(block=True)
            if cmd is None:
                continue
            if self.sess is None or self.size != self.client.size:
                if not self.spawn():
                    time.sleep(1)
                    continue
            try:
                self.probe(cmd)
            except OSError as e:
                self.log("session died: %s" % e)
                self.sess = None
        if self.sess:
            self.sess.close()
        self.log("done")

    def probe(self, cmd):
        sess, cache, anchor = self.sess, self.client.cache, self.anchor
        typed = False
        empty_key = state_key(sess.screen, anchor)
        run = ""
        learned = 0
        chars = list(cmd)
        i = 0
        while i < len(chars):
            key = state_key(sess.screen, anchor)
            ch = chars[i]
            val = cache.get(key, ch.encode())
            if val is not None:
                # fast-forward through known transitions in one burst
                pre = clone_screen(sess.screen, anchor[0])
                j = i
                while j < len(chars):
                    v = cache.get(state_key(pre, anchor), chars[j].encode())
                    if v is None:
                        break
                    apply_diff(pre, anchor, v)
                    j += 1
                sess.write("".join(chars[i:j]).encode())
                typed = True
                wait_quiet(sess, first=1.5, quiet=0.06)
                i = j
                continue
            pre = snapshot(sess.screen, anchor)
            sess.write(ch.encode())
            typed = True
            got = wait_quiet(sess, first=1.5, quiet=0.06)
            if got <= 0:
                self.log("no echo for %r in %r, respawning" % (ch, cmd))
                self.spawn()
                return
            cache.put(key, ch.encode(), make_diff(pre, sess.screen, anchor))
            learned += 1
            i += 1
        if typed:
            sess.write(b"\x15")  # ctrl-u: clear the line
            wait_quiet(sess, first=1.0, quiet=0.06)
            if state_key(sess.screen, anchor) != empty_key:
                self.log("line did not clear after %r, respawning" % cmd)
                self.spawn()
        self.client.stats["probed"] += 1
        self.client.stats["learned_probe"] += learned
        self.log("probed %r (+%d)" % (cmd, learned))


# --------------------------------------------------------------------------- the interactive client

class Client:
    def __init__(self, host, args):
        self.host = host
        self.args = args
        self.term = os.environ.get("TERM", "xterm-256color")
        self.size = self.term_size()
        self.cache = Cache(os.path.join(CACHE_DIR, host + ".json"))
        self.stats = collections.Counter()
        self.stopping = False
        self.probe_queue = collections.deque()
        self.probe_cv = threading.Condition()
        self.probe_seen = set()
        os.makedirs(CACHE_DIR, exist_ok=True)
        self.logf = open(os.path.join(CACHE_DIR, "debug.log"), "a") if args.debug else None
        self.rtt = 0.08
        self.sess = None

    def log(self, msg):
        if self.logf:
            self.logf.write("%.3f %s\n" % (time.monotonic(), msg))
            self.logf.flush()

    @staticmethod
    def term_size():
        try:
            rows, cols = struct.unpack("HHHH", fcntl.ioctl(1, termios.TIOCGWINSZ, b"\0" * 8))[:2]
            return (cols or 80, rows or 24)
        except OSError:
            return (80, 24)

    # ---- probe queue
    def enqueue(self, cmd, front=False):
        cmd = cmd.strip()
        if not cmd or "\n" in cmd or len(cmd) > 120 or any(ord(c) < 32 for c in cmd):
            return
        with self.probe_cv:
            if front:
                self.probe_queue.appendleft(cmd)
            elif cmd not in self.probe_seen:
                self.probe_queue.append(cmd)
            self.probe_seen.add(cmd)
            self.probe_cv.notify()

    def next_probe(self, block):
        with self.probe_cv:
            while not self.probe_queue and not self.stopping:
                if not block:
                    return None
                self.probe_cv.wait(0.5)
                if not self.probe_queue:
                    return None
            return self.probe_queue.popleft() if self.probe_queue else None

    def start_probes(self):
        def coordinator():
            try:
                r = subprocess.run(["ssh", "-o", "BatchMode=yes", "-o", "ControlMaster=auto",
                                    "-o", "ControlPath=~/.ssh/tns-%C", self.host, "fish -c history"],
                                   capture_output=True, text=True, timeout=20)
                lines = r.stdout.splitlines()
            except Exception as e:
                self.log("history fetch failed: %s" % e)
                lines = []
            self.log("history: %d entries" % len(lines))
            for l in lines[: self.args.history]:
                self.enqueue(l)
            deadline = time.monotonic() + self.args.calib_seconds
            for i in range(self.args.probes):
                Prober(self, i, deadline).start()
        threading.Thread(target=coordinator, daemon=True).start()

    # ---- main
    def run(self):
        os.makedirs(CACHE_DIR, exist_ok=True)
        cols, rows = self.size
        self.sess = Session(self.host, cols, rows, self.term)
        sess = self.sess
        old = termios.tcgetattr(0)
        tty.setraw(0)
        resized = [False]
        signal.signal(signal.SIGWINCH, lambda *a: resized.__setitem__(0, True))
        out_fd = 1
        os.write(out_fd, b"\x1b[H\x1b[2J")

        inflight = collections.deque()
        pred = None            # predicted screen (pyte) or None if no overlay
        overlay_cells = {}
        anchor = None
        anchor_valid = False
        pending_anchor = True
        last_out = 0.0
        probes_started = False
        t_start = time.monotonic()

        def key_of(screen):
            return state_key(screen, anchor)

        def runs(cells_by_row):
            """Emit only the given cells, grouped into horizontal runs."""
            out = []
            for y in sorted(cells_by_row):
                xs = sorted(cells_by_row[y])
                i = 0
                while i < len(xs):
                    j = i
                    while j + 1 < len(xs) and xs[j + 1] == xs[j] + 1:
                        j += 1
                    out.append(cup(y, xs[i]))
                    cur = None
                    for x in xs[i:j + 1]:
                        c = cells_by_row[y][x]
                        if c[1:] != cur:
                            cur = c[1:]
                            out.append(sgr(cur))
                        out.append(c[0] if c[0] else " ")
                    i = j + 1
            out.append("\x1b[0m")
            return out

        def paint_overlay():
            """Return bytes that draw pred over confirmed; remember touched cells."""
            nonlocal overlay_cells
            if pred is None:
                overlay_cells = {}
                return b""
            touched = {}
            for y in range(anchor[0], sess.screen.lines):
                a, b = row_cells(pred, y), row_cells(sess.screen, y)
                if a != b:
                    touched[y] = {x: a[x] for x in range(len(a)) if a[x] != b[x]}
            out = runs(touched)
            out.append(cup(pred.cursor.y, pred.cursor.x))
            overlay_cells = {y: set(d) for y, d in touched.items()}
            return "".join(out).encode()

        def restore_overlay():
            nonlocal overlay_cells
            if not overlay_cells:
                return b""
            cells = {}
            for y, xs in overlay_cells.items():
                row = row_cells(sess.screen, y)
                cells[y] = {x: row[x] for x in xs if x < len(row)}
            out = runs(cells)
            out.append(cup(sess.screen.cursor.y, sess.screen.cursor.x))
            overlay_cells = {}
            return "".join(out).encode()

        def rebuild_pred():
            nonlocal pred
            pred = None
            for e in inflight:
                if e["val"] is None:
                    break
                if pred is None:
                    pred = clone_screen(sess.screen, anchor[0])
                apply_diff(pred, anchor, e["val"])

        def learn(e):
            d = make_diff(e["snap"], sess.screen, anchor)
            self.cache.put(e["pre_key"], e["unit"], d)
            self.stats["learned_live"] += 1

        try:
            while True:
                now = time.monotonic()
                timeout = 0.5
                if inflight or pending_anchor:
                    timeout = min(timeout, max(0.0, last_out + 0.04 - now))
                r, _, _ = select.select([0, sess.fd], [], [], timeout)
                now = time.monotonic()

                if resized[0]:
                    resized[0] = False
                    self.size = self.term_size()
                    sess.resize(*self.size)
                    inflight.clear()
                    pred = None
                    overlay_cells = {}
                    anchor_valid = False
                    pending_anchor = True

                if sess.fd in r:
                    try:
                        data = sess.read()
                    except OSError:
                        data = b""
                    if not data:
                        break
                    out = restore_overlay()
                    out += sess.feed(data)
                    last_out = now
                    if inflight and len(inflight) == 1 and not inflight[0].get("seen"):
                        inflight[0]["seen"] = True
                        self.rtt = 0.8 * self.rtt + 0.2 * (now - inflight[0]["t"])
                    while sess.events:
                        ev, arg = sess.events.popleft()
                        if ev == "prompt":
                            pending_anchor = True
                            anchor_valid = False
                            inflight.clear()
                            pred = None
                            if not probes_started:
                                probes_started = True
                                self.start_probes()
                        elif ev == "exec":
                            self.enqueue(arg, front=True)
                    if anchor_valid and inflight and not sess.alt:
                        k = key_of(sess.screen)
                        for i, e in enumerate(inflight):
                            if e["expected"] is not None and e["expected"] == k:
                                for _ in range(i + 1):
                                    inflight.popleft()
                                self.stats["hit"] += i + 1
                                break
                        rebuild_pred()
                    out += paint_overlay()
                    os.write(out_fd, out)

                if 0 in r:
                    data = os.read(0, 4096)
                    if not data:
                        break
                    units = [data]
                    if len(data) <= 6:
                        try:
                            s = data.decode("utf-8")
                            if all(ord(c) >= 32 and c != "\x7f" for c in s):
                                units = [c.encode() for c in s]
                        except UnicodeDecodeError:
                            pass
                    for unit in units:
                        sess.write(unit)
                        self.stats["keys"] += 1
                        if unit in (b"\r", b"\n", b"\x03", b"\x04", b"\x0c"):
                            out = restore_overlay()
                            inflight.clear()
                            pred = None
                            anchor_valid = False
                            os.write(out_fd, out)
                            continue
                        if not anchor_valid or sess.alt:
                            continue
                        base = pred if pred is not None else sess.screen
                        pre_key = key_of(base)
                        learnable = not inflight
                        e = {"unit": unit, "pre_key": pre_key, "t": now, "val": None, "expected": None,
                             "snap": snapshot(sess.screen, anchor) if learnable else None}
                        chain_ok = all(x["val"] is not None for x in inflight)
                        val = self.cache.get(pre_key, unit) if chain_ok else None
                        if val is not None:
                            if pred is None:
                                pred = clone_screen(sess.screen, anchor[0])
                            apply_diff(pred, anchor, val)
                            e["val"] = val
                            e["expected"] = key_of(pred)
                            self.stats["predicted"] += 1
                            os.write(out_fd, paint_overlay())
                        else:
                            self.stats["unpredicted"] += 1
                        inflight.append(e)

                # ---- timers: quiescence
                now = time.monotonic()
                quiet = now - last_out >= 0.04
                if pending_anchor and quiet and last_out > 0 and not sess.alt:
                    pending_anchor = False
                    anchor = (sess.screen.cursor.y, sess.screen.cursor.x)
                    anchor_valid = True
                    self.log("anchor=%s cwd=%s" % (anchor, sess.cwd))
                if inflight and quiet and anchor_valid:
                    e = inflight[0]
                    if e["expected"] is None:
                        if len(inflight) == 1 and e["snap"] is not None and e.get("seen"):
                            learn(e)
                            inflight.clear()
                            rebuild_pred()
                    elif now - e["t"] > max(0.35, 3 * self.rtt) and e.get("seen"):
                        # predicted, but the confirmed screen never matched: misprediction
                        self.stats["miss"] += 1
                        self.log("miss unit=%r" % e["unit"])
                        if len(inflight) == 1 and e["snap"] is not None:
                            learn(e)
                        inflight.clear()
                        pred = None
                        os.write(out_fd, restore_overlay())
                if now - t_start > 5 and int(now) % 15 == 0:
                    self.cache.save()
        finally:
            self.stopping = True
            with self.probe_cv:
                self.probe_cv.notify_all()
            termios.tcsetattr(0, termios.TCSADRAIN, old)
            os.write(out_fd, b"\x1b[0m\r\n")
            self.cache.save()
            sess.close()
            s = self.stats
            print("tns: %d keys, %d predicted (%d confirmed, %d mispredicted), %d unpredicted, "
                  "learned %d live + %d from %d probes, cache %d entries, rtt ~%.0f ms" % (
                      s["keys"], s["predicted"], s["hit"], s["miss"], s["unpredicted"],
                      s["learned_live"], s["learned_probe"], s["probed"], len(self.cache), self.rtt * 1000))


def main():
    ap = argparse.ArgumentParser(description="predictive terminal for a remote fish shell")
    ap.add_argument("host")
    ap.add_argument("--probes", type=int, default=6, help="hidden calibration sessions")
    ap.add_argument("--calib-seconds", type=float, default=12, help="burst calibration time; afterwards one probe keeps learning")
    ap.add_argument("--history", type=int, default=400, help="how many recent history entries to learn")
    ap.add_argument("--debug", action="store_true", help="log to ~/.cache/tns/debug.log")
    args = ap.parse_args()
    Client(args.host, args).run()


if __name__ == "__main__":
    main()
