//! A small VT100/xterm screen model on top of the `vte` parser.
//!
//! Cells are 12 bytes (char + packed fg/flags + packed bg) so a 120x40 grid is
//! ~58 KB and can be copied with a single memcpy.  Semantics follow pyte where
//! they affect the state key and diffs (cursor may rest at `cols`, erases use
//! the current attributes, ICH/DCH vacate default cells).

use unicode_width::UnicodeWidthChar;
use vte::{Params, Perform};

pub const F_BOLD: u32 = 1;
pub const F_DIM: u32 = 2;
pub const F_ITALIC: u32 = 4;
pub const F_UNDERLINE: u32 = 8;
pub const F_BLINK: u32 = 16;
pub const F_REVERSE: u32 = 32;
pub const F_STRIKE: u32 = 64;

const COLOR_MASK: u32 = 0x01FF_FFFF;
const RGB_BIT: u32 = 1 << 24;
pub const COLOR_DEFAULT: u32 = 0;

#[inline]
pub fn color_idx(n: u8) -> u32 {
    n as u32 + 1
}
#[inline]
pub fn color_rgb(r: u8, g: u8, b: u8) -> u32 {
    RGB_BIT | (r as u32) << 16 | (g as u32) << 8 | b as u32
}

/// Decoded view of a packed colour.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Color {
    Default,
    Idx(u8),
    Rgb(u8, u8, u8),
}

#[inline]
pub fn decode_color(c: u32) -> Color {
    if c == 0 {
        Color::Default
    } else if c & RGB_BIT != 0 {
        Color::Rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
    } else {
        Color::Idx((c - 1) as u8)
    }
}

/// One screen cell.  `ch == 0` marks the right half of a wide character.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(C)]
pub struct Cell {
    pub ch: u32,
    pub fga: u32, // bits 0..25 fg colour, bits 25..32 flags
    pub bg: u32,
}

impl Cell {
    pub const BLANK: Cell = Cell { ch: ' ' as u32, fga: 0, bg: 0 };
    #[inline]
    pub fn fg(&self) -> u32 {
        self.fga & COLOR_MASK
    }
    #[inline]
    pub fn flags(&self) -> u32 {
        self.fga >> 25
    }
    #[inline]
    pub fn chr(&self) -> Option<char> {
        if self.ch == 0 {
            None
        } else {
            char::from_u32(self.ch)
        }
    }
    #[inline]
    fn with_fg(self, c: u32) -> Cell {
        Cell { fga: (self.fga & !COLOR_MASK) | c, ..self }
    }
    #[inline]
    fn with_flags(self, f: u32) -> Cell {
        Cell { fga: (self.fga & COLOR_MASK) | (f << 25), ..self }
    }
}

impl Default for Cell {
    fn default() -> Self {
        Cell::BLANK
    }
}

/// A rectangular cell buffer with a cursor.
#[derive(Clone, Debug)]
pub struct Grid {
    pub cols: usize,
    pub rows: usize,
    pub cells: Vec<Cell>,
    pub cx: usize, // 0..=cols (== cols means "pending wrap")
    pub cy: usize,
}

impl Grid {
    pub const BLANK_CELL: Cell = Cell::BLANK;
    pub fn new(cols: usize, rows: usize) -> Grid {
        Grid { cols, rows, cells: vec![Cell::BLANK; cols * rows], cx: 0, cy: 0 }
    }
    #[inline]
    pub fn row(&self, y: usize) -> &[Cell] {
        &self.cells[y * self.cols..(y + 1) * self.cols]
    }
    #[inline]
    pub fn row_mut(&mut self, y: usize) -> &mut [Cell] {
        let c = self.cols;
        &mut self.cells[y * c..(y + 1) * c]
    }
    /// Copy another grid into this one, reusing the allocation.
    pub fn copy_from(&mut self, o: &Grid) {
        self.cols = o.cols;
        self.rows = o.rows;
        self.cells.clear();
        self.cells.extend_from_slice(&o.cells);
        self.cx = o.cx;
        self.cy = o.cy;
    }
    /// Copy only rows `from..` (rows above are left untouched / stale).
    pub fn copy_rows_from(&mut self, o: &Grid, from: usize) {
        if self.cols != o.cols || self.rows != o.rows {
            self.copy_from(o);
            return;
        }
        let s = from.min(o.rows) * o.cols;
        self.cells[s..].copy_from_slice(&o.cells[s..]);
        self.cx = o.cx;
        self.cy = o.cy;
    }
    pub fn resize(&mut self, cols: usize, rows: usize) {
        if cols == self.cols && rows == self.rows {
            return;
        }
        let mut new = vec![Cell::BLANK; cols * rows];
        // like pyte: drop rows from the top when shrinking, keep the bottom
        let drop = self.rows.saturating_sub(rows);
        let keep_rows = self.rows.min(rows);
        let keep_cols = self.cols.min(cols);
        for y in 0..keep_rows {
            let src = &self.cells[(y + drop) * self.cols..(y + drop) * self.cols + keep_cols];
            new[y * cols..y * cols + keep_cols].copy_from_slice(src);
        }
        self.cells = new;
        self.cy = self.cy.saturating_sub(drop).min(rows - 1);
        self.cx = self.cx.min(cols);
        self.cols = cols;
        self.rows = rows;
    }
}

/// Emulator state: a grid plus the VT state machine that mutates it.
pub struct Screen {
    pub grid: Grid,
    attrs: Cell,
    saved: Option<(usize, usize, Cell)>,
    margins: Option<(usize, usize)>,
    autowrap: bool,
    origin: bool,
    insert: bool,
    lnm: bool,
    pub alt: bool,
    alt_cursor: (usize, usize),
    pub dirty: bool,
    /// When false, DECSET 47/1047/1049 are ignored and drawing continues on
    /// the one grid (mosh-client renders everything inside the alternate screen).
    pub track_alt: bool,
}

impl Screen {
    pub fn new(cols: usize, rows: usize) -> Screen {
        Screen {
            grid: Grid::new(cols, rows),
            attrs: Cell::BLANK,
            saved: None,
            margins: None,
            autowrap: true,
            origin: false,
            insert: false,
            lnm: false,
            alt: false,
            alt_cursor: (0, 0),
            dirty: false,
            track_alt: true,
        }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.grid.resize(cols, rows);
        self.margins = None;
        self.dirty = true;
    }

    #[inline]
    fn cols(&self) -> usize {
        self.grid.cols
    }
    #[inline]
    fn rows(&self) -> usize {
        self.grid.rows
    }
    #[inline]
    fn bounds(&self) -> (usize, usize) {
        self.margins.unwrap_or((0, self.grid.rows - 1))
    }
    #[inline]
    fn blank(&self) -> Cell {
        Cell { ch: ' ' as u32, ..self.attrs }
    }
    fn ensure_hbounds(&mut self) {
        self.grid.cx = self.grid.cx.min(self.cols() - 1);
    }
    fn ensure_vbounds(&mut self, use_margins: bool) {
        let (top, bottom) = if (use_margins || self.origin) && self.margins.is_some() {
            self.bounds()
        } else {
            (0, self.rows() - 1)
        };
        self.grid.cy = self.grid.cy.clamp(top, bottom);
    }

    fn reset(&mut self) {
        self.grid.cells.fill(Cell::BLANK);
        self.grid.cx = 0;
        self.grid.cy = 0;
        self.attrs = Cell::BLANK;
        self.saved = None;
        self.margins = None;
        self.autowrap = true;
        self.origin = false;
        self.insert = false;
        self.lnm = false;
        self.dirty = true;
    }

    // ---- cursor movement
    fn cursor_up(&mut self, n: usize) {
        let (top, _) = self.bounds();
        self.grid.cy = self.grid.cy.saturating_sub(n.max(1)).max(top);
    }
    fn cursor_down(&mut self, n: usize) {
        let (_, bottom) = self.bounds();
        self.grid.cy = (self.grid.cy + n.max(1)).min(bottom);
    }
    fn cursor_back(&mut self, n: usize) {
        if self.grid.cx == self.cols() {
            self.grid.cx -= 1;
        }
        self.grid.cx = self.grid.cx.saturating_sub(n.max(1));
        self.ensure_hbounds();
    }
    fn cursor_forward(&mut self, n: usize) {
        self.grid.cx += n.max(1);
        self.ensure_hbounds();
    }
    fn cursor_to_column(&mut self, col: usize) {
        self.grid.cx = col.max(1) - 1;
        self.ensure_hbounds();
    }
    fn cursor_to_line(&mut self, line: usize) {
        let mut y = line.max(1) - 1;
        if self.origin {
            y += self.bounds().0;
        }
        self.grid.cy = y;
        self.ensure_vbounds(false);
    }
    fn cursor_position(&mut self, line: usize, col: usize) {
        let mut y = line.max(1) - 1;
        let x = col.max(1) - 1;
        if let (Some((top, bottom)), true) = (self.margins, self.origin) {
            y += top;
            if y < top || y > bottom {
                return;
            }
        }
        self.grid.cx = x;
        self.grid.cy = y;
        self.ensure_hbounds();
        self.ensure_vbounds(false);
    }
    fn carriage_return(&mut self) {
        self.grid.cx = 0;
    }
    fn index(&mut self) {
        let (top, bottom) = self.bounds();
        if self.grid.cy == bottom {
            self.scroll_up(top, bottom, 1);
        } else {
            self.cursor_down(1);
        }
    }
    fn reverse_index(&mut self) {
        let (top, bottom) = self.bounds();
        if self.grid.cy == top {
            self.scroll_down(top, bottom, 1);
        } else {
            self.cursor_up(1);
        }
    }
    fn linefeed(&mut self) {
        self.index();
        if self.lnm {
            self.carriage_return();
        }
    }
    fn tab(&mut self) {
        let x = self.grid.cx;
        let next = (x / 8 + 1) * 8;
        self.grid.cx = if next < self.cols() { next } else { self.cols() - 1 };
    }
    fn back_tab(&mut self, n: usize) {
        for _ in 0..n.max(1) {
            let x = self.grid.cx.min(self.cols() - 1);
            self.grid.cx = if x == 0 { 0 } else { ((x - 1) / 8) * 8 };
        }
    }

    // ---- scrolling / editing
    fn scroll_up(&mut self, top: usize, bottom: usize, n: usize) {
        let cols = self.cols();
        let n = n.min(bottom - top + 1);
        self.grid.cells[top * cols..(bottom + 1) * cols].rotate_left(n * cols);
        self.grid.cells[(bottom + 1 - n) * cols..(bottom + 1) * cols].fill(Cell::BLANK);
        self.dirty = true;
    }
    fn scroll_down(&mut self, top: usize, bottom: usize, n: usize) {
        let cols = self.cols();
        let n = n.min(bottom - top + 1);
        self.grid.cells[top * cols..(bottom + 1) * cols].rotate_right(n * cols);
        self.grid.cells[top * cols..(top + n) * cols].fill(Cell::BLANK);
        self.dirty = true;
    }
    fn insert_lines(&mut self, n: usize) {
        let (top, bottom) = self.bounds();
        let y = self.grid.cy;
        if y >= top && y <= bottom {
            self.scroll_down(y, bottom, n.max(1));
        }
        self.carriage_return();
    }
    fn delete_lines(&mut self, n: usize) {
        let (top, bottom) = self.bounds();
        let y = self.grid.cy;
        if y >= top && y <= bottom {
            self.scroll_up(y, bottom, n.max(1));
        }
        self.carriage_return();
    }
    fn insert_characters(&mut self, n: usize) {
        let cols = self.cols();
        let x = self.grid.cx.min(cols - 1);
        let n = n.max(1).min(cols - x);
        let row = self.grid.row_mut(self.grid.cy);
        row[x..].rotate_right(n);
        row[x..x + n].fill(Cell::BLANK);
        self.dirty = true;
    }
    fn delete_characters(&mut self, n: usize) {
        let cols = self.cols();
        let x = self.grid.cx.min(cols - 1);
        let n = n.max(1).min(cols - x);
        let row = self.grid.row_mut(self.grid.cy);
        row[x..].rotate_left(n);
        row[cols - n..].fill(Cell::BLANK);
        self.dirty = true;
    }
    fn erase_characters(&mut self, n: usize) {
        let cols = self.cols();
        let x = self.grid.cx.min(cols - 1);
        let end = (x + n.max(1)).min(cols);
        let b = self.blank();
        self.grid.row_mut(self.grid.cy)[x..end].fill(b);
        self.dirty = true;
    }
    fn erase_in_line(&mut self, how: u16) {
        let cols = self.cols();
        let x = self.grid.cx;
        let b = self.blank();
        let row = self.grid.row_mut(self.grid.cy);
        match how {
            0 => row[x.min(cols)..].fill(b),
            1 => row[..(x + 1).min(cols)].fill(b),
            2 => row.fill(b),
            _ => {}
        }
        self.dirty = true;
    }
    fn erase_in_display(&mut self, how: u16) {
        let cols = self.cols();
        let rows = self.rows();
        let y = self.grid.cy;
        let b = self.blank();
        match how {
            0 => self.grid.cells[(y + 1).min(rows) * cols..].fill(b),
            1 => self.grid.cells[..y * cols].fill(b),
            2 | 3 => self.grid.cells.fill(b),
            _ => return,
        }
        if how < 2 {
            self.erase_in_line(how);
        }
        self.dirty = true;
    }
    fn set_margins(&mut self, top: usize, bottom: usize) {
        let rows = self.rows();
        if top == 0 && bottom == 0 {
            self.margins = None;
            return;
        }
        let top = top.max(1).min(rows) - 1;
        let bottom = if bottom == 0 { rows } else { bottom.min(rows) } - 1;
        if bottom > top {
            self.margins = Some((top, bottom));
            self.cursor_position(0, 0);
        }
    }

    fn draw(&mut self, c: char) {
        let width = match c.width() {
            Some(w) => w,
            None => return,
        };
        if width == 0 {
            return; // combining marks are dropped
        }
        let cols = self.cols();
        if self.grid.cx == cols {
            if self.autowrap {
                self.carriage_return();
                self.linefeed();
            } else {
                self.grid.cx -= width.min(self.grid.cx);
            }
        }
        if self.insert {
            self.insert_characters(width);
        }
        let x = self.grid.cx;
        let cell = Cell { ch: c as u32, ..self.attrs };
        let row = self.grid.row_mut(self.grid.cy);
        row[x] = cell;
        if width == 2 && x + 1 < cols {
            row[x + 1] = Cell { ch: 0, ..self.attrs };
        }
        self.grid.cx = (x + width).min(cols);
        self.dirty = true;
    }

    fn sgr(&mut self, params: &Params) {
        if params.is_empty() {
            self.attrs = Cell::BLANK;
            return;
        }
        // Flatten so both "38;5;n" and "38:5:n" work.
        let mut flat: [u16; 32] = [0; 32];
        let mut n = 0;
        for p in params.iter() {
            for &v in p {
                if n < 32 {
                    flat[n] = v;
                    n += 1;
                }
            }
        }
        let mut i = 0;
        let mut a = self.attrs;
        while i < n {
            let v = flat[i];
            let mut f = a.flags();
            match v {
                0 => a = Cell::BLANK,
                1 => f |= F_BOLD,
                2 => f |= F_DIM,
                3 => f |= F_ITALIC,
                4 => f |= F_UNDERLINE,
                5 | 6 => f |= F_BLINK,
                7 => f |= F_REVERSE,
                9 => f |= F_STRIKE,
                22 => f &= !(F_BOLD | F_DIM),
                23 => f &= !F_ITALIC,
                24 => f &= !F_UNDERLINE,
                25 => f &= !F_BLINK,
                27 => f &= !F_REVERSE,
                29 => f &= !F_STRIKE,
                30..=37 => a = a.with_fg(color_idx((v - 30) as u8)),
                90..=97 => a = a.with_fg(color_idx((v - 90 + 8) as u8)),
                39 => a = a.with_fg(COLOR_DEFAULT),
                40..=47 => a.bg = color_idx((v - 40) as u8),
                100..=107 => a.bg = color_idx((v - 100 + 8) as u8),
                49 => a.bg = COLOR_DEFAULT,
                38 | 48 => {
                    let mut col = None;
                    if i + 2 < n && flat[i + 1] == 5 {
                        col = Some(color_idx(flat[i + 2] as u8));
                        i += 2;
                    } else if i + 4 < n && flat[i + 1] == 2 {
                        col = Some(color_rgb(flat[i + 2] as u8, flat[i + 3] as u8, flat[i + 4] as u8));
                        i += 4;
                    }
                    if let Some(c) = col {
                        if v == 38 {
                            a = a.with_fg(c);
                        } else {
                            a.bg = c;
                        }
                    }
                }
                _ => {}
            }
            if !matches!(v, 0 | 30..=49 | 90..=107) {
                a = a.with_flags(f);
            }
            i += 1;
        }
        self.attrs = a;
    }

    fn set_mode(&mut self, params: &Params, private: bool, on: bool) {
        for p in params.iter() {
            let m = p[0];
            if private {
                match m {
                    7 => self.autowrap = on,
                    6 => {
                        self.origin = on;
                        self.cursor_position(0, 0);
                    }
                    47 | 1047 | 1049 if self.track_alt => {
                        if on && !self.alt {
                            self.alt = true;
                            self.alt_cursor = (self.grid.cx, self.grid.cy);
                        } else if !on && self.alt {
                            self.alt = false;
                            self.grid.cx = self.alt_cursor.0;
                            self.grid.cy = self.alt_cursor.1;
                            self.dirty = true;
                        }
                    }
                    _ => {}
                }
            } else {
                match m {
                    4 => self.insert = on,
                    20 => self.lnm = on,
                    _ => {}
                }
            }
        }
    }
}

#[inline]
fn p0(params: &Params, idx: usize) -> usize {
    params.iter().nth(idx).map(|p| p[0] as usize).unwrap_or(0)
}

impl Perform for Screen {
    fn print(&mut self, c: char) {
        if self.alt {
            return;
        }
        self.draw(c);
    }

    fn execute(&mut self, byte: u8) {
        if self.alt {
            return;
        }
        match byte {
            0x08 => self.cursor_back(1),
            0x09 => self.tab(),
            0x0a | 0x0b | 0x0c => self.linefeed(),
            0x0d => self.carriage_return(),
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        let private = intermediates.first() == Some(&b'?');
        if self.alt {
            // Only mode changes matter while an app owns the screen.
            if private && (action == 'h' || action == 'l') {
                self.set_mode(params, true, action == 'h');
            }
            return;
        }
        if !intermediates.is_empty() && !private {
            return;
        }
        match action {
            'A' => self.cursor_up(p0(params, 0)),
            'B' | 'e' => self.cursor_down(p0(params, 0)),
            'C' | 'a' => self.cursor_forward(p0(params, 0)),
            'D' => self.cursor_back(p0(params, 0)),
            'E' => {
                self.cursor_down(p0(params, 0));
                self.carriage_return();
            }
            'F' => {
                self.cursor_up(p0(params, 0));
                self.carriage_return();
            }
            'G' | '`' => self.cursor_to_column(p0(params, 0)),
            'H' | 'f' => self.cursor_position(p0(params, 0), p0(params, 1)),
            'J' => self.erase_in_display(p0(params, 0) as u16),
            'K' => self.erase_in_line(p0(params, 0) as u16),
            'L' => self.insert_lines(p0(params, 0)),
            'M' => self.delete_lines(p0(params, 0)),
            'P' => self.delete_characters(p0(params, 0)),
            'S' => {
                let (t, b) = self.bounds();
                self.scroll_up(t, b, p0(params, 0).max(1));
            }
            'T' => {
                let (t, b) = self.bounds();
                self.scroll_down(t, b, p0(params, 0).max(1));
            }
            'X' => self.erase_characters(p0(params, 0)),
            'Z' => self.back_tab(p0(params, 0)),
            '@' => self.insert_characters(p0(params, 0)),
            'd' => self.cursor_to_line(p0(params, 0)),
            'm' => self.sgr(params),
            'r' => self.set_margins(p0(params, 0), p0(params, 1)),
            's' => self.saved = Some((self.grid.cx, self.grid.cy, self.attrs)),
            'u' => {
                if let Some((x, y, a)) = self.saved {
                    self.grid.cx = x;
                    self.grid.cy = y;
                    self.attrs = a;
                    self.ensure_hbounds();
                    self.ensure_vbounds(true);
                }
            }
            'h' => self.set_mode(params, private, true),
            'l' => self.set_mode(params, private, false),
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        if self.alt {
            return;
        }
        match (intermediates, byte) {
            ([], b'7') => self.saved = Some((self.grid.cx, self.grid.cy, self.attrs)),
            ([], b'8') => {
                if let Some((x, y, a)) = self.saved {
                    self.grid.cx = x;
                    self.grid.cy = y;
                    self.attrs = a;
                    self.ensure_hbounds();
                    self.ensure_vbounds(true);
                } else {
                    self.grid.cx = 0;
                    self.grid.cy = 0;
                    self.attrs = Cell::BLANK;
                }
            }
            ([], b'D') => self.index(),
            ([], b'E') => {
                self.index();
                self.carriage_return();
            }
            ([], b'M') => self.reverse_index(),
            ([], b'c') => self.reset(),
            (b"#", b'8') => {
                self.grid.cells.fill(Cell { ch: 'E' as u32, fga: 0, bg: 0 });
                self.dirty = true;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(s: &mut Screen, bytes: &[u8]) {
        let mut p = vte::Parser::new();
        p.advance(s, bytes);
    }
    fn text(g: &Grid, y: usize) -> String {
        g.row(y).iter().filter_map(|c| c.chr()).collect::<String>().trim_end().to_string()
    }

    #[test]
    fn basic_draw_and_wrap() {
        let mut s = Screen::new(5, 3);
        feed(&mut s, b"hello world");
        assert_eq!(text(&s.grid, 0), "hello");
        assert_eq!(text(&s.grid, 1), " worl");
        assert_eq!(text(&s.grid, 2), "d");
        assert_eq!((s.grid.cx, s.grid.cy), (1, 2));
        feed(&mut s, b"\r\n\r\nx");
        assert_eq!(text(&s.grid, 0), "d");
        assert_eq!(text(&s.grid, 1), "");
        assert_eq!(text(&s.grid, 2), "x");
    }

    #[test]
    fn pending_wrap_cursor() {
        let mut s = Screen::new(4, 2);
        feed(&mut s, b"abcd");
        assert_eq!(s.grid.cx, 4);
        feed(&mut s, b"\x1b[K");
        assert_eq!(text(&s.grid, 0), "abcd");
        feed(&mut s, b"\x08");
        assert_eq!(s.grid.cx, 2);
    }

    #[test]
    fn sgr_and_erase() {
        let mut s = Screen::new(10, 2);
        feed(&mut s, b"\x1b[1;31mab\x1b[0m\x1b[38;5;200mc\x1b[48;2;1;2;3md\x1b[m");
        let r = s.grid.row(0);
        assert_eq!(r[0].flags(), F_BOLD);
        assert_eq!(decode_color(r[0].fg()), Color::Idx(1));
        assert_eq!(decode_color(r[2].fg()), Color::Idx(200));
        assert_eq!(decode_color(r[3].bg), Color::Rgb(1, 2, 3));
        assert_eq!(r[4], Cell::BLANK);
        feed(&mut s, b"\x1b[44m\x1b[1K");
        assert_eq!(decode_color(s.grid.row(0)[0].bg), Color::Idx(4));
        assert_eq!(s.grid.row(0)[0].chr(), Some(' '));
    }

    #[test]
    fn ich_dch_il_dl() {
        let mut s = Screen::new(6, 3);
        feed(&mut s, b"abcdef\x1b[1;2H\x1b[2@");
        assert_eq!(text(&s.grid, 0), "a  bcd");
        feed(&mut s, b"\x1b[1;1H\x1b[P");
        assert_eq!(text(&s.grid, 0), "  bcd");
        feed(&mut s, b"\x1b[2;1Hrow2\x1b[3;1Hrow3\x1b[2;1H\x1b[L");
        assert_eq!(text(&s.grid, 1), "");
        assert_eq!(text(&s.grid, 2), "row2");
        feed(&mut s, b"\x1b[1;1H\x1b[M");
        assert_eq!(text(&s.grid, 0), "");
        assert_eq!(text(&s.grid, 1), "row2");
    }

    #[test]
    fn wide_chars() {
        let mut s = Screen::new(5, 1);
        feed(&mut s, "日本x".as_bytes());
        let r = s.grid.row(0);
        assert_eq!(r[0].chr(), Some('日'));
        assert_eq!(r[1].ch, 0);
        assert_eq!(r[4].chr(), Some('x'));
        assert_eq!(s.grid.cx, 5);
    }

    #[test]
    fn alt_screen_is_ignored() {
        let mut s = Screen::new(5, 2);
        feed(&mut s, b"ab\x1b[?1049hzzz\x1b[2J\x1b[?1049lc");
        assert_eq!(text(&s.grid, 0), "abc");
        assert!(!s.alt);
    }

    #[test]
    fn scroll_region() {
        let mut s = Screen::new(3, 4);
        feed(&mut s, b"1\r\n2\r\n3\r\n4\x1b[2;3r\x1b[3;1H\n");
        assert_eq!(text(&s.grid, 0), "1");
        assert_eq!(text(&s.grid, 1), "3");
        assert_eq!(text(&s.grid, 2), "");
        assert_eq!(text(&s.grid, 3), "4");
    }

    #[test]
    fn resize_keeps_bottom() {
        let mut g = Grid::new(4, 3);
        g.row_mut(2)[0] = Cell { ch: 'x' as u32, ..Cell::BLANK };
        g.cy = 2;
        g.resize(3, 2);
        assert_eq!(g.row(1)[0].chr(), Some('x'));
        assert_eq!(g.cy, 1);
    }
}
