//! A conservative local preview over Claude Code's *existing* terminal UI.
//!
//! No agent protocol, replacement UI, generated input, or persistent cache.
//! Learn literal editing only inside a recognized, single-line composer.
//! Observed text/cursor matches are evidence, not server-side input ACKs:
//! uncertain layouts and nonliteral actions always fall back to passthrough.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::paint::{cup, sgr, RunWriter};
use crate::term::{Cell, Screen, F_REVERSE};

const CONFIRMATIONS: usize = 2;
const MAX_PENDING: usize = 64;

/// Inserting paint commands inside a split UTF-8/CSI/OSC/DCS sequence would
/// corrupt the remote stream. This tracks safe insertion points conservatively,
/// independently of the screen model and its private OSC filtering.
#[derive(Default)]
struct Boundary {
    state: u8, // 0 ground, 1 ESC, 2 CSI, 3 OSC, 4 other control string, 5 string ESC
    utf8: u8,
}

impl Boundary {
    fn safe(&self) -> bool {
        self.state == 0 && self.utf8 == 0
    }

    fn feed(&mut self, bytes: &[u8]) {
        let mut bytes = bytes;
        while !bytes.is_empty() {
            if self.safe() {
                // Most terminal output is plain ASCII. Skip whole runs rather
                // than dispatching every character through the state machine.
                let Some(i) = bytes.iter().position(|&b| b == 0x1b || b >= 0x80) else { return; };
                bytes = &bytes[i..];
            }
            let b = bytes[0];
            bytes = &bytes[1..];
            if matches!(b, 0x18 | 0x1a) {
                self.state = 0;
                self.utf8 = 0;
                continue;
            }
            if self.state == 0 && self.utf8 > 0 {
                self.utf8 -= 1;
                if (0x80..=0xbf).contains(&b) {
                    continue;
                }
                self.utf8 = 0;
            }
            match self.state {
                0 => match b {
                    0x1b => self.state = 1,
                    0xc2..=0xdf => self.utf8 = 1,
                    0xe0..=0xef => self.utf8 = 2,
                    0xf0..=0xf4 => self.utf8 = 3,
                    _ => {}
                },
                1 => match b {
                    b'[' => self.state = 2,
                    b']' => self.state = 3,
                    b'P' | b'_' | b'^' | b'X' => self.state = 4,
                    0x1b | 0x20..=0x2f => {}
                    0x30..=0x7e => self.state = 0,
                    _ => {}
                },
                2 => {
                    if b == 0x1b {
                        self.state = 1;
                    } else if (0x40..=0x7e).contains(&b) {
                        self.state = 0;
                    }
                }
                3 | 4 => {
                    if b == 0x1b {
                        self.state = 5;
                    } else if b == 0x07 && self.state == 3 {
                        self.state = 0;
                    }
                }
                5 => {
                    // ST ends the string; other escapes cancel it and start
                    // the corresponding escape sequence.
                    self.state = 1;
                    self.feed(&[b]);
                }
                _ => unreachable!(),
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Field {
    row: usize,
    cols: usize,
    text: Vec<u8>,
    cursor: usize,
    style: Cell,
}

impl Field {
    const START: usize = 2;

    fn same_layout(&self, other: &Self) -> bool {
        self.row == other.row && self.cols == other.cols && self.style == other.style
    }

    fn edit(&self, input: &[u8]) -> Option<Self> {
        // Cursor-at-start is ambiguous with Claude's placeholder. Never guess
        // it away, or synthesize the first character of an unknown field.
        if self.cursor == 0 || self.text.starts_with(b"/") || self.text.starts_with(b"!") {
            return None;
        }
        let mut next = self.clone();
        match input {
            [b'\x7f'] if self.text.len() > 1 => {
                next.text.remove(next.cursor - 1);
                next.cursor -= 1;
            }
            [b] if (0x20..=0x7e).contains(b) && *b != b'@' => {
                next.text.insert(next.cursor, *b);
                next.cursor += 1;
            }
            _ => return None,
        }
        // Leave wrapping, slash/bang modes and mention pickers to the app.
        if next.text.len() + Self::START >= self.cols - 1
            || next.text.starts_with(b"/") || next.text.starts_with(b"!")
        {
            return None;
        }
        Some(next)
    }
}

fn border(row: &[Cell]) -> bool {
    row.iter().filter(|c| c.ch == '─' as u32).count() >= row.len().saturating_sub(2)
}

fn composer(screen: &Screen) -> Option<Field> {
    let g = &screen.grid;
    if screen.alt || !screen.cursor_visible || !(20..=4096).contains(&g.cols)
        || g.cy == 0 || g.cy + 1 >= g.rows || g.cx < Field::START || g.cx >= g.cols - 1
    {
        return None;
    }
    let row = g.row(g.cy);
    if row[0].ch != '❯' as u32 || !matches!(row[1].ch, 0x20 | 0xa0)
        || !border(g.row(g.cy - 1)) || !border(g.row(g.cy + 1))
    {
        return None;
    }
    let end = row.iter().rposition(|c| c.ch != b' ' as u32)
        .map_or(Field::START, |x| x + 1).max(g.cx);
    if end >= g.cols - 1 {
        return None;
    }
    let style = Cell { ch: b' ' as u32, ..row[Field::START] };
    if style.flags() & F_REVERSE != 0 {
        return None;
    }
    let mut text = Vec::with_capacity(end - Field::START);
    for c in &row[Field::START..end] {
        // No guesses about graphemes, selections, styled mentions, or masks.
        if !(0x20..=0x7e).contains(&c.ch) || c.fga != style.fga || c.bg != style.bg {
            return None;
        }
        text.push(c.ch as u8);
    }
    if text.starts_with(b"/") || text.starts_with(b"!") || text.contains(&b'@')
        || (!text.is_empty() && text.iter().all(|b| *b == b'*'))
    {
        return None;
    }
    Some(Field { row: g.cy, cols: g.cols, text, cursor: g.cx - Field::START, style })
}

fn claude_banner(screen: &Screen) -> bool {
    (0..screen.grid.rows).any(|y| {
        let text: String = screen.grid.row(y).iter().filter_map(Cell::chr).collect();
        text.contains("Claude Code v")
    })
}

struct Pending {
    expected: Field,
    sent: Instant,
    shown: bool,
}

#[derive(Default)]
pub struct Predictor {
    boundary: Boundary,
    identified: bool,
    current: Option<Field>,
    confidence: usize,
    pending: VecDeque<Pending>,
    overlay: Option<(usize, usize)>, // row, exclusive end column
    pub predicted: u64,
    pub confirmed: u64,
    pub discarded: u64,
}

impl Predictor {
    pub fn output(&mut self, bytes: &[u8]) {
        self.boundary.feed(bytes);
    }

    pub fn active(&self) -> bool {
        self.current.is_some()
    }

    /// Cancel the editing epoch; a new real frame must re-establish the field.
    pub fn cancel(&mut self) {
        self.discarded += self.pending.iter().filter(|p| p.shown).count() as u64;
        self.pending.clear();
        self.current = None;
        self.confidence = 0;
    }

    /// A shell prompt or resize ends application identity as well.
    pub fn reset(&mut self) {
        self.cancel();
        self.identified = false;
        self.overlay = None;
    }

    pub fn observe(&mut self, screen: &Screen) {
        if !self.boundary.safe() {
            return;
        }
        let Some(field) = composer(screen) else {
            self.cancel();
            return;
        };
        if !self.identified {
            if !claude_banner(screen) {
                self.cancel();
                return;
            }
            self.identified = true;
        }
        if self.current.as_ref().is_some_and(|old| !old.same_layout(&field)) {
            self.cancel();
        }
        if let Some(i) = self.pending.iter().rposition(|p| p.expected == field) {
            // One Mosh update may confirm several coalesced edits.
            for p in self.pending.drain(..=i) {
                self.confirmed += u64::from(p.shown);
                self.confidence = (self.confidence + 1).min(CONFIRMATIONS);
            }
        } else if self.current.as_ref() != Some(&field) {
            // Completion, history, a moved cursor, or a rewritten input. Never
            // apply old guesses to a newly changed field.
            self.cancel();
        }
        self.current = Some(field);
    }

    /// Called only for actual user input, which the caller sends unchanged.
    /// Returns true iff this key is being locally previewed.
    pub fn input(&mut self, input: &[u8], now: Instant) -> bool {
        if !self.boundary.safe() || self.pending.len() >= MAX_PENDING {
            self.cancel();
            return false;
        }
        let base = self.pending.back().map(|p| &p.expected).or(self.current.as_ref());
        let Some(next) = base.and_then(|f| f.edit(input)) else {
            self.cancel();
            return false;
        };
        // An edit cycle (e.g. type then immediately backspace) can look exactly
        // like an old, unacknowledged frame. Without server input ACKs, do not
        // mistake that repeated state for confirmation of the whole cycle.
        if self.current.as_ref() == Some(&next) || self.pending.iter().any(|p| p.expected == next) {
            self.cancel();
            return false;
        }
        let shown = self.confidence >= CONFIRMATIONS;
        self.predicted += u64::from(shown);
        self.pending.push_back(Pending { expected: next, sent: now, shown });
        shown
    }

    pub fn deadline(&self, rtt: f64) -> Option<Instant> {
        self.pending.front().map(|p| p.sent + Duration::from_secs_f64((4.0 * rtt).clamp(0.5, 2.0)))
    }

    /// Restore before parsing *any* new remote bytes: their cursor-relative
    /// writes must see the actual cursor and attributes, not the local guess.
    pub fn restore(&mut self, screen: &Screen, out: &mut Vec<u8>) {
        let Some((row, end)) = self.overlay.take() else { return; };
        let g = &screen.grid;
        if row < g.rows {
            let mut w = RunWriter::new(out);
            for x in Field::START..end.min(g.cols) {
                w.cell(row, x, &g.row(row)[x]);
            }
            w.finish();
        }
        cup(out, g.cy, g.cx);
        sgr(out, &screen.attrs());
    }

    pub fn paint(&mut self, screen: &Screen, out: &mut Vec<u8>) {
        if !self.boundary.safe() {
            return;
        }
        let Some(last) = self.pending.back().filter(|p| p.shown) else { return; };
        let Some(current) = &self.current else { return; };
        let next = &last.expected;
        let g = &screen.grid;
        if !current.same_layout(next) || next.row >= g.rows || next.cols != g.cols {
            return;
        }
        let end = Field::START + current.text.len().max(next.text.len());
        let mut w = RunWriter::new(out);
        for x in Field::START..end {
            let c = Cell { ch: next.text.get(x - Field::START).copied().unwrap_or(b' ') as u32, ..next.style };
            if c != g.row(next.row)[x] {
                w.cell(next.row, x, &c);
            }
        }
        w.finish();
        cup(out, next.row, Field::START + next.cursor);
        sgr(out, &screen.attrs());
        self.overlay = Some((next.row, end));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(text: &str, cursor: usize) -> Screen {
        let mut s = Screen::new(80, 24);
        s.track_alt = false;
        for (x, c) in "Claude Code vTEST".chars().enumerate() {
            s.grid.row_mut(1)[x].ch = c as u32;
        }
        for y in [9, 11] {
            s.grid.row_mut(y).fill(Cell { ch: '─' as u32, ..Cell::BLANK });
        }
        for (x, c) in format!("❯\u{a0}{text}").chars().enumerate() {
            s.grid.row_mut(10)[x].ch = c as u32;
        }
        s.grid.cy = 10;
        s.grid.cx = cursor + 2;
        s
    }

    fn warm() -> (Predictor, Screen) {
        let mut p = Predictor::default();
        p.observe(&screen("a", 1));
        assert!(!p.input(b"b", Instant::now()));
        p.observe(&screen("ab", 2));
        assert!(!p.input(b"c", Instant::now()));
        let s = screen("abc", 3);
        p.observe(&s);
        assert_eq!(p.confidence, CONFIRMATIONS);
        (p, s)
    }

    #[test]
    fn never_inserts_paint_inside_split_terminal_sequences() {
        for sequence in [
            b"\x1b[31m".as_slice(), b"\x1b]0;title\x1b\\", b"\x1b]0;title\x07",
            b"\x1bPdata\x1b\\", b"\x1b_Gdata\x1b\\", "界".as_bytes(),
        ] {
            for split in 1..sequence.len() {
                let (mut p, s) = warm();
                p.output(&sequence[..split]);
                assert!(!p.boundary.safe(), "{sequence:?} split {split}");
                assert!(!p.input(b"d", Instant::now()));
                let mut out = Vec::new();
                p.paint(&s, &mut out);
                assert!(out.is_empty());
                p.output(&sequence[split..]);
                assert!(p.boundary.safe(), "{sequence:?} split {split}");
            }
        }
    }

    #[test]
    fn learns_echo_then_previews_and_confirms_coalesced_typing() {
        let (mut p, s) = warm();
        assert!(p.input(b"d", Instant::now()));
        assert!(p.input(b"e", Instant::now()));
        let mut out = Vec::new();
        p.paint(&s, &mut out);
        assert!(out.windows(2).any(|w| w == b"de"));
        p.observe(&screen("abcde", 5));
        assert!(p.pending.is_empty());
        assert_eq!((p.predicted, p.confirmed, p.discarded), (2, 2, 0));
    }

    #[test]
    fn unrelated_output_keeps_pending_but_rewritten_field_cancels_it() {
        let (mut p, mut s) = warm();
        assert!(p.input(b"d", Instant::now()));
        s.grid.row_mut(2)[0].ch = 'x' as u32; // spinner/output above the composer
        p.observe(&s);
        assert_eq!(p.pending.len(), 1);
        p.observe(&screen("****", 4)); // masking, completion or another rewrite
        assert!(p.pending.is_empty());
        assert_eq!((p.confidence, p.discarded), (0, 1));
        assert!(!p.input(b"e", Instant::now()));
    }

    #[test]
    fn controls_bulk_input_and_special_modes_never_predict() {
        for key in [b"\r".as_slice(), b"\n", b"\x03", b"\x1b", b"\x1b[D", b"\t", b"paste", b"@", "é".as_bytes()] {
            let (mut p, _) = warm();
            assert!(!p.input(key, Instant::now()), "{key:?}");
            assert!(p.pending.is_empty());
            assert_eq!(p.confidence, 0);
        }
        for text in ["/model", "!shell", "@file", "email name@example", "****"] {
            let (mut p, _) = warm();
            p.observe(&screen(text, text.len()));
            assert!(!p.active());
            assert!(!p.input(b"x", Instant::now()));
        }
    }

    #[test]
    fn unrecognized_hidden_multiline_styled_and_unicode_fields_are_ignored() {
        let mut examples = Vec::new();
        let mut s = screen("abc", 3);
        s.grid.row_mut(1).fill(Cell::BLANK);
        examples.push(s);
        let mut s = screen("abc", 3);
        s.cursor_visible = false;
        examples.push(s);
        let mut s = screen("abc", 3);
        s.grid.row_mut(11).fill(Cell::BLANK);
        examples.push(s);
        let mut s = screen("abc", 3);
        s.grid.row_mut(10)[3].fga = 123;
        examples.push(s);
        let mut s = screen("abc", 3);
        for cell in &mut s.grid.row_mut(10)[2..5] {
            cell.fga = F_REVERSE << 25;
        }
        examples.push(s);
        examples.push(screen("aé", 2));
        for s in examples {
            let mut p = Predictor::default();
            p.observe(&s);
            assert!(!p.active());
            assert!(!p.input(b"d", Instant::now()));
        }
    }

    #[test]
    fn placeholder_and_start_of_field_are_not_guessed() {
        for text in ["", "Try \"a prompt\"", "abc"] {
            let mut p = Predictor::default();
            p.observe(&screen(text, 0));
            assert!(!p.input(b"x", Instant::now()));
            assert!(p.pending.is_empty());
        }
    }

    #[test]
    fn backspace_and_middle_insert_are_literal_but_do_not_wrap() {
        let (mut p, _) = warm();
        assert!(p.input(b"\x7f", Instant::now()));
        p.observe(&screen("ab", 2));
        assert_eq!(p.confirmed, 1);
        let f = composer(&screen("abcd", 2)).unwrap();
        assert_eq!(f.edit(b"Z").unwrap().text, b"abZcd");
        let f = composer(&screen(&"x".repeat(76), 76)).unwrap();
        assert!(f.edit(b"x").is_none());
    }

    #[test]
    fn paints_only_the_input_and_restores_cursor_and_attributes() {
        let (mut p, mut actual) = warm();
        let mut presented = screen("abc", 3);
        let mut parser = vte::Parser::new();
        parser.advance(&mut actual, b"\x1b[31m");
        parser.advance(&mut presented, b"\x1b[31m");
        p.input(b"d", Instant::now());
        let mut out = Vec::new();
        p.paint(&actual, &mut out);
        parser.advance(&mut presented, &out);
        assert_eq!(presented.grid.row(10)[5].ch, b'd' as u32);
        assert_eq!((presented.grid.cx, presented.grid.cy), (6, 10));
        assert_eq!(presented.attrs(), actual.attrs());
        out.clear();
        p.restore(&actual, &mut out);
        parser.advance(&mut presented, &out);
        assert_eq!(presented.grid.cells, actual.grid.cells);
        assert_eq!((presented.grid.cx, presented.grid.cy), (actual.grid.cx, actual.grid.cy));
        assert_eq!(presented.attrs(), actual.attrs());
    }

    #[test]
    fn shell_return_resize_and_timeout_discard_old_guesses() {
        let (mut p, _) = warm();
        let now = Instant::now();
        p.input(b"d", now);
        assert_eq!(p.deadline(0.08), Some(now + Duration::from_millis(500)));
        assert!(p.deadline(100.0).unwrap() <= now + Duration::from_secs(2));
        p.reset();
        assert!(!p.active());
        assert!(!p.identified);
        assert_eq!(p.deadline(0.08), None);
        assert_eq!(p.discarded, 1);
        let (mut p, _) = warm();
        p.input(b"d", now);
        let mut moved = screen("abc", 3);
        moved.grid.resize(79, 24);
        p.observe(&moved);
        assert!(p.pending.is_empty());
        assert_eq!(p.confidence, 0);
    }

    #[test]
    fn pending_input_has_a_hard_cap() {
        let (mut p, _) = warm();
        let now = Instant::now();
        for _ in 0..MAX_PENDING {
            assert!(p.input(b"d", now));
        }
        assert!(!p.input(b"x", now));
        assert!(p.pending.is_empty());
    }

    #[test]
    fn repeated_states_cannot_ack_an_unseen_edit_cycle() {
        let (mut p, s) = warm();
        assert!(p.input(b"d", Instant::now()));
        assert!(!p.input(b"\x7f", Instant::now()));
        p.observe(&s);
        assert!(p.pending.is_empty());
        assert_eq!(p.confirmed, 0);
        assert_eq!(p.discarded, 1);
        assert_eq!(p.confidence, 0);
    }
}
