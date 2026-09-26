//! The (state, key) -> screen diff cache.
//!
//! States are hashed to 128 bits; diffs are stored run-length encoded in two
//! shared arenas (runs + UTF-8 text) so an entry costs ~16 bytes of map slot
//! plus ~16 bytes per run of changed cells.  Persisted as a compact binary
//! file at ~/.cache/tns/<host>.bin.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use xxhash_rust::xxh3::Xxh3;

use crate::term::{Cell, Grid};

pub type Key = u128;
pub type Anchor = (usize, usize); // (row, col)

const MAGIC: &[u8; 8] = b"TNSCACH2";

#[derive(Clone, Copy)]
#[repr(C)]
struct Run {
    dy: i16,
    dx: i16,
    len: u16,
    fga: u32,
    bg: u32,
}

#[derive(Clone, Copy)]
struct Entry {
    run_off: u32,
    run_len: u32,
    text_off: u32,
    cur_dy: i16,
    cur_dx: i16,
}

pub struct Cache {
    path: PathBuf,
    map: HashMap<Key, Entry>,
    runs: Vec<Run>,
    text: Vec<u8>,
    dirty: bool,
    waste: usize,
}

/// Hash of what the user sees from the anchor down: columns, cursor offset and
/// the text of each row (anchor row from the anchor column, cut at the first
/// gap of 4+ spaces so the right prompt is ignored; trailing blank rows dropped).
pub fn state_hash(g: &Grid, anchor: Anchor) -> Key {
    let (ay, ax) = anchor;
    let mut h = Xxh3::new();
    h.update(&(g.cols as u32).to_le_bytes());
    h.update(&((g.cy as i64 - ay as i64) as i32).to_le_bytes());
    h.update(&((g.cx as i64 - ax as i64) as i32).to_le_bytes());
    let mut pending_empty: u32 = 0;
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    for y in ay..g.rows {
        let row = g.row(y);
        let (start, mut end) = if y == ay { (ax.min(g.cols), g.cols) } else { (0, g.cols) };
        if y == ay {
            // cut at the first run of 4+ spaces
            let mut spaces = 0;
            for x in start..g.cols {
                if row[x].ch == ' ' as u32 {
                    spaces += 1;
                    if spaces == 4 {
                        end = x + 1 - 4;
                        break;
                    }
                } else {
                    spaces = 0;
                }
            }
        }
        // rstrip
        while end > start && row[end - 1].ch == ' ' as u32 {
            end -= 1;
        }
        if end == start {
            pending_empty += 1;
            continue;
        }
        for _ in 0..pending_empty {
            h.update(&[0xff]);
        }
        pending_empty = 0;
        buf.clear();
        for c in &row[start..end] {
            if c.ch != 0 {
                buf.extend_from_slice(&c.ch.to_le_bytes());
            }
        }
        h.update(&buf);
        h.update(&[0xff]);
    }
    h.digest128()
}

/// Cache key for pressing `unit` in state `state`.
pub fn key_hash(state: Key, unit: &[u8]) -> Key {
    let mut h = Xxh3::new();
    h.update(&state.to_le_bytes());
    h.update(unit);
    h.digest128()
}

impl Cache {
    pub fn load(path: &Path) -> Cache {
        let mut c = Cache {
            path: path.to_path_buf(),
            map: HashMap::new(),
            runs: Vec::new(),
            text: Vec::new(),
            dirty: false,
            waste: 0,
        };
        if let Ok(data) = fs::read(path) {
            if c.parse(&data).is_none() {
                c.map.clear();
                c.runs.clear();
                c.text.clear();
            }
        }
        c
    }

    fn parse(&mut self, d: &[u8]) -> Option<()> {
        let mut p = 0;
        let take = |p: &mut usize, n: usize| -> Option<&[u8]> {
            let s = d.get(*p..*p + n)?;
            *p += n;
            Some(s)
        };
        if take(&mut p, 8)? != MAGIC {
            return None;
        }
        let u32_at = |p: &mut usize| -> Option<u32> { Some(u32::from_le_bytes(take(p, 4)?.try_into().ok()?)) };
        let n = u32_at(&mut p)? as usize;
        let nr = u32_at(&mut p)? as usize;
        let nt = u32_at(&mut p)? as usize;
        self.map.reserve(n);
        for _ in 0..n {
            let key = u128::from_le_bytes(take(&mut p, 16)?.try_into().ok()?);
            let run_off = u32_at(&mut p)?;
            let run_len = u32_at(&mut p)?;
            let text_off = u32_at(&mut p)?;
            let cur_dy = i16::from_le_bytes(take(&mut p, 2)?.try_into().ok()?);
            let cur_dx = i16::from_le_bytes(take(&mut p, 2)?.try_into().ok()?);
            self.map.insert(key, Entry { run_off, run_len, text_off, cur_dy, cur_dx });
        }
        self.runs.reserve(nr);
        for _ in 0..nr {
            let b = take(&mut p, 14)?;
            self.runs.push(Run {
                dy: i16::from_le_bytes([b[0], b[1]]),
                dx: i16::from_le_bytes([b[2], b[3]]),
                len: u16::from_le_bytes([b[4], b[5]]),
                fga: u32::from_le_bytes([b[6], b[7], b[8], b[9]]),
                bg: u32::from_le_bytes([b[10], b[11], b[12], b[13]]),
            });
        }
        self.text = take(&mut p, nt)?.to_vec();
        for e in self.map.values() {
            if (e.run_off + e.run_len) as usize > self.runs.len() || e.text_off as usize > self.text.len() {
                return None;
            }
        }
        Some(())
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn has(&self, key: Key) -> bool {
        self.map.contains_key(&key)
    }

    /// Approximate resident bytes of the cache.
    pub fn mem_bytes(&self) -> usize {
        self.map.capacity() * (16 + 16 + 1) + self.runs.capacity() * 16 + self.text.capacity()
    }

    /// Record the diff `pre -> post` (rows from the anchor down) under `key`.
    pub fn put_diff(&mut self, key: Key, pre: &Grid, post: &Grid, anchor: Anchor) {
        let (ay, ax) = anchor;
        let run_off = self.runs.len() as u32;
        let text_off = self.text.len() as u32;
        let cols = post.cols;
        let mut utf8 = [0u8; 4];
        for y in ay..post.rows {
            let prow = if y < pre.rows && pre.cols == cols { Some(pre.row(y)) } else { None };
            let row = post.row(y);
            let mut x = 0;
            while x < cols {
                let changed = prow.map_or(true, |p| p[x] != row[x]);
                if !changed {
                    x += 1;
                    continue;
                }
                let attr = (row[x].fga, row[x].bg);
                let start = x;
                while x < cols
                    && x - start < u16::MAX as usize
                    && prow.map_or(true, |p| p[x] != row[x])
                    && (row[x].fga, row[x].bg) == attr
                {
                    match row[x].chr() {
                        Some(c) => self.text.extend_from_slice(c.encode_utf8(&mut utf8).as_bytes()),
                        None => self.text.push(0),
                    }
                    x += 1;
                }
                self.runs.push(Run {
                    dy: (y as i64 - ay as i64) as i16,
                    dx: (start as i64 - ax as i64) as i16,
                    len: (x - start) as u16,
                    fga: attr.0,
                    bg: attr.1,
                });
            }
        }
        let e = Entry {
            run_off,
            run_len: self.runs.len() as u32 - run_off,
            text_off,
            cur_dy: (post.cy as i64 - ay as i64) as i16,
            cur_dx: (post.cx as i64 - ax as i64) as i16,
        };
        if let Some(old) = self.map.insert(key, e) {
            self.waste += old.run_len as usize;
        }
        self.dirty = true;
    }

    /// Apply the diff stored under `key` to `g`.  Returns false if unknown.
    pub fn apply(&self, key: Key, g: &mut Grid, anchor: Anchor) -> bool {
        let e = match self.map.get(&key) {
            Some(e) => *e,
            None => return false,
        };
        let (ay, ax) = anchor;
        let mut t = e.text_off as usize;
        for r in &self.runs[e.run_off as usize..(e.run_off + e.run_len) as usize] {
            let y = ay as i64 + r.dy as i64;
            let mut x = ax as i64 + r.dx as i64;
            for _ in 0..r.len {
                let (ch, adv) = decode_utf8(&self.text[t..]);
                t += adv;
                if y >= 0 && (y as usize) < g.rows && x >= 0 && (x as usize) < g.cols {
                    g.row_mut(y as usize)[x as usize] = Cell { ch, fga: r.fga, bg: r.bg };
                }
                x += 1;
            }
        }
        g.cy = (ay as i64 + e.cur_dy as i64).clamp(0, g.rows as i64 - 1) as usize;
        g.cx = (ax as i64 + e.cur_dx as i64).clamp(0, g.cols as i64) as usize;
        true
    }

    fn compact(&mut self) {
        if self.waste == 0 {
            return;
        }
        let mut runs = Vec::with_capacity(self.runs.len() - self.waste);
        let mut text = Vec::with_capacity(self.text.len());
        for e in self.map.values_mut() {
            let new_off = runs.len() as u32;
            let new_toff = text.len() as u32;
            let mut t = e.text_off as usize;
            for r in &self.runs[e.run_off as usize..(e.run_off + e.run_len) as usize] {
                for _ in 0..r.len {
                    let (_, adv) = decode_utf8(&self.text[t..]);
                    text.extend_from_slice(&self.text[t..t + adv]);
                    t += adv;
                }
                runs.push(*r);
            }
            e.run_off = new_off;
            e.text_off = new_toff;
        }
        self.runs = runs;
        self.text = text;
        self.waste = 0;
    }

    pub fn save(&mut self) -> io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        self.compact();
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut buf = Vec::with_capacity(32 + self.map.len() * 32 + self.runs.len() * 14 + self.text.len());
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&(self.map.len() as u32).to_le_bytes());
        buf.extend_from_slice(&(self.runs.len() as u32).to_le_bytes());
        buf.extend_from_slice(&(self.text.len() as u32).to_le_bytes());
        for (k, e) in &self.map {
            buf.extend_from_slice(&k.to_le_bytes());
            buf.extend_from_slice(&e.run_off.to_le_bytes());
            buf.extend_from_slice(&e.run_len.to_le_bytes());
            buf.extend_from_slice(&e.text_off.to_le_bytes());
            buf.extend_from_slice(&e.cur_dy.to_le_bytes());
            buf.extend_from_slice(&e.cur_dx.to_le_bytes());
        }
        for r in &self.runs {
            buf.extend_from_slice(&r.dy.to_le_bytes());
            buf.extend_from_slice(&r.dx.to_le_bytes());
            buf.extend_from_slice(&r.len.to_le_bytes());
            buf.extend_from_slice(&r.fga.to_le_bytes());
            buf.extend_from_slice(&r.bg.to_le_bytes());
        }
        buf.extend_from_slice(&self.text);
        let tmp = self.path.with_extension("bin.tmp");
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&buf)?;
        }
        fs::rename(&tmp, &self.path)?;
        self.dirty = false;
        Ok(())
    }
}

/// Decode one UTF-8 scalar (0 byte = wide-char continuation); returns (code, bytes used).
#[inline]
fn decode_utf8(b: &[u8]) -> (u32, usize) {
    let b0 = b[0] as u32;
    if b0 < 0x80 {
        (b0, 1)
    } else if b0 < 0xe0 {
        ((b0 & 0x1f) << 6 | (b[1] as u32 & 0x3f), 2)
    } else if b0 < 0xf0 {
        ((b0 & 0x0f) << 12 | (b[1] as u32 & 0x3f) << 6 | (b[2] as u32 & 0x3f), 3)
    } else {
        ((b0 & 0x07) << 18 | (b[1] as u32 & 0x3f) << 12 | (b[2] as u32 & 0x3f) << 6 | (b[3] as u32 & 0x3f), 4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::Screen;
    use vte::Parser;

    fn screen(cols: usize, rows: usize, bytes: &[u8]) -> Grid {
        let mut s = Screen::new(cols, rows);
        let mut p = Parser::new();
        p.advance(&mut s, bytes);
        s.grid
    }

    #[test]
    fn diff_roundtrip() {
        let dir = std::env::temp_dir().join(format!("tns-test-{}", std::process::id()));
        let path = dir.join("c.bin");
        let pre = screen(10, 3, b"$ \x1b[1;3H");
        let post = screen(10, 3, "$ \x1b[31mé日\x1b[0mx \x1b[1;8H".as_bytes());
        let anchor = (0, 2);
        let mut c = Cache::load(&path);
        let k = key_hash(state_hash(&pre, anchor), b"e");
        c.put_diff(k, &pre, &post, anchor);
        let mut g = pre.clone();
        assert!(c.apply(k, &mut g, anchor));
        assert_eq!(g.cells, post.cells);
        assert_eq!((g.cx, g.cy), (post.cx, post.cy));
        c.save().unwrap();
        let c2 = Cache::load(&path);
        let mut g2 = pre.clone();
        assert!(c2.apply(k, &mut g2, anchor));
        assert_eq!(g2.cells, post.cells);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_hash_ignores_right_prompt_and_prompt_prefix() {
        let a = screen(40, 2, b"user@h $ ls -l        12:00\x1b[1;12H");
        let b = screen(40, 2, b"root@x $ ls -l        13:37\x1b[1;12H");
        assert_eq!(state_hash(&a, (0, 9)), state_hash(&b, (0, 9)));
        let c = screen(40, 2, b"user@h $ ls -la       12:00\x1b[1;12H");
        assert_ne!(state_hash(&a, (0, 9)), state_hash(&c, (0, 9)));
    }
}
