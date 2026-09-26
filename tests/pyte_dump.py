#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["pyte>=0.8.2"]
# ///
"""Reference dumper: feed stdin through pyte the way tns.py does and print the
screen in the same format as `tns --dump-screen`."""
import re, sys, copy
import pyte
from pyte import graphics

OSC_RE = re.compile(rb"\x1b\](\d+);([^\x07\x1b]*)(?:\x07|\x1b\\)")
ALT_RE = re.compile(rb"\x1b\[\?(?:1049|1047|47)[hl]")

NAMES = ["black", "red", "green", "brown", "blue", "magenta", "cyan", "white"]
IDX = {n: i for i, n in enumerate(NAMES)}
IDX.update({"bright" + n: i + 8 for i, n in enumerate(NAMES)})
HEX256 = {h: i for i, h in reversed(list(enumerate(graphics.FG_BG_256)))}


def canon(v):
    if v == "default":
        return "default"
    if v in IDX:
        return "i%d" % IDX[v]
    if v in HEX256:
        return "i%d" % HEX256[v]
    return v


def dump(cols, rows, data):
    screen = pyte.Screen(cols, rows)
    stream = pyte.ByteStream(screen)
    out = bytearray(); pos = 0
    for m in OSC_RE.finditer(data):
        if m.group(1) == b"7770":
            out += data[pos:m.start()]; pos = m.end()
    out += data[pos:]
    out = bytes(out)
    # pyte treats "CSI > 4;1 m" (xterm modifyOtherKeys) as SGR 4;1 -- a pyte
    # bug the Rust emulator does not share; drop those so the diff is useful.
    out = re.sub(rb"\x1b\[>[0-9;]*m", b"", out)
    p = 0; alt = False; saved = None
    for m in ALT_RE.finditer(out):
        stream.feed(out[p:m.start()]); p = m.end()
        if m.group(0).endswith(b"h"):
            if not alt:
                saved = (copy.deepcopy(screen.buffer), copy.copy(screen.cursor)); alt = True
        else:
            if alt and saved:
                screen.buffer, screen.cursor = saved
            alt = False; saved = None
    stream.feed(out[p:])
    if alt and saved:  # tns.py restores this on exit; that is the semantic state
        screen.buffer, screen.cursor = saved
    o = ["cursor %d %d\n" % (screen.cursor.y, screen.cursor.x)]
    for y in range(rows):
        line = screen.buffer[y]
        for x in range(cols):
            c = line[x]
            flags = (1 if c.bold else 0) | (4 if c.italics else 0) | (8 if c.underscore else 0) | (32 if c.reverse else 0) | (64 if c.strikethrough else 0)
            o.append("%s\t%s\t%s\t%d\n" % (c.data, canon(c.fg), canon(c.bg), flags))
    return "".join(o)


def main():
    cols, rows = map(int, sys.argv[1].split("x"))
    if len(sys.argv) > 2:
        # batch mode: pyte_dump.py COLSxROWS CAPTURE STEP OUTDIR
        data = open(sys.argv[2], "rb").read(); step = int(sys.argv[3]); outdir = sys.argv[4]
        cuts = list(range(step, len(data), step)) + [len(data)]
        for cut in cuts:
            open("%s/pyte-%d.txt" % (outdir, cut), "w").write(dump(cols, rows, data[:cut]))
        print(" ".join(map(str, cuts)))
    else:
        sys.stdout.write(dump(cols, rows, sys.stdin.buffer.read()))


if __name__ == "__main__":
    main()
