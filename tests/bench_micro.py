#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["pyte>=0.8.2"]
# ///
"""Python counterpart of `tns --bench`: the same hot paths using tns.py's code."""
import os, sys, time, tempfile
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import tns
from tns import Session, state_key, make_diff, apply_diff, snapshot, clone_screen, Cache, cup, sgr, row_cells
import pyte


class Em:
    """Session.feed without the pty."""
    def __init__(self, cols, rows):
        self.screen = pyte.Screen(cols, rows)
        self.stream = pyte.ByteStream(self.screen)
        self.carry = b""; self.alt = False; self.saved = None; self.cwd = None
        import collections; self.events = collections.deque()
        self.rows = rows
    feed = Session.feed


def main():
    data = open(sys.argv[1], "rb").read()
    cols, rows = 120, 40
    reps = max(1, 2_000_000 // len(data))
    em = Em(cols, rows)
    t = time.perf_counter()
    for _ in range(reps):
        em.feed(data)
    dt = time.perf_counter() - t
    print("emulator: %.1f MB/s (%d bytes x%d, %.3f s)" % (len(data) * reps / dt / 1e6, len(data), reps, dt))

    em = Em(cols, rows)
    em.feed(b"\x1b[1;38;2;137;180;250m~/Developer/tns\x1b[0m \r\n\x1b[1;32m\xe2\x9d\xaf\x1b[0m \x1b[K\r\x1b[112C21:58:22\r\x1b[2C")
    anchor = (em.screen.cursor.y, em.screen.cursor.x)
    pre = snapshot(em.screen, anchor)
    pre_screen = clone_screen(em.screen, anchor[0])
    em.feed(b"\x1b[34mls\x1b[0m\x1b[38;5;8m -la\x1b[0m\x1b[4D")
    d = tempfile.mkdtemp()
    cache = Cache(os.path.join(d, "c.json"))
    cache.put(state_key(pre_screen, anchor), b"l", make_diff(pre, em.screen, anchor))
    for i in range(5000):
        cache.put(state_key(pre_screen, anchor), str(i).encode(), make_diff(pre, em.screen, anchor))
    n = 5000
    painted = 0
    t = time.perf_counter()
    for _ in range(n):
        key = state_key(pre_screen, anchor)
        val = cache.get(key, b"l")
        pred = clone_screen(pre_screen, anchor[0])
        apply_diff(pred, anchor, val)
        out = []
        for y in range(anchor[0], rows):
            a, b = row_cells(pred, y), row_cells(pre_screen, y)
            if a != b:
                for x in range(cols):
                    if a[x] != b[x]:
                        out.append(cup(y, x)); out.append(sgr(a[x][1:])); out.append(a[x][0])
        painted += len("".join(out).encode())
    dt = time.perf_counter() - t
    print("keystroke: %.2f us per key (state key + lookup + apply + paint, %d bytes painted)" % (dt / n * 1e6, painted // n))
    t = time.perf_counter()
    cache.save()
    c2 = Cache(cache.path)
    import json
    print("cache: %d entries save+load %.1f ms, %d KB on disk" % (len(c2), (time.perf_counter() - t) * 1e3, os.path.getsize(cache.path) // 1024))


if __name__ == "__main__":
    main()
