//! Turning cells into bytes for the real terminal.

use crate::term::{decode_color, Cell, Color, F_BLINK, F_BOLD, F_DIM, F_ITALIC, F_REVERSE, F_STRIKE, F_UNDERLINE};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::{color_idx, color_rgb};

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
