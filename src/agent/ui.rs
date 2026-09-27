//! The local view: a transcript, a status line and an input editor.  All of
//! it is rendered here, so typing, scrolling and answering permission
//! prompts never wait for the network.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::proto::Ev;

#[derive(Debug)]
pub enum Block_ {
    User(String),
    Assistant { text: String, done: bool },
    Thinking { done: bool },
    Tool { id: String, name: String, summary: String, result: Option<(bool, String)> },
    Note(String),
    Error(String),
}

pub struct Pending {
    pub id: String,
    pub title: String,
    pub detail: String,
}

pub struct Transcript {
    pub blocks: Vec<Block_>,
    pub status: String,
    pub running: bool,
    pub pending: Option<Pending>,
    pub scroll_from_bottom: usize, // 0 = follow the end
    pub session: Option<String>,
    pub agent: String,
    pub host: String,
}

impl Transcript {
    pub fn new(agent: &str, host: &str) -> Transcript {
        Transcript {
            blocks: Vec::new(),
            status: "connecting".into(),
            running: false,
            pending: None,
            scroll_from_bottom: 0,
            session: None,
            agent: agent.to_string(),
            host: host.to_string(),
        }
    }

    pub fn push_user(&mut self, text: &str) {
        self.blocks.push(Block_::User(text.to_string()));
        self.running = true;
        self.status = "thinking".into();
        self.scroll_from_bottom = 0;
    }

    pub fn apply(&mut self, ev: Ev) {
        match ev {
            Ev::SessionId(id) => self.session = Some(id),
            Ev::Status(s) => self.status = s,
            Ev::Thinking => {
                if !matches!(self.blocks.last(), Some(Block_::Thinking { done: false })) {
                    self.finish_open();
                    self.blocks.push(Block_::Thinking { done: false });
                }
                self.status = "thinking".into();
            }
            Ev::TextDelta(d) => {
                if let Some(Block_::Assistant { text, done: false }) = self.blocks.last_mut() {
                    text.push_str(&d);
                } else {
                    self.finish_open();
                    self.blocks.push(Block_::Assistant { text: d, done: false });
                }
                self.status = "streaming".into();
            }
            Ev::TextDone => self.finish_open(),
            Ev::ToolStart { id, name, summary } => {
                self.finish_open();
                self.blocks.push(Block_::Tool { id, name, summary, result: None });
                self.status = "running tool".into();
            }
            Ev::ToolDone { id, ok, summary } => {
                if let Some(Block_::Tool { result, .. }) = self.blocks.iter_mut().rev().find(|b| matches!(b, Block_::Tool { id: i, .. } if *i == id)) {
                    *result = Some((ok, summary));
                }
            }
            Ev::Permission { id, title, detail } => {
                self.pending = Some(Pending { id, title, detail });
                self.status = "waiting for you".into();
            }
            Ev::TurnDone(note) => {
                self.finish_open();
                self.running = false;
                self.status = "idle".into();
                if let Some(n) = note {
                    self.blocks.push(Block_::Note(n));
                }
            }
            Ev::Error(e) => {
                self.finish_open();
                self.blocks.push(Block_::Error(e));
            }
        }
    }

    fn finish_open(&mut self) {
        match self.blocks.last_mut() {
            Some(Block_::Assistant { done, .. }) => *done = true,
            Some(Block_::Thinking { done }) => *done = true,
            _ => {}
        }
    }

    /// Wrapped lines for a given width.
    fn lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut out: Vec<Line> = Vec::new();
        let w = width.max(10);
        let dim = Style::default().fg(Color::DarkGray);
        for b in &self.blocks {
            match b {
                Block_::User(t) => {
                    out.push(Line::default());
                    for (i, l) in wrap(t, w - 2).into_iter().enumerate() {
                        let prefix = if i == 0 { "› " } else { "  " };
                        out.push(Line::from(vec![Span::styled(prefix, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)), Span::styled(l, Style::default().add_modifier(Modifier::BOLD))]));
                    }
                }
                Block_::Assistant { text, done } => {
                    out.push(Line::default());
                    let mut ls = wrap(text, w);
                    if !done {
                        if let Some(last) = ls.last_mut() {
                            last.push('▍');
                        } else {
                            ls.push("▍".into());
                        }
                    }
                    for l in ls {
                        out.push(Line::from(l));
                    }
                }
                Block_::Thinking { done } => {
                    out.push(Line::from(Span::styled(if *done { "· thought" } else { "· thinking…" }, dim)));
                }
                Block_::Tool { name, summary, result, .. } => {
                    let (mark, style) = match result {
                        None => ("⋯", Style::default().fg(Color::Yellow)),
                        Some((true, _)) => ("✓", Style::default().fg(Color::Green)),
                        Some((false, _)) => ("✗", Style::default().fg(Color::Red)),
                    };
                    let head = format!("{} {}", name, summary);
                    for (i, l) in wrap(&head, w - 2).into_iter().enumerate() {
                        let m = if i == 0 { format!("{} ", mark) } else { "  ".into() };
                        out.push(Line::from(vec![Span::styled(m, style), Span::styled(l, dim)]));
                    }
                    if let Some((_, s)) = result {
                        if !s.trim().is_empty() {
                            for l in wrap(s, w - 4).into_iter().take(3) {
                                out.push(Line::from(vec![Span::raw("    "), Span::styled(l, dim)]));
                            }
                        }
                    }
                }
                Block_::Note(n) => out.push(Line::from(Span::styled(format!("  {}", n), dim))),
                Block_::Error(e) => {
                    for l in wrap(e, w - 2) {
                        out.push(Line::from(vec![Span::styled("! ", Style::default().fg(Color::Red)), Span::styled(l, Style::default().fg(Color::Red))]));
                    }
                }
            }
        }
        out
    }
}

/// Greedy word wrap by display width, keeping explicit newlines.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(4);
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        let mut lw = 0;
        for word in para.split(' ') {
            let ww = word.width();
            if ww > width {
                // hard-break a very long token
                if !line.is_empty() {
                    out.push(std::mem::take(&mut line));
                }
                let mut cur = String::new();
                let mut cw = 0;
                for c in word.chars() {
                    let w = c.width().unwrap_or(0);
                    if cw + w > width {
                        out.push(std::mem::take(&mut cur));
                        cw = 0;
                    }
                    cur.push(c);
                    cw += w;
                }
                line = cur;
                lw = cw;
                continue;
            }
            let sep = if line.is_empty() { 0 } else { 1 };
            if lw + sep + ww > width {
                out.push(std::mem::take(&mut line));
                lw = 0;
            }
            if !line.is_empty() {
                line.push(' ');
                lw += 1;
            }
            line.push_str(word);
            lw += ww;
        }
        out.push(line);
    }
    out
}

/// A small multi-line editor.
pub struct Editor {
    pub text: Vec<char>,
    pub cursor: usize,
    pub history: Vec<String>,
    hist_pos: Option<usize>,
    stash: Vec<char>,
}

impl Editor {
    pub fn new() -> Editor {
        Editor { text: Vec::new(), cursor: 0, history: Vec::new(), hist_pos: None, stash: Vec::new() }
    }
    pub fn is_empty(&self) -> bool {
        self.text.iter().all(|c| c.is_whitespace())
    }
    pub fn take(&mut self) -> String {
        let s: String = self.text.iter().collect();
        self.text.clear();
        self.cursor = 0;
        self.hist_pos = None;
        let t = s.trim().to_string();
        if !t.is_empty() && self.history.last() != Some(&t) {
            self.history.push(t.clone());
        }
        t
    }
    pub fn insert(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += 1;
    }
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.text.remove(self.cursor);
        }
    }
    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            self.text.remove(self.cursor);
        }
    }
    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }
    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.text.len());
    }
    pub fn home(&mut self) {
        while self.cursor > 0 && self.text[self.cursor - 1] != '\n' {
            self.cursor -= 1;
        }
    }
    pub fn end(&mut self) {
        while self.cursor < self.text.len() && self.text[self.cursor] != '\n' {
            self.cursor += 1;
        }
    }
    pub fn delete_word(&mut self) {
        let mut i = self.cursor;
        while i > 0 && self.text[i - 1] == ' ' {
            i -= 1;
        }
        while i > 0 && self.text[i - 1] != ' ' && self.text[i - 1] != '\n' {
            i -= 1;
        }
        self.text.drain(i..self.cursor);
        self.cursor = i;
    }
    pub fn kill_line(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }
    pub fn multiline(&self) -> bool {
        self.text.contains(&'\n')
    }
    pub fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let pos = match self.hist_pos {
            None => {
                self.stash = self.text.clone();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(p) => p - 1,
        };
        self.hist_pos = Some(pos);
        self.text = self.history[pos].chars().collect();
        self.cursor = self.text.len();
    }
    pub fn history_down(&mut self) {
        match self.hist_pos {
            None => {}
            Some(p) if p + 1 < self.history.len() => {
                self.hist_pos = Some(p + 1);
                self.text = self.history[p + 1].chars().collect();
                self.cursor = self.text.len();
            }
            Some(_) => {
                self.hist_pos = None;
                self.text = std::mem::take(&mut self.stash);
                self.cursor = self.text.len();
            }
        }
    }
    /// (lines, cursor row, cursor col) for a given width.
    fn layout(&self, width: usize) -> (Vec<String>, usize, usize) {
        let width = width.max(4);
        let mut lines = vec![String::new()];
        let mut col = 0usize;
        let (mut cy, mut cx) = (0, 0);
        for (i, &c) in self.text.iter().enumerate() {
            if i == self.cursor {
                cy = lines.len() - 1;
                cx = col;
            }
            if c == '\n' {
                lines.push(String::new());
                col = 0;
                continue;
            }
            let w = c.width().unwrap_or(1);
            if col + w > width {
                lines.push(String::new());
                col = 0;
            }
            lines.last_mut().unwrap().push(c);
            col += w;
        }
        if self.cursor == self.text.len() {
            cy = lines.len() - 1;
            cx = col;
        }
        (lines, cy, cx)
    }
}

pub struct View {
    pub transcript: Transcript,
    pub editor: Editor,
    pub quit_armed: bool,
    pub toast: Option<String>,
}

impl View {
    pub fn new(agent: &str, host: &str) -> View {
        View { transcript: Transcript::new(agent, host), editor: Editor::new(), quit_armed: false, toast: None }
    }

    pub fn scroll(&mut self, delta: i64) {
        let s = self.transcript.scroll_from_bottom as i64 + delta;
        self.transcript.scroll_from_bottom = s.max(0) as usize;
    }

    pub fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        let width = area.width as usize;
        let (elines, cy, cx) = self.editor.layout(width.saturating_sub(4));
        let input_h = (elines.len().min(6) + 2) as u16;
        let [top, status, input] = Layout::vertical([Constraint::Min(1), Constraint::Length(1), Constraint::Length(input_h)]).areas(area);

        // transcript
        let lines = self.transcript.lines(width.saturating_sub(1));
        let h = top.height as usize;
        let max_from_bottom = lines.len().saturating_sub(h);
        if self.transcript.scroll_from_bottom > max_from_bottom {
            self.transcript.scroll_from_bottom = max_from_bottom;
        }
        let end = lines.len() - self.transcript.scroll_from_bottom;
        let start = end.saturating_sub(h);
        let visible: Vec<Line> = lines[start..end].to_vec();
        f.render_widget(Paragraph::new(visible), top);

        // status line
        let t = &self.transcript;
        let mut left = format!(" {} @ {} · {}", t.agent, t.host, t.status);
        if let Some(s) = &t.session {
            left.push_str(&format!(" · {}", &s[..s.len().min(8)]));
        }
        let right = if self.transcript.scroll_from_bottom > 0 {
            format!("↑{} lines  End: bottom ", self.transcript.scroll_from_bottom)
        } else if t.pending.is_some() {
            "y: allow  a: always  n: deny ".to_string()
        } else if t.running {
            "Esc: interrupt  ctrl-c: quit ".to_string()
        } else {
            "Enter: send  alt-Enter: newline  ctrl-c: quit ".to_string()
        };
        let pad = width.saturating_sub(left.width() + right.width());
        let line = Line::from(vec![
            Span::styled(left, Style::default().fg(Color::Black).bg(Color::Cyan)),
            Span::styled(" ".repeat(pad), Style::default().bg(Color::Cyan)),
            Span::styled(right, Style::default().fg(Color::Black).bg(Color::Cyan)),
        ]);
        f.render_widget(Paragraph::new(line), status);

        // input / prompt
        if let Some(p) = &t.pending {
            let text = vec![Line::from(vec![
                Span::styled(format!(" allow {}? ", p.title), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::raw(p.detail.clone()),
            ])];
            let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Yellow)).title(" permission ");
            f.render_widget(Paragraph::new(text).block(block), input);
            return;
        }
        let title = match &self.toast {
            Some(m) => format!(" {} ", m),
            None => String::new(),
        };
        let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)).title(title);
        let inner = Rect { x: input.x + 2, y: input.y + 1, width: input.width.saturating_sub(4), height: input.height.saturating_sub(2) };
        f.render_widget(block, input);
        let first = cy.saturating_sub(inner.height as usize - 1);
        let shown: Vec<Line> = elines.iter().skip(first).take(inner.height as usize).map(|l| Line::from(l.clone())).collect();
        f.render_widget(Paragraph::new(shown), inner);
        f.set_cursor_position((inner.x + cx as u16, inner.y + (cy - first) as u16));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_words_and_long_tokens() {
        assert_eq!(wrap("aaa bbb ccc", 7), ["aaa bbb", "ccc"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("a\n\nb", 10), ["a", "", "b"]);
    }

    #[test]
    fn editor_basics() {
        let mut e = Editor::new();
        for c in "hello world".chars() {
            e.insert(c);
        }
        e.delete_word();
        assert_eq!(e.text.iter().collect::<String>(), "hello ");
        e.home();
        e.delete();
        assert_eq!(e.take(), "ello");
        e.history_up();
        assert_eq!(e.text.iter().collect::<String>(), "ello");
    }

    #[test]
    fn transcript_streams_into_one_block() {
        let mut t = Transcript::new("claude", "h");
        t.push_user("hi");
        t.apply(Ev::TextDelta("a".into()));
        t.apply(Ev::TextDelta("b".into()));
        t.apply(Ev::TextDone);
        t.apply(Ev::ToolStart { id: "1".into(), name: "Bash".into(), summary: "ls".into() });
        t.apply(Ev::ToolDone { id: "1".into(), ok: true, summary: "x".into() });
        t.apply(Ev::TurnDone(None));
        assert!(matches!(&t.blocks[1], Block_::Assistant { text, done: true } if text == "ab"));
        assert!(matches!(&t.blocks[2], Block_::Tool { result: Some((true, _)), .. }));
        assert!(!t.running);
    }
}
