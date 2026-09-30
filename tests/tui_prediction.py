#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = ["pyte>=0.8.2"]
# ///
"""Predict the original Claude-shaped TUI through a normal tns session.

TNS_BIN=target/release/tns uv run --script tests/tui_prediction.py
Fake SSH/mosh only: no Claude API, remote commands, or permission approvals.
"""

import os
from pathlib import Path
import tempfile
import time
import unittest

import pyte
import client_regressions as base


# Keep the existing mock's shell-event channel, then launch a terminal app by
# typing `claude` at its shell prompt. This is not the `tns agent` code path.
base.MOSH = base.MOSH.split("if mode == 'terminal-replies':")[0] + r"""
screen(b'$ ')
event()
command = bytearray()
while b'\r' not in command:
    data = os.read(0, 4096)
    if not data or b'\x04' in data: raise SystemExit
    command.extend(data)
assert bytes(command) == b'claude\r', command

text = ''
cursor = 0
menu = False
def frame():
    line = text if text else 'Try "a question"'
    output = '\x1b[2J\x1b[HClaude Code vTEST\r\nOriginal terminal app; not an agent adapter.'
    output += '\x1b[10;1H' + '─'*80
    output += '\x1b[11;1H❯\u00a0' + line + '\x1b[K'
    output += '\x1b[12;1H' + '─'*80
    output += '\x1b[?25h\x1b[11;' + str(cursor+3) + 'H'
    os.write(1, output.encode())
frame()
while True:
    data = os.read(0, 4096)
    if not data or b'\x04' in data: break
    with (home/'tui-input').open('ab') as f: f.write(data)
    if data == b'\x1b':
        menu = True
        os.write(1, b'\x1b[2J\x1b[HPermission menu: y or n\x1b[?25l')
        continue
    if menu: continue
    if data == b'\r':
        text = ''
        cursor = 0
    else:
        for b in data:
            if b == 127 and cursor:
                text = text[:cursor-1] + text[cursor:]
                cursor -= 1
            elif 32 <= b < 127:
                text = text[:cursor] + chr(b) + text[cursor:]
                cursor += 1
    while (home/'hold').exists(): time.sleep(.005)
    time.sleep(.12)
    if (home/'rewrite').exists():
        text = 'MASKED'
        cursor = len(text)
    frame()
    (home/'ack').write_text(text)
"""


class Terminal:
    def __init__(self, client):
        self.client = client
        self.screen = pyte.Screen(80, 24)
        self.stream = pyte.ByteStream(self.screen)
        self.consumed = 0

    def refresh(self):
        data = self.client.output[self.consumed:]
        self.consumed += len(data)
        self.stream.feed(bytes(data))

    def text(self):
        self.refresh()
        return self.screen.display[10][2:].rstrip()

    def wait(self, predicate, timeout=3):
        deadline = time.monotonic() + timeout
        while True:
            self.client.pump(.005)
            self.refresh()
            if predicate():
                return
            if time.monotonic() >= deadline:
                raise AssertionError(self.screen.display)


class OriginalTuiPrediction(unittest.TestCase):
    def start(self, disabled=False):
        tmp = tempfile.TemporaryDirectory(prefix="tns-original-tui-")
        self.addCleanup(tmp.cleanup)
        c = base.Client(Path(tmp.name), "claude-original",
                        extra_args=["--no-tui-prediction"] if disabled else [])
        self.addCleanup(c.close)
        term = Terminal(c)
        c.send(b"claude\r", .1)
        term.wait(lambda: term.text().startswith("Try "))
        return c, term

    def warm(self, c, term):
        for char, expected in [(b"a", "a"), (b"b", "ab"), (b"c", "abc")]:
            c.send(char, .01)
            term.wait(lambda: term.text() == expected)
        self.assertNotIn("TUI literal edit preview", c.log())

    def test_typing_is_shown_before_remote_echo_without_changing_input(self):
        c, t = self.start()
        self.warm(c, t)
        hold = c.home / "hold"
        hold.touch()
        c.send(b"d", .03)
        t.wait(lambda: t.text() == "abcd")
        self.assertEqual((c.home / "ack").read_text(), "abc")
        self.assertEqual((c.home / "tui-input").read_bytes(), b"abcd")
        hold.unlink()
        t.wait(lambda: (c.home / "ack").read_text() == "abcd")
        c.finish()
        self.assertIn(b"TUI 1 previewed (1 matched, 0 discarded)", c.output)

    def test_cold_field_and_explicit_opt_out_do_not_guess(self):
        for disabled in [False, True]:
            with self.subTest(disabled=disabled):
                c, t = self.start(disabled)
                self.warm(c, t) if disabled else None
                before = t.text()
                hold = c.home / "hold"
                hold.touch()
                c.send(b"d", .05)
                self.assertEqual(t.text(), before)
                hold.unlink()
                c.pump(.2)

    def test_missing_echo_expires_and_restores_original_screen(self):
        c, t = self.start()
        self.warm(c, t)
        hold = c.home / "hold"
        hold.touch()
        c.send(b"d", .02)
        t.wait(lambda: t.text() == "abcd")
        t.wait(lambda: t.text() == "abc", timeout=2)
        self.assertIn("TUI preview expired", c.log())
        hold.unlink()
        t.wait(lambda: t.text() == "abcd")
        c.finish()
        self.assertIn(b"TUI 1 previewed (0 matched, 1 discarded)", c.output)

    def test_rewrite_invalidates_predictions_and_menu_keys_are_not_predicted(self):
        c, t = self.start()
        self.warm(c, t)
        (c.home / "rewrite").touch()
        c.send(b"d", .02)
        t.wait(lambda: t.text() == "MASKED")
        before = c.log().count("TUI literal edit preview")
        c.send(b"\x1b", .1)
        c.send(b"n", .03)
        self.assertEqual(c.log().count("TUI literal edit preview"), before)
        self.assertTrue((c.home / "tui-input").read_bytes().endswith(b"\x1bn"))

    def test_backspace_restores_tail_and_control_enter_is_not_predicted(self):
        c, t = self.start()
        self.warm(c, t)
        hold = c.home / "hold"
        hold.touch()
        c.send(b"\x7f", .03)
        t.wait(lambda: t.text() == "ab")
        self.assertEqual((c.home / "ack").read_text(), "abc")
        hold.unlink()
        t.wait(lambda: (c.home / "ack").read_text() == "ab")
        before = c.log().count("TUI literal edit preview")
        c.send(b"\r", .2)
        self.assertEqual(c.log().count("TUI literal edit preview"), before)
        self.assertTrue(t.text().startswith("Try "))

    def test_type_backspace_cycle_does_not_confirm_against_a_stale_frame(self):
        c, t = self.start()
        self.warm(c, t)
        hold = c.home / "hold"
        hold.touch()
        c.send(b"d", .02)
        t.wait(lambda: t.text() == "abcd")
        c.send(b"\x7f", .02)
        t.wait(lambda: t.text() == "abc")
        self.assertEqual((c.home / "ack").read_text(), "abc")
        hold.unlink()
        c.pump(.4)
        c.finish()
        self.assertIn(b"TUI 1 previewed (0 matched, 1 discarded)", c.output)


if __name__ == "__main__":
    unittest.main()
