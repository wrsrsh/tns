//! Local echo for keys the cache cannot predict (native mosh transport).
//!
//! The idea is mosh's: draw a typed character at the cursor before the
//! server replies, but only once the application has been seen echoing.
//! What makes it sound is the server's echo acknowledgment: when state N of
//! our input is acknowledged, the confirmed screen includes whatever the
//! application did with it, so a guess is then either right or dropped.
//!
//! Guesses are grouped in epochs.  Anything whose effect is not modelled
//! (Enter, control keys, escape sequences), and the application moving the
//! cursor by itself, starts a new epoch.  An epoch's guesses stay hidden
//! until one of them turns up on the real screen with the cursor right
//! behind it, so typing into a prompt that does not echo is not drawn.  A
//! shell prompt reported by the shell's own hook is trusted from its first
//! key once a prompt with the same text has echoed before.
//!
//! What this cannot rule out, as mosh cannot: a program that stops echoing
//! in the middle of a confirmed epoch, with no unmodelled key and no cursor
//! movement in between, has the keys already in flight drawn until its next
//! frame or their acknowledgment (a round trip plus 50 ms) withdraws them.
//!
//! A guess needs to know where the cursor will be.  While earlier keys are
//! still unanswered (typing straight after Enter, say) it is not known; the
//! plain characters typed meanwhile are remembered and, once the screen shows
//! where the application put the first of them, the rest are lined up after
//! it.

use std::collections::VecDeque;

use crate::paint::{cup, sgr, RunWriter};
use crate::term::{Cell, Screen};

const MAX_PENDING: usize = 128;
/// Echoes seen at hook-identified prompts before those prompts are trusted.
const TRUST: u8 = 2;
const SPACE: u32 = ' ' as u32;

/// Characters programs print in place of what is typed.  One of them
/// appearing where it was typed says nothing about echo.
fn mask(ch: u32) -> bool {
    matches!(char::from_u32(ch), Some('*' | '\u{2022}' | '\u{25cf}'))
}

enum Kind {
    Char(u32),
    Backspace,
    Left,
    Right,
}

fn classify(unit: &[u8]) -> Option<Kind> {
    match unit {
        [0x7f] => Some(Kind::Backspace),
        b"\x1b[D" | b"\x1bOD" => Some(Kind::Left),
        b"\x1b[C" | b"\x1bOC" => Some(Kind::Right),
        _ => {
            let mut chars = std::str::from_utf8(unit).ok()?.chars();
            let (c, None) = (chars.next()?, chars.next()) else { return None };
            (c >= ' ' && unicode_width::UnicodeWidthChar::width(c) == Some(1)).then_some(Kind::Char(c as u32))
        }
    }
}

struct Pending {
    num: u64, // our input state that carries this key
    epoch: u64,
    /// Column, the cell we expect there, and the character it replaces.
    cell: Option<(usize, Cell, u32)>,
    /// The confirmed screen already shows the expected cell.
    matched: bool,
    cursor: usize, // column after this key
    /// Paint it: the cache did not predict this key.
    preview: bool,
    anchored: bool,
    counted: bool,
    /// Placed by `resync` from the screen, not from a known cursor.
    lined_up: bool,
}

/// What the caller knows about the key being typed.
pub struct Context {
    /// Start of the command line when a shell prompt is confirmed.
    pub anchor: Option<(usize, usize)>,
    pub preview: bool,
}

/// Write the confirmed cell at column `x` of `row` back to the terminal.
/// The right half of a wide character is drawn by its left half.
fn put_back(w: &mut RunWriter, row: &[Cell], y: usize, x: usize) {
    let x = if row[x].ch == 0 && x > 0 { x - 1 } else { x };
    w.cell(y, x, &row[x]);
}

#[derive(Default)]
pub struct Predictor {
    pending: VecDeque<Pending>,
    row: usize,
    epoch: u64,
    /// Guesses of epochs up to this one are shown.
    shown_epoch: Option<u64>,
    /// Newest input state whose effect on the cursor is unknown; a fresh
    /// guess has to wait for its acknowledgment.
    blocked_until: u64,
    /// An unmodelled key followed these guesses: drop them at the next
    /// screen change instead of drawing them over its result.
    doomed: bool,
    /// Plain characters sent since `unmodelled` and not yet acknowledged, in
    /// order, whether or not they were guessed.
    ahead: VecDeque<(u64, u32)>,
    /// `ahead` is the whole story: no cursor keys in flight, no wrong guess.
    ahead_ok: bool,
    unmodelled: u64, // the newest key whose effect was not modelled at all
    last_num: u64,
    trust: u8,
    /// The epoch of a shell prompt that may be trusted from its first key.
    prompt_epoch: Option<u64>,
    /// The text of the current shell prompt, and of the last one that was
    /// seen echoing.
    prompt_text: Vec<u32>,
    known_prompt: Vec<u32>,
    /// The confirmed cursor while nothing is in flight.
    settled: Option<(usize, usize)>,
    overlay: Vec<(usize, usize)>,
    overlay_cursor: bool,
    pub predicted: u64,
    pub confirmed: u64,
    pub discarded: u64,
}

impl Predictor {
    fn shown(&self, p: &Pending) -> bool {
        self.shown_epoch.is_some_and(|e| p.epoch <= e)
    }

    /// The shell's hook reported a prompt and the screen has settled with
    /// the cursor at `anchor`.  That is trusted to echo, until a key with an
    /// unmodelled effect is typed (history search, a vi command mode, ...),
    /// if a prompt reading the same has echoed before: the report and the
    /// screen travel separately, and after typing ahead the settled screen
    /// can be some other program's prompt.
    pub fn prompt(&mut self, screen: &Screen, anchor: (usize, usize)) {
        let g = &screen.grid;
        self.prompt_text.clear();
        if anchor.0 < g.rows {
            self.prompt_text.extend(g.row(anchor.0)[..anchor.1.min(g.cols)].iter().map(|c| c.ch));
        }
        self.epoch += 1;
        let known = !self.prompt_text.is_empty() && self.prompt_text == self.known_prompt;
        self.prompt_epoch = known.then_some(self.epoch);
    }

    /// Withdraw every guess in flight: one of them was wrong.
    fn withdraw(&mut self, anchored: bool) {
        self.discarded += self.pending.iter().filter(|p| p.counted).count() as u64;
        self.drop_pending(None);
        self.epoch += 1;
        if anchored {
            self.trust = 0;
        }
        self.ahead.clear();
        self.ahead_ok = false;
    }

    /// A key whose effect is not modelled was sent in state `num`.
    pub fn untracked(&mut self, num: u64) {
        self.blocked_until = self.blocked_until.max(num);
        self.unmodelled = num;
        self.last_num = num;
        self.epoch += 1;
        self.doomed = !self.pending.is_empty();
        self.ahead.clear();
        self.ahead_ok = true;
    }

    /// Stop guessing about the keys in flight.  Those `row` already shows
    /// were right; the rest can no longer be told and count as neither.
    fn drop_pending(&mut self, row: Option<&[Cell]>) {
        self.settled = None;
        if let Some(last) = self.pending.back() {
            // Their real echoes are still on the way and will move the cursor.
            self.blocked_until = self.blocked_until.max(last.num);
        }
        for p in self.pending.drain(..).filter(|p| p.counted) {
            self.confirmed += u64::from(match (p.cell, row) {
                (Some((x, cell, _)), Some(row)) => row.get(x).is_some_and(|c| c.ch == cell.ch),
                (None, Some(_)) => true, // a cursor move nothing contradicted
                (_, None) => false,
            });
        }
        self.doomed = false;
    }

    /// Forget everything, including what is painted (the screen is redrawn).
    pub fn reset(&mut self) {
        self.drop_pending(None);
        self.ahead.clear();
        self.ahead_ok = false;
        self.epoch += 1;
        self.overlay.clear();
        self.overlay_cursor = false;
    }

    /// A key typed by the user, sent unchanged in input state `num`.
    /// Returns true if it is drawn locally.
    pub fn input(&mut self, unit: &[u8], num: u64, screen: &Screen, echo_ack: u64, ctx: Context) -> bool {
        let Some(kind) = classify(unit) else {
            self.untracked(num);
            return false;
        };
        let typed = match kind {
            Kind::Char(ch) => Some(ch),
            _ => None,
        };
        match self.guess(kind, num, screen, echo_ack, &ctx) {
            Some(shown) => {
                self.last_num = num;
                match typed {
                    Some(ch) if self.ahead_ok => self.ahead.push_back((num, ch)),
                    Some(_) => {}
                    None => {
                        // A cursor key in flight: "what was typed" no longer
                        // says where things are.
                        self.ahead.clear();
                        self.ahead_ok = false;
                    }
                }
                shown
            }
            None if typed.is_some() && self.waiting(echo_ack) && self.ahead_ok => {
                // Only the cursor is unknown.  Remember the character: the
                // screen will show where this run of typing landed.
                self.ahead.push_back((num, typed.unwrap()));
                self.blocked_until = self.blocked_until.max(num);
                self.last_num = num;
                false
            }
            None => {
                self.untracked(num);
                false
            }
        }
    }

    /// Earlier keys are unanswered, so the confirmed cursor is not yet where
    /// the next key will land.
    fn waiting(&self, echo_ack: u64) -> bool {
        self.doomed || (self.pending.is_empty() && echo_ack < self.blocked_until)
    }

    fn guess(&mut self, kind: Kind, num: u64, screen: &Screen, echo_ack: u64, ctx: &Context) -> Option<bool> {
        let g = &screen.grid;
        if self.waiting(echo_ack) || self.pending.len() >= MAX_PENDING || !screen.cursor_visible {
            return None;
        }
        let col = match self.pending.back() {
            Some(last) => last.cursor,
            None => {
                if g.cx >= g.cols {
                    return None;
                }
                self.row = g.cy;
                g.cx
            }
        };
        if self.row != g.cy {
            return None;
        }
        let row = g.row(self.row);
        // What the user sees in a column: the confirmed cell under our guesses.
        let seen = |x: usize| self.pending.iter().rev().find_map(|p| p.cell.filter(|c| c.0 == x).map(|c| c.1)).unwrap_or(row[x]);
        let start = ctx.anchor.filter(|a| a.0 == self.row).map_or(0, |a| a.1);
        let (cell, cursor) = match kind {
            Kind::Char(ch) => {
                if col + 1 >= g.cols || row[col].ch == 0 {
                    return None; // wrapping is the application's business
                }
                // Continue the word's colour; a new word starts plain.
                let style = if col > start && seen(col - 1).ch != SPACE { seen(col - 1) } else { screen.attrs() };
                (Some((col, Cell { ch, ..style }, row[col].ch)), col + 1)
            }
            // A wide character takes two columns and one key press: its
            // right half (`ch == 0`) is not a place for a guess.
            Kind::Backspace if col > start && seen(col - 1).ch != 0 => {
                // At the end of the text the character disappears; in the
                // middle only the cursor is certain.
                let erased = (seen(col).ch == SPACE).then_some((col - 1, Cell::BLANK, row[col - 1].ch));
                (erased, col - 1)
            }
            Kind::Left if col > start && seen(col - 1).ch != 0 => (None, col - 1),
            Kind::Right if col + 2 < g.cols && seen(col + 1).ch != 0 && (seen(col).ch != SPACE || seen(col + 1).ch != SPACE) => (None, col + 1),
            _ => return None,
        };
        if let Some((x, ..)) = cell {
            // A newer guess for a column replaces the older one.
            for p in self.pending.iter_mut().filter(|p| p.cell.is_some_and(|c| c.0 == x)) {
                p.cell = None;
            }
        }
        let anchored = ctx.anchor.is_some();
        if anchored && self.trust >= TRUST && self.prompt_epoch == Some(self.epoch) {
            self.shown_epoch = Some(self.epoch);
        }
        self.pending.push_back(Pending { num, epoch: self.epoch, cell, matched: false, cursor, preview: ctx.preview, anchored, counted: false, lined_up: false });
        self.settled = None;
        let last = self.pending.back().unwrap();
        Some(last.preview && self.shown(last))
    }

    /// The confirmed screen changed and/or acknowledges input up to `echo_ack`.
    pub fn frame(&mut self, screen: &Screen, echo_ack: u64, changed: bool) {
        while self.ahead.front().is_some_and(|a| a.0 <= echo_ack) {
            self.ahead.pop_front();
        }
        if echo_ack >= self.last_num {
            self.ahead_ok = true; // nothing in flight: a clean start
        }
        if !self.pending.is_empty() {
            self.settle(screen, echo_ack, changed);
        }
        if self.pending.is_empty() && self.ahead_ok && !self.ahead.is_empty() && echo_ack >= self.unmodelled {
            self.resync(screen);
        }
        if self.pending.is_empty() {
            // With nothing of ours in flight, a cursor that moves was moved
            // by the application: whatever echoed before may have ended.
            let cursor = (screen.grid.cy, screen.grid.cx);
            if self.settled.is_some_and(|at| at != cursor) {
                self.epoch += 1;
            }
            self.settled = Some(cursor);
        }
    }

    /// Guess the characters in flight from where the screen shows the run of
    /// typing has got to.  The guesses are as hidden as any others until the
    /// application is seen echoing one of them.
    fn resync(&mut self, screen: &Screen) {
        let g = &screen.grid;
        if !screen.cursor_visible || g.cx >= g.cols {
            return;
        }
        let row = g.row(g.cy);
        // Some of them may already be on screen, just before the cursor.
        let echoed = (0..=self.ahead.len().min(g.cx)).rev().find(|&n| (0..n).all(|i| row[g.cx - n + i].ch == self.ahead[i].1)).unwrap_or(0);
        let rest = self.ahead.len() - echoed;
        if rest == 0 || rest > MAX_PENDING || g.cx + rest + 1 >= g.cols || row[g.cx..g.cx + rest].iter().any(|c| c.ch == 0) {
            return;
        }
        self.row = g.cy;
        for (i, &(num, ch)) in self.ahead.iter().skip(echoed).enumerate() {
            let x = g.cx + i;
            self.pending.push_back(Pending { num, epoch: self.epoch, cell: Some((x, Cell { ch, ..screen.attrs() }, row[x].ch)), matched: false, cursor: x + 1, preview: true, anchored: false, counted: false, lined_up: true });
        }
    }

    fn settle(&mut self, screen: &Screen, echo_ack: u64, changed: bool) {
        let g = &screen.grid;
        if self.row >= g.rows {
            self.drop_pending(None);
            return;
        }
        let row = g.row(self.row);
        if (self.doomed && changed) || g.cy != self.row {
            if !self.doomed {
                self.epoch += 1; // the application left the row by itself
            }
            self.drop_pending(Some(row));
            return;
        }
        // Guesses lined up before the screen had caught up (the prompt was
        // not there yet): none shown, none on screen.  Line them up again.
        let adrift = |p: &Pending| p.lined_up && !p.counted && !p.cell.is_some_and(|(x, cell, _)| row[x].ch == cell.ch);
        if changed && self.pending.iter().all(adrift) {
            self.pending.clear();
            return;
        }
        let mut evidence: Vec<(u64, bool)> = Vec::new();
        for p in self.pending.iter_mut() {
            let Some((x, cell, was)) = p.cell else { continue };
            if !p.matched && cell.ch != SPACE && x < g.cols && row[x].ch == cell.ch {
                p.matched = true;
                // Only a cell that changed into what was typed says anything.
                if was != cell.ch && !mask(cell.ch) {
                    evidence.push((p.epoch, p.anchored));
                }
            }
        }
        // An application echoing our keys fills the guessed cells in order
        // and leaves its cursor behind the last one it has got to (or where
        // cursor keys typed after it lead).  Text that merely coincides with
        // a guess, such as a prompt being printed, does not.
        let mut expect: Vec<usize> = Vec::new();
        let mut passed = None;
        for p in &self.pending {
            match p.cell {
                Some((x, cell, _)) if cell.ch != SPACE => {
                    if !p.matched {
                        passed = (g.cx > x).then_some(p.anchored);
                        break;
                    }
                    expect.clear();
                    expect.push(p.cursor);
                }
                _ => expect.push(p.cursor),
            }
        }
        if let Some(anchored) = passed {
            // The cursor went past a guessed cell and left something else.
            self.wrong(anchored);
            return;
        }
        if expect.contains(&g.cx) {
            for (epoch, anchored) in evidence {
                self.shown_epoch = Some(self.shown_epoch.map_or(epoch, |e| e.max(epoch)));
                if anchored {
                    self.trust = (self.trust + 1).min(TRUST);
                    self.known_prompt.clone_from(&self.prompt_text);
                }
            }
        }
        while self.pending.front().is_some_and(|p| p.num <= echo_ack) {
            let p = self.pending.pop_front().unwrap();
            if p.cell.is_some_and(|(_, cell, _)| cell.ch != SPACE) && !p.matched {
                // Acknowledged, and the application did not echo it.
                self.pending.push_front(p);
                let anchored = self.pending[0].anchored;
                self.wrong(anchored);
                return;
            }
            self.confirmed += u64::from(p.counted);
        }
        if self.pending.is_empty() {
            // The next guess starts from the confirmed cursor, wherever a
            // cursor key really took it (accepting a suggestion, say).
            self.doomed = false;
        }
    }

    /// A guess turned out wrong.  Guesses that were only lined up from the
    /// screen and never shown cost nothing: they are lined up again.
    fn wrong(&mut self, anchored: bool) {
        if self.pending.iter().all(|p| p.lined_up && !p.counted) {
            self.pending.clear();
        } else {
            self.withdraw(anchored);
        }
    }

    /// Repaint the confirmed screen under our guesses.  Call before any real
    /// output is passed through: it is relative to the real cursor.
    pub fn restore(&mut self, screen: &Screen, out: &mut Vec<u8>) {
        if self.overlay.is_empty() && !self.overlay_cursor {
            return;
        }
        let g = &screen.grid;
        if !self.overlay.is_empty() {
            let mut w = RunWriter::new(out);
            for &(y, x) in &self.overlay {
                if y < g.rows && x < g.cols {
                    put_back(&mut w, g.row(y), y, x);
                }
            }
            w.finish();
        }
        cup(out, g.cy, g.cx);
        sgr(out, &screen.attrs());
        self.overlay.clear();
        self.overlay_cursor = false;
    }

    /// Draw the shown guesses over the confirmed screen.  `cursor` is where
    /// the terminal cursor belongs if the last key was not one of ours.
    pub fn paint(&mut self, screen: &Screen, out: &mut Vec<u8>, cursor: (usize, usize)) {
        let g = &screen.grid;
        if self.pending.is_empty() || self.row >= g.rows {
            return;
        }
        let row = g.row(self.row);
        let mut cells: Vec<(usize, Cell)> = Vec::new();
        let mut own_cursor = None;
        let mut newly = 0;
        let shown_epoch = self.shown_epoch;
        for p in self.pending.iter_mut() {
            if !shown_epoch.is_some_and(|e| p.epoch <= e) {
                break; // hidden, and so is everything typed after it
            }
            own_cursor = p.preview.then_some(p.cursor);
            if !p.preview || p.matched {
                continue;
            }
            newly += u64::from(!std::mem::replace(&mut p.counted, true));
            if let Some((x, cell, _)) = p.cell.filter(|c| c.0 < g.cols && row[c.0] != c.1) {
                cells.push((x, cell));
            }
        }
        self.predicted += newly;
        // A guess that was drawn and is no longer made (erased again, or
        // replaced) gives its cell back to the confirmed screen.
        let stale: Vec<(usize, usize)> = self.overlay.iter().copied().filter(|&(y, x)| y != self.row || !cells.iter().any(|c| c.0 == x)).collect();
        if cells.is_empty() && stale.is_empty() && own_cursor.is_none() {
            return;
        }
        if !cells.is_empty() || !stale.is_empty() {
            cells.sort_by_key(|c| c.0);
            let mut w = RunWriter::new(out);
            for &(y, x) in stale.iter().filter(|&&(y, x)| y < g.rows && x < g.cols) {
                put_back(&mut w, g.row(y), y, x);
            }
            for (x, cell) in &cells {
                w.cell(self.row, *x, cell);
            }
            w.finish();
            self.overlay = cells.iter().map(|c| (self.row, c.0)).collect();
        }
        let (y, x) = own_cursor.map_or(cursor, |x| (self.row, x));
        cup(out, y, x);
        sgr(out, &screen.attrs());
        self.overlay_cursor = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(text: &str) -> Screen {
        let mut s = Screen::new(40, 4);
        vte::Parser::new().advance(&mut s, text.as_bytes());
        s
    }

    fn plain() -> Context {
        Context { anchor: None, preview: true }
    }

    fn at_prompt() -> Context {
        Context { anchor: Some((0, 2)), preview: true }
    }

    /// What the terminal shows after `paint`, starting from the confirmed screen.
    fn painted(p: &mut Predictor, confirmed: &Screen) -> (String, usize) {
        let mut out = Vec::new();
        p.paint(confirmed, &mut out, (confirmed.grid.cy, confirmed.grid.cx));
        let mut shown = confirmed.clone();
        vte::Parser::new().advance(&mut shown, &out);
        (shown.grid.row(0).iter().filter_map(Cell::chr).collect::<String>().trim_end().to_string(), shown.grid.cx)
    }

    fn painted_row(p: &mut Predictor, confirmed: &Screen, y: usize) -> String {
        let mut out = Vec::new();
        p.paint(confirmed, &mut out, (confirmed.grid.cy, confirmed.grid.cx));
        let mut shown = confirmed.clone();
        vte::Parser::new().advance(&mut shown, &out);
        shown.grid.row(y).iter().filter_map(Cell::chr).collect::<String>().trim_end_matches(' ').to_string()
    }

    #[test]
    fn first_guess_of_an_epoch_is_hidden_until_the_application_echoes() {
        let mut p = Predictor::default();
        let s = screen("$ ");
        assert!(!p.input(b"a", 1, &s, 0, plain()));
        assert!(!p.input(b"b", 2, &s, 0, plain()));
        assert_eq!(painted(&mut p, &s), ("$".into(), 2));
        // The echo of "a" arrives before its acknowledgment: that is evidence.
        let s = screen("$ a");
        p.frame(&s, 0, true);
        assert_eq!(painted(&mut p, &s), ("$ ab".into(), 4));
        assert!(p.input(b"c", 3, &s, 0, plain()));
        assert_eq!(painted(&mut p, &s), ("$ abc".into(), 5));
        let s = screen("$ abc");
        p.frame(&s, 3, true);
        assert!(p.pending.is_empty());
        assert_eq!((p.predicted, p.confirmed, p.discarded), (2, 2, 0)); // "a" was never drawn
        // The epoch stays confirmed: the next key shows at once.
        assert!(p.input(b"d", 4, &s, 3, plain()));
    }

    #[test]
    fn unechoed_input_is_never_shown() {
        let mut p = Predictor::default();
        let s = screen("Password: ");
        for (num, key) in [b"h", b"u", b"n", b"t", b"e", b"r"].iter().enumerate() {
            assert!(!p.input(*key, num as u64 + 1, &s, num as u64, plain()));
            assert_eq!(painted(&mut p, &s).0, "Password:");
            p.frame(&s, num as u64 + 1, false);
        }
        assert_eq!((p.predicted, p.confirmed, p.discarded), (0, 0, 0));
        assert!(p.pending.is_empty());
    }

    #[test]
    fn a_wrong_guess_is_withdrawn_and_trust_has_to_be_earned_again() {
        let mut p = Predictor::default();
        let s = screen("$ ");
        p.input(b"a", 1, &s, 0, plain());
        let s = screen("$ a");
        p.frame(&s, 1, true);
        assert!(p.input(b"b", 2, &s, 1, plain()));
        assert!(p.input(b"c", 3, &s, 1, plain()));
        assert_eq!(painted(&mut p, &s), ("$ abc".into(), 5));
        // The application turned "b" into something else (a completion, say).
        let s = screen("$ aX");
        let mut out = Vec::new();
        p.restore(&s, &mut out);
        p.frame(&s, 2, true);
        assert!(p.pending.is_empty());
        assert_eq!(painted(&mut p, &s), ("$ aX".into(), 4));
        assert_eq!(p.discarded, 2);
        // "c" is still in flight, so the cursor is not yet known...
        assert!(!p.input(b"d", 4, &s, 2, plain()));
        // ...and once it is, the next guess is hidden until echoed again.
        let s = screen("$ aXcd");
        p.frame(&s, 4, true);
        assert!(!p.input(b"e", 5, &s, 4, plain()));
        let s = screen("$ aXcde");
        p.frame(&s, 4, true);
        assert!(p.input(b"f", 6, &s, 4, plain()));
    }

    #[test]
    fn enter_and_controls_start_a_hidden_epoch_and_drop_stale_guesses() {
        let mut p = Predictor::default();
        let s = screen("$ ");
        p.input(b"l", 1, &s, 0, plain());
        let s = screen("$ l");
        p.frame(&s, 1, true);
        assert!(p.input(b"s", 2, &s, 1, plain()));
        assert!(!p.input(b"\r", 3, &s, 1, plain()));
        // Still drawn until the screen reacts, then gone rather than drawn
        // over whatever Enter produced.
        assert_eq!(painted(&mut p, &s).0, "$ ls");
        assert!(!p.input(b"x", 4, &s, 1, plain()));
        let s = screen("$ ls\r\nfile\r\n$ x");
        p.frame(&s, 2, true);
        assert!(p.pending.is_empty());
        // Until Enter and "x" are acknowledged the cursor could still move.
        assert!(!p.input(b"y", 5, &s, 3, plain()));
        let s = screen("$ ls\r\nfile\r\n$ xy");
        p.frame(&s, 5, true);
        assert!(!p.input(b"z", 6, &s, 5, plain()), "a new epoch starts hidden");
        for key in [b"\x1b".as_slice(), b"\x03", b"\t", b"\x1b[A", b"ab", "日".as_bytes()] {
            assert!(classify(key).is_none(), "{key:?}");
        }
    }

    #[test]
    fn typing_ahead_of_the_prompt_is_lined_up_once_it_appears() {
        let mut p = Predictor::default();
        let s = screen("$ make");
        // Enter, then "git" before the command has even finished.
        p.untracked(1);
        for (num, key) in [(2, b"g"), (3, b"i"), (4, b"t")] {
            assert!(!p.input(key, num, &s, 0, plain()));
        }
        // Enter is acknowledged; the new prompt already shows the "g".
        let s = screen("$ make\r\ndone\r\n$ g");
        p.frame(&s, 1, true);
        assert_eq!(p.pending.len(), 2);
        assert_eq!(painted_row(&mut p, &s, 2), "$ g", "lined up, but not shown before any echo");
        // "i" lands where it was guessed: the application echoes.
        let s = screen("$ make\r\ndone\r\n$ gi");
        p.frame(&s, 2, true);
        assert_eq!(painted_row(&mut p, &s, 2), "$ git");
        assert!(p.input(b"x", 5, &s, 2, plain()));
        assert_eq!(painted_row(&mut p, &s, 2), "$ gitx");
        let s = screen("$ make\r\ndone\r\n$ gitx");
        p.frame(&s, 5, true);
        assert_eq!((p.pending.len(), p.ahead.len(), p.discarded), (0, 0, 0));
    }

    #[test]
    fn typing_ahead_into_a_silent_prompt_stays_hidden() {
        let mut p = Predictor::default();
        let s = screen("$ sudo true");
        p.untracked(1);
        for (num, key) in [(2, b" "), (3, b"p"), (4, b"w")] {
            assert!(!p.input(key, num, &s, 0, plain()));
        }
        // The prompt's own trailing space is not an echo of the typed one.
        let s = screen("$ sudo true\r\nPassword: ");
        p.frame(&s, 1, true);
        assert_eq!(painted_row(&mut p, &s, 1), "Password:");
        p.frame(&s, 4, false);
        assert_eq!(painted_row(&mut p, &s, 1), "Password:");
        assert!(p.pending.is_empty() && p.ahead.is_empty());
        assert_eq!(p.predicted, 0);
        // A cursor key in flight means the typed characters alone do not
        // say where the cursor is: no lining up.
        let mut p = Predictor { shown_epoch: Some(0), ..Predictor::default() };
        let s = screen("$ abc");
        assert!(p.input(b"\x1b[D", 1, &s, 0, plain()));
        assert!(p.input(b"x", 2, &s, 0, plain()));
        p.frame(&screen("$ abc\r\nnoise"), 0, true);
        assert!(p.pending.is_empty() && p.ahead.is_empty());
    }

    #[test]
    fn trusted_shell_prompts_echo_from_the_first_key() {
        let mut p = Predictor::default();
        let s = screen("$ ");
        p.prompt(&s, (0, 2));
        assert!(!p.input(b"a", 1, &s, 0, at_prompt()));
        assert!(!p.input(b"b", 2, &s, 0, at_prompt()));
        let s = screen("$ ab");
        p.frame(&s, 2, true);
        assert_eq!(p.trust, TRUST);
        // A new prompt reading the same, in a new epoch: no waiting this time.
        p.untracked(3);
        let s = screen("$ ab\r\n$ ");
        p.frame(&s, 3, true);
        p.prompt(&s, (1, 2));
        let ctx = || Context { anchor: Some((1, 2)), preview: true };
        assert!(p.input(b"c", 4, &s, 3, ctx()));
        // Outside a hook-identified prompt the same session starts hidden.
        let mut q = Predictor { trust: TRUST, ..Predictor::default() };
        assert!(!q.input(b"c", 1, &s, 0, plain()));
        // So does the prompt itself after a key with an unknown effect, such
        // as Escape into a vi command mode.
        let s = screen("$ ab\r\n$ c");
        p.frame(&s, 4, true);
        p.untracked(5);
        p.frame(&s, 5, false);
        assert!(!p.input(b"d", 6, &s, 5, ctx()));
        assert_eq!(painted_row(&mut p, &s, 1), "$ c");
        // A wrong guess at a prompt revokes the trust; one elsewhere does not.
        p.frame(&s, 6, false);
        assert_eq!(p.trust, 0);
        let mut q = Predictor { trust: TRUST, ..Predictor::default() };
        q.input(b"j", 1, &s, 0, plain());
        q.frame(&s, 1, false);
        assert_eq!(q.trust, TRUST);
    }

    #[test]
    fn a_prompt_that_reads_differently_is_not_trusted() {
        // The shell's report and the screen travel separately: after typing
        // ahead, the screen that settles can be another program's prompt.
        let mut p = Predictor::default();
        let s = screen("$ ");
        p.prompt(&s, (0, 2));
        p.input(b"a", 1, &s, 0, at_prompt());
        p.input(b"b", 2, &s, 0, at_prompt());
        p.frame(&screen("$ ab"), 2, true);
        assert_eq!(p.trust, TRUST);
        p.untracked(3);
        let s = screen("$ ab\r\nPassword: ");
        p.frame(&s, 3, true);
        p.prompt(&s, (1, 10));
        let ctx = || Context { anchor: Some((1, 10)), preview: true };
        assert!(!p.input(b"h", 4, &s, 3, ctx()));
        assert!(!p.input(b"u", 5, &s, 3, ctx()));
        assert_eq!(painted_row(&mut p, &s, 1), "Password:");
        p.frame(&s, 5, false);
        assert_eq!((p.predicted, p.trust), (0, 0));
        // A shell prompt that changed (another directory) earns it again
        // with one echo.
        p.untracked(6);
        let s = screen("~/src $ ");
        p.frame(&s, 6, true);
        p.prompt(&s, (0, 8));
        let ctx = || Context { anchor: Some((0, 8)), preview: true };
        assert!(!p.input(b"l", 7, &s, 6, ctx()));
    }

    #[test]
    fn text_that_coincides_with_a_guess_is_not_an_echo() {
        // Typed before the prompt is printed, at the start of an empty line.
        let mut p = Predictor::default();
        p.untracked(1);
        let s = screen("$ sudo true\r\n");
        p.frame(&s, 1, true);
        for (num, key) in [(2, b"P"), (3, b"a"), (4, b"5"), (5, b"5")] {
            assert!(!p.input(key, num, &s, 1, plain()));
        }
        // "Pa" lands on the prompt's own "Pa", but the cursor is not behind
        // it, and "5" was passed over: the guesses are wrong, not confirmed.
        let s = screen("$ sudo true\r\nPassword: ");
        p.frame(&s, 1, true);
        assert!(p.pending.is_empty() && p.shown_epoch.is_none());
        assert_eq!(painted_row(&mut p, &s, 1), "Password:");
        assert!(!p.input(b"w", 6, &s, 1, plain()));
        // The same typed ahead of Enter's acknowledgment, lined up later.
        let mut p = Predictor::default();
        let s = screen("$ sudo true");
        p.untracked(1);
        for (num, key) in [(2, b"P"), (3, b"a"), (4, b"5")] {
            assert!(!p.input(key, num, &s, 0, plain()));
        }
        p.frame(&screen("$ sudo true\r\n"), 1, true);
        let s = screen("$ sudo true\r\nPassword: ");
        p.frame(&s, 1, true);
        assert_eq!(painted_row(&mut p, &s, 1), "Password:");
        assert!(p.shown_epoch.is_none());
        // A mask character where one was typed is no evidence either.
        let mut p = Predictor::default();
        let s = screen("Password: ");
        for (num, key) in [(1, b"*"), (2, b"s"), (3, b"e")] {
            assert!(!p.input(key, num, &s, 0, plain()));
        }
        let s = screen("Password: *");
        p.frame(&s, 0, true);
        assert_eq!(painted_row(&mut p, &s, 0), "Password: *");
    }

    #[test]
    fn the_application_moving_the_cursor_ends_the_epoch() {
        // A one-key answer that is echoed, then a prompt that is not, with
        // no Enter in between.
        let mut p = Predictor::default();
        let s = screen("Continue? [y/N] ");
        p.frame(&s, 0, false);
        assert!(!p.input(b"y", 1, &s, 0, plain()));
        let s = screen("Continue? [y/N] y");
        p.frame(&s, 1, true);
        assert!(p.shown_epoch.is_some());
        for s in [screen("Continue? [y/N] y\r\nPassword: "), screen("\x1b[4;1HContinue? [y/N] y\r\nPassword: ")] {
            let mut p = Predictor { shown_epoch: Some(0), settled: Some((s.grid.cy, 17)), ..Predictor::default() };
            p.frame(&s, 1, true);
            assert!(!p.input(b"h", 2, &s, 1, plain()));
            assert!(!p.input(b"u", 3, &s, 1, plain()));
        }
        // Typed ahead: the terminal echoed "h" before the program turned
        // echo off and printed its prompt behind it.
        let mut p = Predictor::default();
        p.untracked(1);
        let s = screen("$ sudo true\r\n");
        p.frame(&s, 1, true);
        for (num, key) in [(2, b"h"), (3, b"u"), (4, b"n")] {
            assert!(!p.input(key, num, &s, 1, plain()));
        }
        let s = screen("$ sudo true\r\nh[sudo] password: ");
        p.frame(&s, 1, true);
        assert_eq!(painted_row(&mut p, &s, 1), "h[sudo] password:");
        assert!(p.pending.is_empty());
    }

    #[test]
    fn typing_lined_up_too_early_is_lined_up_again() {
        // Prompt on the bottom row: Enter's acknowledgment shows only the
        // new line, and the prompt then appears on that same row.
        let mut p = Predictor::default();
        let s = screen("\r\n\r\n\r\n$ cd /tmp");
        p.untracked(1);
        for (num, key) in [(2, b"g"), (3, b"i"), (4, b"t")] {
            assert!(!p.input(key, num, &s, 0, plain()));
        }
        p.frame(&screen("\r\n\r\n\r\n$ cd /tmp\r\n"), 1, true);
        assert_eq!(p.pending.len(), 3);
        let s = screen("\r\n\r\n\r\n$ cd /tmp\r\n$ g");
        p.frame(&s, 1, true);
        assert_eq!((p.pending.len(), p.pending[0].cell.unwrap().0), (2, 3));
        let s = screen("\r\n\r\n\r\n$ cd /tmp\r\n$ gi");
        p.frame(&s, 2, true);
        assert_eq!(painted_row(&mut p, &s, 3), "$ git");
        assert_eq!((p.discarded, p.ahead_ok), (0, true));
    }

    #[test]
    fn wide_characters_are_left_to_the_application() {
        let s = screen("$ 日");
        let mut p = Predictor { shown_epoch: Some(0), ..Predictor::default() };
        // One key press erases two columns, and moves over two.
        assert!(!p.input(b"\x7f", 1, &s, 0, at_prompt()));
        assert!(!p.input(b"\x1b[D", 2, &s, 2, at_prompt()));
        let left = screen("$ 日\x1b[1;3H");
        assert!(!p.input(b"\x1b[C", 3, &left, 3, at_prompt()));
        // A guess over the left half is taken back by redrawing the glyph.
        let mut p = Predictor { shown_epoch: Some(0), ..Predictor::default() };
        assert!(p.input(b"x", 4, &left, 3, at_prompt()));
        let mut out = Vec::new();
        p.paint(&left, &mut out, (0, 2));
        let mut shown = left.clone();
        vte::Parser::new().advance(&mut shown, &out);
        assert_eq!(shown.grid.row(0)[2].chr(), Some('x'));
        out.clear();
        p.restore(&left, &mut out);
        vte::Parser::new().advance(&mut shown, &out);
        assert_eq!(shown.grid.cells, left.grid.cells);
    }

    #[test]
    fn backspace_and_arrows_move_the_cursor_within_the_command() {
        let mut p = Predictor { shown_epoch: Some(0), ..Predictor::default() };
        let s = screen("$ abc");
        assert!(p.input(b"\x7f", 1, &s, 0, at_prompt()));
        assert_eq!(painted(&mut p, &s), ("$ ab".into(), 4));
        assert!(p.input(b"\x1b[D", 2, &s, 0, at_prompt()));
        assert!(p.input(b"\x1bOD", 3, &s, 0, at_prompt()));
        assert_eq!(painted(&mut p, &s), ("$ ab".into(), 2));
        // Not past the start of the command, which the anchor marks.
        assert!(!p.input(b"\x1b[D", 4, &s, 0, at_prompt()));
        let mut p = Predictor { shown_epoch: Some(0), ..Predictor::default() };
        assert!(p.input(b"x", 1, &s, 0, at_prompt()));
        // The terminal keeps what was painted: erasing has to take it back.
        let mut out = Vec::new();
        p.paint(&s, &mut out, (0, 5));
        assert!(p.input(b"\x7f", 2, &s, 0, at_prompt()));
        p.paint(&s, &mut out, (0, 5));
        let mut shown = s.clone();
        vte::Parser::new().advance(&mut shown, &out);
        assert_eq!((shown.grid.row(0)[5].chr(), shown.grid.cx), (Some(' '), 5));
        assert!(p.overlay.is_empty());
        // Type-then-erase is acknowledged without the "x" ever being seen.
        p.frame(&s, 2, false);
        assert_eq!((p.pending.len(), p.discarded, p.trust), (0, 0, 0));
        // Right only moves over text that is there.
        let s = screen("$ abc\x1b[1;4H");
        assert!(p.input(b"\x1b[C", 3, &s, 2, at_prompt()));
        assert!(p.input(b"\x1b[C", 4, &s, 2, at_prompt()));
        assert!(!p.input(b"\x1b[C", 5, &s, 2, at_prompt()));
        // Where a cursor key really went is the application's business (it
        // may have accepted a suggestion): typing goes on from there, shown.
        let mut p = Predictor { shown_epoch: Some(0), ..Predictor::default() };
        assert!(p.input(b"\x1b[C", 1, &s, 0, at_prompt()));
        let s = screen("$ abcdef");
        p.frame(&s, 1, true);
        assert!(p.input(b"g", 2, &s, 1, at_prompt()));
        assert_eq!(painted(&mut p, &s), ("$ abcdefg".into(), 9));
    }

    #[test]
    fn cache_predicted_keys_are_tracked_but_not_painted() {
        let mut p = Predictor { shown_epoch: Some(0), ..Predictor::default() };
        let s = screen("$ ");
        assert!(!p.input(b"l", 1, &s, 0, Context { anchor: Some((0, 2)), preview: false }));
        assert!(p.input(b"s", 2, &s, 0, at_prompt()));
        let mut out = Vec::new();
        p.paint(&s, &mut out, (0, 3));
        let mut shown = s.clone();
        vte::Parser::new().advance(&mut shown, &out);
        // Only our own cell is drawn, one column after the cached key's.
        assert_eq!(shown.grid.row(0)[2].chr(), Some(' '));
        assert_eq!(shown.grid.row(0)[3].chr(), Some('s'));
        assert_eq!(shown.grid.cx, 4);
        assert_eq!(p.predicted, 1);
        out.clear();
        p.restore(&s, &mut out);
        vte::Parser::new().advance(&mut shown, &out);
        assert_eq!(shown.grid.cells, s.grid.cells);
        assert_eq!(shown.grid.cx, 2);
    }

    #[test]
    fn output_on_another_row_or_a_resize_drops_guesses_without_blame() {
        let mut p = Predictor { shown_epoch: Some(0), trust: TRUST, ..Predictor::default() };
        let s = screen("$ ");
        assert!(p.input(b"a", 1, &s, 0, at_prompt()));
        p.frame(&screen("$ \r\nnoise"), 0, true);
        assert!(p.pending.is_empty());
        assert_eq!((p.trust, p.discarded), (TRUST, 0));
        // The application wrote elsewhere by itself: typing has to be seen
        // echoing again before it is drawn.
        assert!(!p.input(b"a", 2, &s, 1, at_prompt()));
        p.reset();
        assert!(p.pending.is_empty() && p.overlay.is_empty());
        // No guesses at the last column or under a hidden cursor.
        let mut edge = screen("");
        edge.grid.cx = 39;
        assert!(!p.input(b"a", 3, &edge, 2, plain()));
        assert!(!p.input(b"a", 4, &screen("$ \x1b[?25l"), 4, plain()));
    }
}
