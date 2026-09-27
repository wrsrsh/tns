//! Hidden calibration sessions: type history into a second remote fish and
//! record every (state, key) -> diff transition.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::cache::{key_hash, state_hash, Anchor};
use crate::remote::Sink;
use crate::session::{poll_read, Event, Session, Transport};
use crate::shared::Shared;
use crate::term::Grid;

/// Read until output has been silent for `quiet`.  Returns bytes seen, or
/// -1 if the session died.
pub fn wait_quiet(sess: &mut Session, buf: &mut [u8], scratch: &mut Vec<u8>, first: Duration, quiet: Duration) -> i64 {
    let mut got: i64 = 0;
    let deadline = Instant::now() + first;
    loop {
        let timeout = if got > 0 { quiet } else { deadline.saturating_duration_since(Instant::now()) };
        if !poll_read(&[sess.pty.fd], timeout.as_millis() as i32)[0] {
            return got;
        }
        let n = sess.pty.read(buf);
        if n == 0 {
            return -1;
        }
        got += n as i64;
        scratch.clear();
        sess.em.feed(&buf[..n], scratch);
    }
}

pub struct Prober {
    shared: Arc<Shared>,
    idx: usize,
    deadline: Instant,
    sess: Option<Session>,
    anchor: Anchor,
    size: (usize, usize),
    buf: Vec<u8>,
    scratch: Vec<u8>,
    pre: Grid,
}

impl Prober {
    pub fn start(shared: Arc<Shared>, idx: usize, deadline: Instant) {
        let mut p = Prober {
            shared,
            idx,
            deadline,
            sess: None,
            anchor: (0, 0),
            size: (0, 0),
            buf: vec![0; 65536],
            scratch: Vec::with_capacity(65536),
            pre: Grid::new(1, 1),
        };
        std::thread::Builder::new()
            .name(format!("probe{}", idx))
            .stack_size(256 * 1024)
            .spawn(move || p.run())
            .expect("spawn probe thread");
    }

    fn log(&self, msg: &str) {
        self.shared.log(&format!("probe{}: {}", self.idx, msg));
    }

    fn spawn(&mut self) -> bool {
        self.sess = None;
        let (cols, rows) = *self.shared.size.lock().unwrap();
        self.size = (cols, rows);
        let cwd = self.shared.cwd.lock().unwrap().clone();
        let mut sess = match Session::open(&self.shared.host, cols, rows, Transport::Ssh, &self.shared.session_id, &Sink::Osc, cwd.as_deref()) {
            Ok(s) => s,
            Err(e) => {
                self.log(&format!("spawn failed: {}", e));
                return false;
            }
        };
        let t0 = Instant::now();
        let hooks = self.shared.shell.has_hooks();
        while t0.elapsed() < Duration::from_secs(10) {
            let got = wait_quiet(&mut sess, &mut self.buf, &mut self.scratch, Duration::from_secs(2), Duration::from_millis(150));
            if got < 0 {
                break;
            }
            // without hooks, a quiet screen with a cursor is the best prompt signal we have
            if sess.em.events.iter().any(|e| matches!(e, Event::Prompt)) || (!hooks && got > 0 && sess.em.screen.grid.cx > 0) {
                sess.em.events.clear();
                wait_quiet(&mut sess, &mut self.buf, &mut self.scratch, Duration::from_millis(200), Duration::from_millis(150));
                self.anchor = (sess.em.screen.grid.cy, sess.em.screen.grid.cx);
                self.log(&format!("ready anchor={:?} in {:.2}s", self.anchor, t0.elapsed().as_secs_f64()));
                self.sess = Some(sess);
                return true;
            }
        }
        self.log("failed to get a prompt");
        false
    }

    fn run(&mut self) {
        while !self.shared.stopping() {
            if self.idx > 0 && Instant::now() > self.deadline {
                break;
            }
            let cmd = match self.shared.next_probe(true) {
                Some(c) => c,
                None => continue,
            };
            if self.sess.is_none() || self.size != *self.shared.size.lock().unwrap() {
                if !self.spawn() {
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                }
            }
            if !self.probe(&cmd) {
                self.log("session died");
                self.sess = None;
            }
        }
        self.sess = None;
        self.log("done");
    }

    /// Type `cmd` char by char, learning unknown transitions.  False if the
    /// session died.
    fn probe(&mut self, cmd: &str) -> bool {
        let anchor = self.anchor;
        let chars: Vec<char> = cmd.chars().collect();
        let mut sess = self.sess.take().unwrap();
        let mut typed = false;
        let mut learned = 0u64;
        let empty_key = state_hash(&sess.em.screen.grid, anchor);
        let mut i = 0;
        let mut utf8 = [0u8; 4];
        let mut burst = String::new();
        let ok = loop {
            if i >= chars.len() {
                break true;
            }
            let key = state_hash(&sess.em.screen.grid, anchor);
            let ch = chars[i];
            let known = self.shared.cache.lock().unwrap().has(key_hash(key, ch.encode_utf8(&mut utf8).as_bytes()));
            if known {
                // fast-forward through known transitions in one burst
                self.pre.copy_from(&sess.em.screen.grid);
                let mut j = i;
                {
                    let cache = self.shared.cache.lock().unwrap();
                    while j < chars.len() {
                        let k = key_hash(state_hash(&self.pre, anchor), chars[j].encode_utf8(&mut utf8).as_bytes());
                        if !cache.apply(k, &mut self.pre, anchor) {
                            break;
                        }
                        j += 1;
                    }
                }
                burst.clear();
                burst.extend(&chars[i..j]);
                if sess.pty.write(burst.as_bytes()).is_err() {
                    break false;
                }
                typed = true;
                if wait_quiet(&mut sess, &mut self.buf, &mut self.scratch, Duration::from_millis(1500), Duration::from_millis(60)) < 0 {
                    break false;
                }
                i = j;
                continue;
            }
            self.pre.copy_from(&sess.em.screen.grid);
            if sess.pty.write(ch.encode_utf8(&mut utf8).as_bytes()).is_err() {
                break false;
            }
            typed = true;
            let got = wait_quiet(&mut sess, &mut self.buf, &mut self.scratch, Duration::from_millis(1500), Duration::from_millis(60));
            if got <= 0 {
                self.log(&format!("no echo for {:?} in {:?}, respawning", ch, cmd));
                self.spawn();
                return true;
            }
            let k = key_hash(key, ch.encode_utf8(&mut utf8).as_bytes());
            self.shared.cache.lock().unwrap().put_diff(k, &self.pre, &sess.em.screen.grid, anchor);
            learned += 1;
            i += 1;
        };
        if !ok {
            return false;
        }
        if typed {
            if sess.pty.write(b"\x15").is_err() {
                return false;
            }
            if wait_quiet(&mut sess, &mut self.buf, &mut self.scratch, Duration::from_secs(1), Duration::from_millis(60)) < 0 {
                return false;
            }
            if state_hash(&sess.em.screen.grid, anchor) != empty_key {
                self.log(&format!("line did not clear after {:?}, respawning", cmd));
                self.spawn();
                self.shared.with_stats(|s| {
                    s.probed += 1;
                    s.learned_probe += learned;
                });
                return true;
            }
        }
        self.sess = Some(sess);
        self.shared.with_stats(|s| {
            s.probed += 1;
            s.learned_probe += learned;
        });
        self.log(&format!("probed {:?} (+{})", cmd, learned));
        true
    }
}
