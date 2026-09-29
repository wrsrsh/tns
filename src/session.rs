//! One ssh pty to the remote fish, with an emulator tracking its screen.

use std::collections::VecDeque;
use std::ffi::CString;
use std::io::{self, BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::remote::{launch_argv, Sink};
use crate::term::Screen;

/// Emits OSC marks (cwd, prompt, executed command) without a single
/// backslash, so the snippet survives both a fish and a POSIX login shell
/// on the remote.
pub fn fish_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

pub enum Event {
    Prompt,
    Exec(String),
    Cwd(String),
}

/// Screen model + the byte-level filtering that the passthrough needs.
pub struct Emulator {
    pub screen: Screen,
    parser: vte::Parser,
    carry: Vec<u8>,
    pub cwd: Option<String>,
    pub events: VecDeque<Event>,
}

/// Index at which an incomplete trailing escape sequence starts, or len.
fn split_tail(data: &[u8]) -> usize {
    let mut last_escape = None;
    let mut pos = 0;
    while let Some(offset) = data[pos..].iter().position(|&b| b == 0x1b) {
        let i = pos + offset;
        last_escape = Some(i);
        pos = i + 1;
        if data.get(pos) != Some(&b']') {
            continue;
        }
        // Walk OSCs from their opening ESC: the ESC in a split ST belongs to
        // this OSC, not to a new trailing escape sequence.
        let end = match data[i + 2..].iter().position(|&b| b == 0x07 || b == 0x1b) {
            Some(offset) => i + 2 + offset,
            None => return i,
        };
        if data[end] == 0x07 {
            pos = end + 1;
        } else {
            match data.get(end + 1) {
                Some(b'\\') => pos = end + 2,
                None => return i,
                // As in parse_osc, another escape cancels this OSC. Resume at
                // that escape so incomplete CSI/charset sequences stay held.
                Some(_) => {
                    pos = end;
                    continue;
                }
            }
        }
        last_escape = None;
    }
    let i = match last_escape {
        Some(i) => i,
        None => return data.len(),
    };
    let tail = &data[i..];
    if tail.len() == 1 {
        return i;
    }
    match tail[1] {
        b'[' => {
            if tail[2..].iter().any(|&b| (0x40..=0x7e).contains(&b)) {
                data.len()
            } else {
                i
            }
        }
        b'(' | b')' | b'#' | b'%' => {
            if tail.len() >= 3 {
                data.len()
            } else {
                i
            }
        }
        _ => data.len(),
    }
}

pub fn url_unquote(s: &[u8]) -> String {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'%' && i + 2 < s.len() {
            let h = std::str::from_utf8(&s[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(b) = h {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(s[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Find `ESC ] code ; payload (BEL | ESC \)` starting at `i`.
/// Returns (code, payload range, end index).
fn parse_osc(data: &[u8], i: usize) -> Option<(u32, std::ops::Range<usize>, usize)> {
    if data.get(i) != Some(&0x1b) || data.get(i + 1) != Some(&b']') {
        return None;
    }
    let mut j = i + 2;
    let mut code: u32 = 0;
    let mut digits = 0;
    while let Some(&b) = data.get(j) {
        if b.is_ascii_digit() {
            code = code.saturating_mul(10).saturating_add((b - b'0') as u32);
            digits += 1;
            j += 1;
        } else {
            break;
        }
    }
    if digits == 0 || data.get(j) != Some(&b';') {
        return None;
    }
    let start = j + 1;
    let mut k = start;
    while let Some(&b) = data.get(k) {
        match b {
            0x07 => return Some((code, start..k, k + 1)),
            0x1b => {
                return if data.get(k + 1) == Some(&b'\\') { Some((code, start..k, k + 2)) } else { None };
            }
            _ => k += 1,
        }
    }
    None
}

impl Emulator {
    pub fn new(cols: usize, rows: usize) -> Emulator {
        Emulator {
            screen: Screen::new(cols, rows),
            parser: vte::Parser::new(),
            carry: Vec::new(),
            cwd: None,
            events: VecDeque::new(),
        }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.screen.resize(cols, rows);
    }

    /// Update the model; append bytes safe to pass to the real terminal to `out`.
    pub fn feed(&mut self, data: &[u8], out: &mut Vec<u8>) {
        self.carry.extend_from_slice(data);
        let mut n = split_tail(&self.carry);
        if self.carry.len() - n > 4096 {
            n = self.carry.len();
        }
        let start = out.len();
        {
            let buf = &self.carry[..n];
            let mut pos = 0;
            let mut i = 0;
            while i < buf.len() {
                if buf[i] != 0x1b {
                    i += 1;
                    continue;
                }
                if let Some((code, arg, end)) = parse_osc(buf, i) {
                    let arg_b = &buf[arg.clone()];
                    match code {
                        133 if arg_b.first() == Some(&b'A') => self.events.push_back(Event::Prompt),
                        7770 | 7771 => {
                            let text = url_unquote(arg_b);
                            if code == 7770 {
                                self.events.push_back(Event::Exec(text));
                            } else {
                                self.cwd = Some(text.clone());
                                self.events.push_back(Event::Cwd(text));
                            }
                            out.extend_from_slice(&buf[pos..i]);
                            pos = end;
                        }
                        _ => {}
                    }
                    i = end;
                } else {
                    i += 1;
                }
            }
            out.extend_from_slice(&buf[pos..]);
        }
        self.parser.advance(&mut self.screen, &out[start..]);
        self.carry.drain(..n);
    }
}

/// A child process on a pty.
pub struct Pty {
    pub pid: libc::pid_t,
    pub fd: libc::c_int,
}

fn winsize(cols: usize, rows: usize) -> libc::winsize {
    libc::winsize { ws_row: rows as u16, ws_col: cols as u16, ws_xpixel: 0, ws_ypixel: 0 }
}

impl Pty {
    pub fn spawn(argv: &[String], cols: usize, rows: usize) -> io::Result<Pty> {
        let cargs: Vec<CString> = argv.iter().map(|a| CString::new(a.as_str()).unwrap()).collect();
        let mut ptrs: Vec<*const libc::c_char> = cargs.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        let mut ws = winsize(cols, rows);
        let mut master: libc::c_int = -1;
        // SAFETY: forkpty is the documented way to spawn a child on a pty; in the
        // child we only call async-signal-safe functions before exec.
        let pid = unsafe { libc::forkpty(&mut master, std::ptr::null_mut(), std::ptr::null_mut(), &mut ws) };
        if pid < 0 {
            return Err(io::Error::last_os_error());
        }
        if pid == 0 {
            unsafe {
                libc::execvp(ptrs[0], ptrs.as_ptr());
                libc::_exit(127);
            }
        }
        Ok(Pty { pid, fd: master })
    }

    pub fn resize(&self, cols: usize, rows: usize) {
        let ws = winsize(cols, rows);
        unsafe {
            libc::ioctl(self.fd, libc::TIOCSWINSZ, &ws);
        }
    }

    /// Read into `buf`; Ok(0) on EOF (a dead child reports EIO on some systems).
    pub fn read(&self, buf: &mut [u8]) -> usize {
        loop {
            let n = unsafe { libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n >= 0 {
                return n as usize;
            }
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return 0;
        }
    }

    pub fn write(&self, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            let n = unsafe { libc::write(self.fd, data.as_ptr() as *const libc::c_void, data.len()) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted || e.kind() == io::ErrorKind::WouldBlock {
                    continue;
                }
                return Err(e);
            }
            data = &data[n as usize..];
        }
        Ok(())
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
            libc::kill(self.pid, libc::SIGHUP);
            // reap without blocking; the child usually exits on SIGHUP promptly
            let mut status = 0;
            libc::waitpid(self.pid, &mut status, libc::WNOHANG);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Transport {
    Ssh,
    Mosh,
}

/// A session to `host` running the hooked shell prepared by `remote`.
pub struct Session {
    pub pty: Pty,
    pub em: Emulator,
    pub cols: usize,
    pub rows: usize,
}

pub fn ssh_base(host: &str) -> Vec<String> {
    vec![
        "ssh".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ControlMaster=auto".into(),
        "-o".into(),
        "ControlPath=~/.ssh/tns-%C".into(),
        "-o".into(),
        "ControlPersist=120".into(),
        host.into(),
    ]
}

impl Session {
    /// Start the hooked shell of session `id` over `transport`.  Mosh's own
    /// prediction is off: its speculative echo would otherwise be learned as
    /// the shell's redraw.
    pub fn open(host: &str, cols: usize, rows: usize, transport: Transport, id: &str, sink: &Sink, cwd: Option<&str>) -> io::Result<Session> {
        let launch = launch_argv(id, sink, cwd);
        let argv = match transport {
            Transport::Ssh => {
                let mut v = ssh_base(host);
                v.insert(1, "-t".into());
                v.extend(launch);
                v
            }
            Transport::Mosh => {
                let ssh = ssh_base(host);
                let mut v = vec!["mosh".to_string(), "--predict=never".into(), format!("--ssh={}", ssh[..ssh.len() - 1].join(" ")), host.into(), "--".into()];
                v.extend(launch);
                v
            }
        };
        let pty = Pty::spawn(&argv, cols, rows)?;
        let mut em = Emulator::new(cols, rows);
        if transport == Transport::Mosh {
            em.screen.track_alt = false; // mosh-client draws inside the alternate screen
        }
        Ok(Session { pty, em, cols, rows })
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.cols = cols;
        self.rows = rows;
        self.pty.resize(cols, rows);
        self.em.resize(cols, rows);
    }
}

/// Reads prompt/exec/cwd events from the server-side event file over a
/// multiplexed ssh connection (`tail -F`), for transports that do not pass
/// OSC marks through.  Wakes the main loop through a pipe.
pub struct EventChannel {
    pub events: Arc<Mutex<VecDeque<Event>>>,
    pub wake_fd: libc::c_int,
    child: Arc<Mutex<Option<Child>>>,
    stopping: Arc<AtomicBool>,
}

impl EventChannel {
    pub fn start(host: &str, path: &str) -> io::Result<EventChannel> {
        let mut fds = [0 as libc::c_int; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let (rd, wr) = (fds[0], fds[1]);
        let events: Arc<Mutex<VecDeque<Event>>> = Default::default();
        let child: Arc<Mutex<Option<Child>>> = Default::default();
        let stopping = Arc::new(AtomicBool::new(false));
        let (host, path) = (host.to_string(), path.to_string());
        let (ev2, child2, stop2) = (events.clone(), child.clone(), stopping.clone());
        std::thread::Builder::new()
            .name("events".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                let mut first = true;
                while !stop2.load(std::sync::atomic::Ordering::Relaxed) {
                    let base = ssh_base(&host);
                    // first attach replays the file (fish may already have written
                    // to it); a reconnect after a drop only follows new lines.
                    let remote = format!("touch {p}; tail -n {n} -F {p}", p = fish_quote(&path), n = if first { "+1" } else { "0" });
                    first = false;
                    // -tt: a remote pty, so the tail is hung up when this ssh dies
                    let spawned = Command::new(&base[0]).arg("-tt").args(&base[1..]).arg(&remote).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
                    let mut c = match spawned {
                        Ok(c) => c,
                        Err(_) => {
                            std::thread::sleep(Duration::from_secs(2));
                            continue;
                        }
                    };
                    let stdout = c.stdout.take().unwrap();
                    *child2.lock().unwrap() = Some(c);
                    for line in BufReader::new(stdout).lines() {
                        let line = match line {
                            Ok(l) => l.trim_end_matches(['\r', '\n']).to_string(),
                            Err(_) => break,
                        };
                        let ev = match (line.get(..1), line.get(2..)) {
                            (Some("P"), _) => Event::Prompt,
                            (Some("X"), Some(arg)) => Event::Exec(url_unquote(arg.as_bytes())),
                            (Some("D"), Some(arg)) => Event::Cwd(url_unquote(arg.as_bytes())),
                            _ => continue,
                        };
                        ev2.lock().unwrap().push_back(ev);
                        unsafe {
                            libc::write(wr, b"\n".as_ptr() as *const libc::c_void, 1);
                        }
                    }
                    if let Some(mut c) = child2.lock().unwrap().take() {
                        let _ = c.kill();
                        let _ = c.wait();
                    }
                    if !stop2.load(std::sync::atomic::Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_secs(2));
                    }
                }
            })
            .expect("spawn event thread");
        Ok(EventChannel { events, wake_fd: rd, child, stopping })
    }

    /// Drain the wake pipe and move pending events into `into`.
    pub fn drain(&self, into: &mut VecDeque<Event>) {
        let mut b = [0u8; 64];
        unsafe {
            libc::read(self.wake_fd, b.as_mut_ptr() as *mut libc::c_void, b.len());
        }
        into.extend(self.events.lock().unwrap().drain(..));
    }

    pub fn stop(&self) {
        self.stopping.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Wait for `fd` to become readable for up to `ms` milliseconds.
pub fn poll_read(fds: &[libc::c_int], ms: i32) -> Vec<bool> {
    let mut pfds: Vec<libc::pollfd> = fds.iter().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
    let r = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, ms) };
    if r <= 0 {
        return vec![false; fds.len()];
    }
    pfds.iter().map(|p| p.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_split() {
        assert_eq!(split_tail(b"abc\x1b["), 3);
        assert_eq!(split_tail(b"abc\x1b[1;2H"), 9);
        assert_eq!(split_tail(b"abc\x1b]7;file://x/y"), 3);
        assert_eq!(split_tail(b"abc\x1b]7;x\x07"), 9);
        assert_eq!(split_tail(b"abc\x1b"), 3);
        assert_eq!(split_tail(b"abc\x1b(B"), 6);
    }

    #[test]
    fn osc_events_and_stripping() {
        let mut em = Emulator::new(20, 2);
        let mut out = Vec::new();
        em.feed(b"a\x1b]7771;/tmp/x%20y\x07b\x1b]133;A\x07c\x1b]7770;ls%20-l\x1b\\d", &mut out);
        assert_eq!(out, b"ab\x1b]133;A\x07cd");
        assert_eq!(em.cwd.as_deref(), Some("/tmp/x y"));
        assert!(matches!(em.events.pop_front(), Some(Event::Cwd(_))));
        assert!(matches!(em.events.pop_front(), Some(Event::Prompt)));
        assert!(matches!(em.events.pop_front(), Some(Event::Exec(s)) if s == "ls -l"));
        assert_eq!(em.screen.grid.row(0)[3].chr(), Some('d'));
    }

    #[test]
    fn partial_sequences_are_held() {
        let mut em = Emulator::new(10, 2);
        let mut out = Vec::new();
        em.feed(b"x\x1b[3", &mut out);
        assert_eq!(out, b"x");
        em.feed(b"1mY", &mut out);
        assert_eq!(out, b"x\x1b[31mY");
        assert_eq!(em.screen.grid.row(0)[1].chr(), Some('Y'));
    }

    #[test]
    fn osc_split_between_st_bytes_is_held() {
        let mut em = Emulator::new(20, 2);
        let mut out = Vec::new();
        em.feed(b"a\x1b]7771;/tmp/x\x1b", &mut out);
        assert_eq!(out, b"a");
        assert!(em.events.is_empty());
        em.feed(b"\\b", &mut out);
        assert_eq!(out, b"ab");
        assert_eq!(em.cwd.as_deref(), Some("/tmp/x"));
        assert!(matches!(em.events.pop_front(), Some(Event::Cwd(s)) if s == "/tmp/x"));
        assert!(em.events.is_empty());
    }

    #[test]
    fn osc_events_survive_every_chunk_boundary() {
        for (first, second) in [("\x07", "\x1b\\"), ("\x1b\\", "\x07")] {
            let input = format!(
                "a\x1b]7771;/tmp/x%20y{first}b\x1b[31mc\x1b]133;A{second}d\
                 \x1b]0;title{first}e\x1b]7770;echo%20%C3%A9{second}f\
                 \x1b(Bg\x1b7h\x1b[0mi\x1b]7771;/tmp/z{second}j"
            );
            let expected = format!("ab\x1b[31mc\x1b]133;A{second}d\x1b]0;title{first}ef\x1b(Bg\x1b7h\x1b[0mij");
            let check = |chunks: &[&[u8]]| {
                let mut em = Emulator::new(30, 2);
                let mut out = Vec::new();
                for chunk in chunks {
                    em.feed(chunk, &mut out);
                    assert!(expected.as_bytes().starts_with(&out), "marker leaked for chunks={chunks:?}");
                }
                assert_eq!(out, expected.as_bytes(), "chunks={chunks:?}");
                assert!(em.carry.is_empty());
                assert_eq!(em.cwd.as_deref(), Some("/tmp/z"));
                assert!(matches!(em.events.pop_front(), Some(Event::Cwd(s)) if s == "/tmp/x y"));
                assert!(matches!(em.events.pop_front(), Some(Event::Prompt)));
                assert!(matches!(em.events.pop_front(), Some(Event::Exec(s)) if s == "echo é"));
                assert!(matches!(em.events.pop_front(), Some(Event::Cwd(s)) if s == "/tmp/z"));
                assert!(em.events.is_empty());
            };
            let bytes = input.as_bytes();
            // Every two- and three-chunk split, including empty chunks, plus a
            // maximally fragmented byte-at-a-time stream.
            for a in 0..=bytes.len() {
                for b in a..=bytes.len() {
                    check(&[&bytes[..a], &bytes[a..b], &bytes[b..]]);
                }
            }
            check(&bytes.chunks(1).collect::<Vec<_>>());
        }
    }

    #[test]
    fn incomplete_osc_cap_and_escape_resynchronization_are_preserved() {
        let mut em = Emulator::new(20, 2);
        let mut out = Vec::new();
        let mut marker = b"\x1b]7771;".to_vec();
        marker.resize(4095, b'x');
        marker.push(0x1b);
        em.feed(&marker, &mut out);
        assert!(out.is_empty());
        assert_eq!(em.carry.len(), 4096);
        em.feed(b"x", &mut out);
        marker.push(b'x');
        assert_eq!(out, marker);
        assert!(em.carry.is_empty());
        assert!(em.events.is_empty());

        let mut em = Emulator::new(20, 2);
        let mut out = Vec::new();
        let mut unterminated = b"\x1b]7771;".to_vec();
        unterminated.resize(4096, b'x');
        em.feed(&unterminated, &mut out);
        assert!(out.is_empty());
        em.feed(b"x", &mut out);
        unterminated.push(b'x');
        assert_eq!(out, unterminated);
        assert!(em.carry.is_empty());

        // ESC followed by something other than backslash cancels the OSC.
        assert_eq!(split_tail(b"a\x1b]7771;x\x1b[31"), b"a\x1b]7771;x".len());
        assert_eq!(split_tail(b"a\x1b]7771;x\x1b[31m"), b"a\x1b]7771;x\x1b[31m".len());
        assert_eq!(split_tail(b"a\x1b[31\x1b("), b"a\x1b[31".len());
        assert_eq!(split_tail(b"a\x1b[31\x1b(B"), b"a\x1b[31\x1b(B".len());
    }
}
