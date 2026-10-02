//! Turning cells into bytes for the real terminal.

use crate::term::{decode_color, Cell, Color, Grid, Screen, F_BLINK, F_BOLD, F_DIM, F_ITALIC, F_REVERSE, F_STRIKE, F_UNDERLINE};

fn push_num(out: &mut Vec<u8>, mut n: u32) {
    let mut buf = [0u8; 10];
    let mut i = buf.len();
    if n == 0 {
        out.push(b'0');
        return;
    }
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    out.extend_from_slice(&buf[i..]);
}

fn push_color(out: &mut Vec<u8>, c: u32, bg: bool) {
    match decode_color(c) {
        Color::Default => {}
        Color::Idx(n) => {
            out.push(b';');
            if n < 8 {
                push_num(out, (if bg { 40 } else { 30 }) + n as u32);
            } else if n < 16 {
                push_num(out, (if bg { 100 } else { 90 }) + n as u32 - 8);
            } else {
                out.extend_from_slice(if bg { b"48;5;" } else { b"38;5;" });
                push_num(out, n as u32);
            }
        }
        Color::Rgb(r, g, b) => {
            out.extend_from_slice(if bg { b";48;2;" } else { b";38;2;" });
            push_num(out, r as u32);
            out.push(b';');
            push_num(out, g as u32);
            out.push(b';');
            push_num(out, b as u32);
        }
    }
}

/// Full SGR reset + attributes of `c`.
pub fn sgr(out: &mut Vec<u8>, c: &Cell) {
    out.extend_from_slice(b"\x1b[0");
    let f = c.flags();
    if f & F_BOLD != 0 {
        out.extend_from_slice(b";1");
    }
    if f & F_DIM != 0 {
        out.extend_from_slice(b";2");
    }
    if f & F_ITALIC != 0 {
        out.extend_from_slice(b";3");
    }
    if f & F_UNDERLINE != 0 {
        out.extend_from_slice(b";4");
    }
    if f & F_BLINK != 0 {
        out.extend_from_slice(b";5");
    }
    if f & F_REVERSE != 0 {
        out.extend_from_slice(b";7");
    }
    if f & F_STRIKE != 0 {
        out.extend_from_slice(b";9");
    }
    push_color(out, c.fg(), false);
    push_color(out, c.bg, true);
    out.push(b'm');
}

pub fn cup(out: &mut Vec<u8>, y: usize, x: usize) {
    out.extend_from_slice(b"\x1b[");
    push_num(out, y as u32 + 1);
    out.push(b';');
    push_num(out, x as u32 + 1);
    out.push(b'H');
}

/// Emits cells given in row-major order, grouping consecutive cells into runs
/// and only re-sending SGR when attributes change.  Ends with a reset.
pub struct RunWriter<'a> {
    out: &'a mut Vec<u8>,
    last: Option<(usize, usize)>,
    attr: Option<(u32, u32)>,
}

impl<'a> RunWriter<'a> {
    pub fn new(out: &'a mut Vec<u8>) -> RunWriter<'a> {
        RunWriter { out, last: None, attr: None }
    }
    pub fn cell(&mut self, y: usize, x: usize, c: &Cell) {
        if c.ch == 0 {
            // right half of a wide char: drawn by the left half
            return;
        }
        if self.last != Some((y, x)) {
            cup(self.out, y, x);
        }
        if self.attr != Some((c.fga, c.bg)) {
            sgr(self.out, c);
            self.attr = Some((c.fga, c.bg));
        }
        let mut b = [0u8; 4];
        let ch = c.chr().unwrap_or(' ');
        self.out.extend_from_slice(ch.encode_utf8(&mut b).as_bytes());
        // a wide char advances the terminal cursor by two
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1).max(1);
        self.last = Some((y, x + w));
    }
    pub fn finish(self) {
        self.out.extend_from_slice(b"\x1b[0m");
    }
}

/// Draw `screen` on a terminal whose contents are unknown (it was resized).
pub fn redraw(screen: &Screen, out: &mut Vec<u8>) {
    // The same screen at another size: everything is repainted, and modes,
    // titles and the bell count, which are unchanged, are left alone.
    let mut unknown = screen.clone();
    unknown.resize(screen.grid.cols + 1, screen.grid.rows);
    repaint(&unknown, screen, out);
}

fn mode(out: &mut Vec<u8>, number: u16, on: bool) {
    out.extend_from_slice(b"\x1b[?");
    push_num(out, number as u32);
    out.push(if on { b'h' } else { b'l' });
}

/// How many rows the screen scrolled up between `old` and `new`: the top of
/// `new` is the rest of `old`, with something else below.
fn scrolled(old: &Grid, new: &Grid) -> usize {
    if old.row(0) == new.row(0) {
        return 0; // also the common case of nothing but a few changed cells
    }
    let rows = new.rows;
    (1..rows).find(|&k| (0..rows - k).all(|y| new.row(y) == old.row(y + k)) && (k..rows).any(|y| old.row(y).iter().any(|c| *c != Cell::BLANK))).unwrap_or(0)
}

/// Make a terminal that shows `from` show `to`: cells, cursor, titles,
/// modes, clipboard and bell.  This is the whole display path of a native
/// mosh session, whose server describes states, not a byte stream.
pub fn repaint(from: &Screen, to: &Screen, out: &mut Vec<u8>) {
    let (old, new) = (&from.grid, &to.grid);
    let resized = old.cols != new.cols || old.rows != new.rows;
    if to.bells != from.bells {
        out.push(0x07);
    }
    let cells_differ = resized || old.cells != new.cells;
    let mut shift = 0;
    if cells_differ {
        out.extend_from_slice(b"\x1b[?25l");
        if resized {
            out.extend_from_slice(b"\x1b[r\x1b[0m\x1b[H\x1b[2J");
        } else {
            shift = scrolled(old, new);
            if shift > 0 {
                // Let the terminal move the rows that stay: line feeds on
                // the last row scroll the whole (alternate) screen.
                out.extend_from_slice(b"\x1b[r\x1b[0m");
                cup(out, new.rows - 1, 0);
                out.extend(std::iter::repeat_n(b'\n', shift));
            }
        }
        // What the terminal shows now, to compare the target against.
        let shown = |y: usize, x: usize| {
            if resized || y + shift >= old.rows {
                Cell::BLANK
            } else {
                old.row(y + shift)[x]
            }
        };
        let mut w = RunWriter::new(out);
        for y in 0..new.rows {
            let row = new.row(y);
            let mut written: Option<usize> = None;
            for (x, cell) in row.iter().enumerate() {
                if shown(y, x) == *cell {
                    continue;
                }
                // Rewriting a few unchanged cells costs less than a cursor
                // move over them.
                if let Some(last) = written.filter(|last| x - last <= 6) {
                    for gap in last + 1..x {
                        w.cell(y, gap, &row[gap]);
                    }
                }
                w.cell(y, x, cell);
                written = Some(x);
            }
        }
        w.finish();
    }
    for (code, name, was) in [(b'1', &to.icon_name, &from.icon_name), (b'2', &to.window_title, &from.window_title)] {
        if let Some(name) = name.as_ref().filter(|_| name != was) {
            out.extend_from_slice(&[0x1b, b']', code, b';']);
            out.extend_from_slice(name.as_bytes());
            out.push(0x07);
        }
    }
    if let Some(clipboard) = to.clipboard.as_ref().filter(|_| to.clipboard != from.clipboard) {
        out.extend_from_slice(b"\x1b]52;");
        out.extend_from_slice(clipboard.as_bytes());
        out.push(0x07);
    }
    let (a, b) = (from.modes, to.modes);
    for (number, was, is) in [(5, a.reverse_video, b.reverse_video), (2004, a.bracketed_paste, b.bracketed_paste), (1004, a.focus, b.focus), (1007, a.alt_scroll, b.alt_scroll)] {
        if was != is {
            mode(out, number, is);
        }
    }
    for (was, is) in [(a.mouse, b.mouse), (a.mouse_encoding, b.mouse_encoding)] {
        if was != is {
            if was != 0 {
                mode(out, was, false);
            }
            if is != 0 {
                mode(out, is, true);
            }
        }
    }
    if cells_differ || (old.cy, old.cx) != (new.cy, new.cx) {
        cup(out, new.cy, new.cx.min(new.cols - 1));
    }
    if cells_differ {
        sgr(out, &to.attrs());
    }
    if to.cursor_visible && (cells_differ || !from.cursor_visible) {
        out.extend_from_slice(b"\x1b[?25h");
    } else if !to.cursor_visible && from.cursor_visible && !cells_differ {
        out.extend_from_slice(b"\x1b[?25l");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::{color_idx, color_rgb};

    fn screen(cols: usize, rows: usize, bytes: &[u8]) -> Screen {
        let mut s = Screen::new(cols, rows);
        vte::Parser::new().advance(&mut s, bytes);
        s
    }

    /// Replay `repaint`'s output on a terminal that shows `start`.
    fn replay(start: &Screen, target: &Screen) -> (Screen, Vec<u8>) {
        let mut out = Vec::new();
        repaint(start, target, &mut out);
        let mut shown = start.clone();
        shown.resize(target.grid.cols, target.grid.rows); // as a real terminal was
        shown.bells = 0;
        vte::Parser::new().advance(&mut shown, &out);
        (shown, out)
    }

    #[test]
    fn repaint_reaches_the_target_from_any_screen() {
        let target_bytes = b"\x1b[1;31mred\x1b[0m plain \xe6\x97\xa5\r\nsecond\x1b[?2004h\x1b[?1002h\x1b[?1006h\x1b]2;title\x07\x1b]52;c;aGk=\x07\x07\x1b[4m\x1b[2;3H";
        let target = screen(20, 4, target_bytes);
        for start in [screen(20, 4, b""), screen(20, 4, b"old text everywhere\r\nmore\r\nlines\x1b[?1000h\x1b[?25l"), screen(30, 6, b"another size")] {
            let (shown, _) = replay(&start, &target);
            assert_eq!(shown.grid.cells, target.grid.cells);
            assert_eq!((shown.grid.cy, shown.grid.cx), (target.grid.cy, target.grid.cx));
            assert_eq!(shown.attrs(), target.attrs());
            assert_eq!(shown.modes, target.modes);
            assert_eq!(shown.window_title, target.window_title);
            assert_eq!(shown.clipboard, target.clipboard);
            assert_eq!(shown.bells, 1, "one bell, however many rang in between");
            assert!(shown.cursor_visible);
        }
        // Nothing to do, nothing sent; a moved or hidden cursor alone is cheap.
        assert!(replay(&target, &target).1.is_empty());
        let moved = screen(20, 4, &[target_bytes.as_slice(), b"\x1b[1;1H\x1b[?25l"].concat());
        let (shown, out) = replay(&target, &moved);
        assert_eq!(out, b"\x1b[1;1H\x1b[?25l");
        assert!(!shown.cursor_visible);
    }

    #[test]
    fn redraw_repaints_everything_and_nothing_else() {
        let target = screen(20, 4, b"\x1b[1;31mred\x1b[0m plain\r\nsecond\x1b[?2004h\x1b]2;title\x07\x07\x1b[2;3H");
        let mut out = Vec::new();
        redraw(&target, &mut out);
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("\x1b[2J") && text.contains("red") && text.contains("second"), "{text:?}");
        assert!(!text.contains("title") && !text.contains("2004") && !out.contains(&0x07), "{text:?}");
        let mut shown = screen(20, 4, b"garbage left by the terminal's own reflow");
        vte::Parser::new().advance(&mut shown, &out);
        assert_eq!(shown.grid.cells, target.grid.cells);
        assert_eq!((shown.grid.cy, shown.grid.cx), (target.grid.cy, target.grid.cx));
    }

    #[test]
    fn scrolling_moves_rows_instead_of_redrawing_them() {
        let lines: Vec<String> = (0..12).map(|i| format!("line {i} with some text")).collect();
        let before = screen(40, 6, lines[..6].join("\r\n").as_bytes());
        let after = screen(40, 6, lines[..9].join("\r\n").as_bytes());
        let (shown, out) = replay(&before, &after);
        assert_eq!(shown.grid.cells, after.grid.cells);
        assert_eq!((shown.grid.cy, shown.grid.cx), (after.grid.cy, after.grid.cx));
        let text = String::from_utf8_lossy(&out);
        assert!(!text.contains("line 4") && text.contains("line 8 with some text"), "{text:?}");
        // A status line that stays put defeats the shortcut, not the result.
        let pinned = screen(40, 6, &[lines[3..8].join("\r\n").as_bytes(), b"\x1b[6;1Hstatus"].concat());
        let (shown, _) = replay(&screen(40, 6, &[lines[..5].join("\r\n").as_bytes(), b"\x1b[6;1Hstatus"].concat()), &pinned);
        assert_eq!(shown.grid.cells, pinned.grid.cells);
    }

    #[test]
    fn sgr_codes() {
        let mut o = Vec::new();
        let c = Cell { ch: 'a' as u32, fga: color_idx(9) | (F_BOLD << 25), bg: color_rgb(1, 2, 3) };
        sgr(&mut o, &c);
        assert_eq!(o, b"\x1b[0;1;91;48;2;1;2;3m");
        o.clear();
        sgr(&mut o, &Cell { ch: 'a' as u32, fga: color_idx(200), bg: 0 });
        assert_eq!(o, b"\x1b[0;38;5;200m");
    }

    #[test]
    fn runs_group() {
        let mut o = Vec::new();
        let mut w = RunWriter::new(&mut o);
        let c = Cell::BLANK;
        w.cell(2, 3, &Cell { ch: 'a' as u32, ..c });
        w.cell(2, 4, &Cell { ch: 'b' as u32, ..c });
        w.cell(2, 6, &Cell { ch: 'c' as u32, ..c });
        w.finish();
        assert_eq!(o, b"\x1b[3;4H\x1b[0mab\x1b[3;7Hc\x1b[0m");
    }
}
