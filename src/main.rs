//! tns: a predictive terminal for a remote shell.
//!
//! The real shell runs on the remote host, reached over mosh's protocol
//! (`mosh`) or a plain ssh pty.  Locally we keep a model of the remote screen
//! and a cache of "what does the screen look like after key K is pressed in
//! state S".  Keystrokes are painted from the cache instantly and reconciled
//! when the real screen arrives; keys the cache does not know are echoed
//! literally (`echo`).  The cache is seeded by hidden probe sessions that
//! type your history into the remote shell, and it keeps learning from every
//! keystroke you type.

mod agent;
mod cache;
mod echo;
mod mosh;
mod paint;
mod probe;
mod remote;
mod session;
mod setup;
mod shared;
mod term;
mod tui;

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cache::{key_hash, state_hash, Anchor, Cache, Key};
use paint::{cup, redraw, repaint, sgr, RunWriter};
use probe::Prober;
use remote::{Shell, Sink};
use session::{poll_read, Emulator, Event, EventChannel, Link, Session, Transport};
use shared::{Shared, Stats};
use term::Grid;

struct Args {
    host: String,
    probes: usize,
    calib_seconds: f64,
    history: usize,
    debug: bool,
    dump: Option<(usize, usize)>,
    bench: Option<PathBuf>,
    ssh: bool,
    mosh_client: bool,
    shell: Option<String>,
    tui_prediction: bool,
    shell_prediction: bool,
}

fn usage() -> ! {
    usage_exit(2)
}

fn usage_exit(code: i32) -> ! {
    eprintln!(
        "usage: tns [--ssh | --mosh-client] [--shell NAME] [--probes N] [--calib-seconds S] [--history N] [--debug] HOST\n\
         \x20      tns --dump-screen COLSxROWS < bytes   (emulator test mode)\n\
         \x20      tns --bench CAPTURE.bin               (micro benchmarks)\n\
         \x20      tns agent <claude|codex|pi|opencode> HOST   (local UI for a remote agent, see tns agent --help)\n\
         \x20      tns setup [--local] [--ssh] [HOST]   (read-only local and remote setup checks)\n\n\
         predictive terminal for remote shells and existing tools\n\n\
         --probes N          hidden calibration sessions (default 6)\n\
         --calib-seconds S   burst calibration time; afterwards one probe keeps learning (default 12)\n\
         --history N         how many recent history entries to learn (default 400)\n\
         --ssh               carry the session over plain ssh instead of mosh (the default)\n\
         --mosh-client       use the installed mosh program instead of tns's built-in mosh client\n\
         --shell NAME        remote shell to start (bash, zsh, fish, ...); default: the login shell\n\
         --no-tui-prediction disable local typing previews inside applications\n\
         --no-shell-prediction disable literal typing previews on shell cache misses\n\
         --debug             log to ~/.cache/tns/debug.log"
    );
    std::process::exit(code)
}

fn parse_args() -> Args {
    let mut a = Args { host: String::new(), probes: 6, calib_seconds: 12.0, history: 400, debug: false, dump: None, bench: None, ssh: false, mosh_client: false, shell: None, tui_prediction: true, shell_prediction: true };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let val = |it: &mut dyn Iterator<Item = String>| it.next().unwrap_or_else(|| usage());
        match arg.as_str() {
            "--probes" => a.probes = val(&mut it).parse().unwrap_or_else(|_| usage()),
            "--calib-seconds" => a.calib_seconds = val(&mut it).parse().unwrap_or_else(|_| usage()),
            "--history" => a.history = val(&mut it).parse().unwrap_or_else(|_| usage()),
            "--debug" => a.debug = true,
            "--ssh" => a.ssh = true,
            "--mosh" => (a.ssh, a.mosh_client) = (false, false),
            "--mosh-client" => (a.ssh, a.mosh_client) = (false, true),
            "--no-tui-prediction" => a.tui_prediction = false,
            "--no-shell-prediction" => a.shell_prediction = false,
            "--shell" => a.shell = Some(val(&mut it)),
            "--print-hooks" => {
                // debugging aid: tns --print-hooks bash osc
                let sh = Shell::from_name(&val(&mut it));
                let sink = if val(&mut it) == "file" { Sink::File("/tmp/tns-events".into()) } else { Sink::Osc };
                print!("{}", remote::hooks(sh, &sink));
                std::process::exit(0)
            }
            "--dump-screen" => {
                let v = val(&mut it);
                let (c, r) = v.split_once('x').unwrap_or_else(|| usage());
                a.dump = Some((c.parse().unwrap_or_else(|_| usage()), r.parse().unwrap_or_else(|_| usage())));
            }
            "--bench" => a.bench = Some(PathBuf::from(val(&mut it))),
            "-h" | "--help" => usage_exit(0),
            "-V" | "--version" => {
                println!("tns {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0)
            }
            s if s.starts_with('-') => usage(),
            _ => a.host = arg,
        }
    }
    if a.host.is_empty() && a.dump.is_none() && a.bench.is_none() {
        usage();
    }
    a
}

fn cache_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".cache").join("tns")
}

fn term_size() -> (usize, usize) {
    let mut ws = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    let r = unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) };
    if r != 0 {
        return (80, 24);
    }
    (if ws.ws_col == 0 { 80 } else { ws.ws_col as usize }, if ws.ws_row == 0 { 24 } else { ws.ws_row as usize })
}

static RESIZED: AtomicBool = AtomicBool::new(false);
extern "C" fn on_winch(_: libc::c_int) {
    RESIZED.store(true, Ordering::Relaxed);
}

static QUIT: AtomicBool = AtomicBool::new(false);
extern "C" fn on_quit(_: libc::c_int) {
    QUIT.store(true, Ordering::Relaxed);
}

struct RawMode {
    old: libc::termios,
}

impl RawMode {
    fn enter() -> Option<RawMode> {
        let mut old: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(0, &mut old) } != 0 {
            return None;
        }
        let mut raw = old;
        unsafe {
            libc::cfmakeraw(&mut raw);
            libc::tcsetattr(0, libc::TCSANOW, &raw);
        }
        Some(RawMode { old })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(0, libc::TCSADRAIN, &self.old);
        }
    }
}

fn write_all(fd: libc::c_int, mut data: &[u8]) {
    while !data.is_empty() {
        let n = unsafe { libc::write(fd, data.as_ptr() as *const libc::c_void, data.len()) };
        if n < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        data = &data[n as usize..];
    }
}

/// Emulator test mode: feed stdin through the screen model and print it.
fn dump_screen(cols: usize, rows: usize) {
    let mut data = Vec::new();
    io::stdin().read_to_end(&mut data).unwrap();
    let mut em = Emulator::new(cols, rows);
    let mut out = Vec::new();
    em.feed(&data, &mut out);
    let g = &em.screen.grid;
    let mut o = String::new();
    o.push_str(&format!("cursor {} {}\n", g.cy, g.cx));
    for y in 0..rows {
        for c in g.row(y) {
            let ch = match c.chr() {
                Some(ch) => ch.to_string(),
                None => String::new(),
            };
            let col = |v: u32| match term::decode_color(v) {
                term::Color::Default => "default".to_string(),
                term::Color::Idx(n) => format!("i{}", n),
                term::Color::Rgb(r, g, b) => format!("{:02x}{:02x}{:02x}", r, g, b),
            };
            o.push_str(&format!("{}\t{}\t{}\t{}\n", ch, col(c.fg()), col(c.bg), c.flags() & !term::F_BLINK & !term::F_DIM));
        }
    }
    print!("{}", o);
}

/// Micro benchmarks of the hot paths, comparable with tests/bench_micro.py.
fn bench(capture: &std::path::Path) {
    let data = fs::read(capture).expect("read capture");
    let (cols, rows) = (120usize, 40usize);
    // 1. emulator throughput
    let reps = (20_000_000 / data.len().max(1)).max(1);
    let mut em = Emulator::new(cols, rows);
    let mut out = Vec::new();
    let t = Instant::now();
    for _ in 0..reps {
        out.clear();
        em.feed(&data, &mut out);
    }
    let dt = t.elapsed().as_secs_f64();
    println!("emulator: {:.1} MB/s ({} bytes x{}, {:.3} s)", (data.len() * reps) as f64 / dt / 1e6, data.len(), reps, dt);
    // 2. per-keystroke work: state hash + cache lookup + diff apply + overlay paint
    let mut em = Emulator::new(cols, rows);
    em.feed(b"\x1b[1;38;2;137;180;250m~/Developer/tns\x1b[0m \r\n\x1b[1;32m\xe2\x9d\xaf\x1b[0m \x1b[K\r\x1b[112C21:58:22\r\x1b[2C", &mut out);
    let anchor = (em.screen.grid.cy, em.screen.grid.cx);
    let pre = em.screen.grid.clone();
    em.feed(b"\x1b[34mls\x1b[0m\x1b[38;5;8m -la\x1b[0m\x1b[4D", &mut out);
    let post = em.screen.grid.clone();
    let dir = std::env::temp_dir().join(format!("tns-bench-{}", std::process::id()));
    let mut cache = Cache::load(&dir.join("c.bin"));
    let k = key_hash(state_hash(&pre, anchor), b"l");
    cache.put_diff(k, &pre, &post, anchor);
    for i in 0..5000u32 {
        // pad the cache with plausible entries
        cache.put_diff(key_hash(state_hash(&pre, anchor), &i.to_le_bytes()), &pre, &post, anchor);
    }
    let n = 200_000;
    let mut pred = Grid::new(cols, rows);
    let mut painted = 0usize;
    let t = Instant::now();
    for _ in 0..n {
        let key = key_hash(state_hash(&pre, anchor), b"l");
        pred.copy_from(&pre);
        if !cache.apply(key, &mut pred, anchor) {
            panic!("cache miss");
        }
        out.clear();
        let mut w = RunWriter::new(&mut out);
        for y in anchor.0..rows {
            let (a, b) = (pred.row(y), pre.row(y));
            if a != b {
                for x in 0..cols {
                    if a[x] != b[x] {
                        w.cell(y, x, &a[x]);
                    }
                }
            }
        }
        w.finish();
        painted += out.len();
    }
    let dt = t.elapsed().as_secs_f64();
    println!("keystroke: {:.2} us per key (state hash + lookup + apply + paint, {} bytes painted)", dt / n as f64 * 1e6, painted / n);
    // 3. cache save/load
    let t = Instant::now();
    cache.save().unwrap();
    let c2 = Cache::load(&dir.join("c.bin"));
    println!("cache: {} entries save+load {:.1} ms, {} KB in memory, {} KB on disk", c2.len(), t.elapsed().as_secs_f64() * 1e3, c2.mem_bytes() / 1024, fs::metadata(dir.join("c.bin")).map(|m| m.len() / 1024).unwrap_or(0));
    let _ = fs::remove_dir_all(&dir);
}

/// One keystroke that has been sent but not yet confirmed by the remote.
struct Inflight {
    unit: [u8; 8],
    ulen: u8,
    pre_key: Key,
    t: Instant,
    val: Option<Key>,     // cache key of the applied prediction
    expected: Option<Key>, // state hash the confirmed screen should reach
    has_snap: bool,
    seen: bool,
    num: u64, // native mosh: our input state that carries the key
}

impl Inflight {
    fn unit_str(&self) -> String {
        String::from_utf8_lossy(&self.unit[..self.ulen as usize]).into_owned()
    }
}

const OUTPUT_QUIET: Duration = Duration::from_millis(40);

/// Output is not an acknowledgment of any particular key. Only a single key
/// with its own snapshot can be learned after ordinary output quiescence.
/// Ambiguous bursts (and failed predictions) get an RTT-sized grace period
/// after the *last* key, then are discarded without learning a combined diff.
fn inflight_deadline(inflight: &VecDeque<Inflight>, last_out: Option<Instant>, rtt: f64) -> Option<Instant> {
    let first = inflight.front()?;
    let last = inflight.back()?;
    let lo = last_out.filter(|lo| *lo >= last.t)?;
    let quiet = lo + OUTPUT_QUIET;
    if inflight.len() == 1 && first.expected.is_none() && first.has_snap {
        Some(quiet)
    } else {
        Some(quiet.max(last.t + Duration::from_secs_f64(0.35f64.max(3.0 * rtt))))
    }
}

/// Round up sub-millisecond waits: truncating them to zero busy-polls until
/// the deadline. Callers only supply timers whose expiry can change state.
fn poll_timeout_ms(now: Instant, deadlines: impl IntoIterator<Item = Instant>) -> i32 {
    let wait = deadlines.into_iter().map(|t| t.saturating_duration_since(now)).min().unwrap_or(Duration::from_millis(500));
    wait.as_nanos().div_ceil(1_000_000).min(500) as i32
}

/// Classify input without withholding or changing any bytes sent to the PTY.
/// DA/DSR replies are not typing. An incomplete possible reply blocks taking
/// an anchor until it is identified; other escapes remain keyboard input.
#[derive(Default)]
struct TerminalInput {
    pending: Vec<u8>,
    query: Vec<u8>,
    cursor_reports: [u8; 2], // outstanding normal/private CPR queries
}

struct InputActivity {
    user: bool,
    protocol: bool, // do not learn/predict a reply or a fragmented sequence
}

impl TerminalInput {
    fn output(&mut self, bytes: &[u8]) {
        const QUERIES: [&[u8]; 2] = [b"\x1b[6n", b"\x1b[?6n"];
        for &b in bytes {
            if b == 0x1b {
                self.query.clear();
            } else if self.query.is_empty() {
                continue;
            }
            self.query.push(b);
            if let Some(i) = QUERIES.iter().position(|q| *q == self.query) {
                self.cursor_reports[i] = self.cursor_reports[i].saturating_add(1);
                self.query.clear();
            } else if !QUERIES.iter().any(|q| q.starts_with(&self.query)) {
                self.query.clear();
            }
        }
    }

    fn csi_reply(body: &[u8], cursor_reports: &mut [u8; 2]) -> bool {
        let (&final_byte, params) = body.split_last().unwrap();
        let numbers = |p: &[u8]| p.split(|&b| b == b';').all(|n| !n.is_empty() && n.iter().all(u8::is_ascii_digit));
        match final_byte {
            b'c' => matches!(params.first(), Some(b'?' | b'>')) && numbers(&params[1..]),
            b'n' => matches!(params, b"0" | b"?0"),
            b'R' => {
                let private = usize::from(params.starts_with(b"?"));
                let p = &params[private..];
                // CPR has the same encoding as some modified function keys.
                // Only recognize it when the terminal was actually queried.
                if cursor_reports[private] > 0 && p.iter().filter(|&&b| b == b';').count() == 1 && numbers(p) {
                    cursor_reports[private] -= 1;
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    fn input(&mut self, bytes: &[u8]) -> InputActivity {
        let mut activity = InputActivity { user: false, protocol: !self.pending.is_empty() };
        for &b in bytes {
            if self.pending.is_empty() && b != 0x1b {
                activity.user = true;
                continue;
            }
            self.pending.push(b);
            if b"\x1b[".starts_with(&self.pending) {
                continue;
            }
            if self.pending.starts_with(b"\x1b[") {
                if (0x40..=0x7e).contains(&b) {
                    if Self::csi_reply(&self.pending[2..], &mut self.cursor_reports) {
                        activity.protocol = true;
                    } else {
                        activity.user = true;
                    }
                    self.pending.clear();
                    continue;
                }
                if self.pending.len() <= 128 && (b.is_ascii_digit() || matches!(b, b';' | b'?' | b'>')) {
                    continue;
                }
            }
            activity.user = true;
            self.pending.clear();
        }
        activity.protocol |= !self.pending.is_empty();
        activity
    }
}

/// Prompt events and mosh screen frames have no shared sequence number. Allow
/// a recent pre-event screen from this command epoch, but give an early event
/// time to catch up before using it. Arbitrarily delayed/reordered frames
/// cannot be distinguished from command output without a protocol change.
const PROMPT_EVENT_LOOKBACK: Duration = Duration::from_secs(1);
const PROMPT_EVENT_GRACE: Duration = Duration::from_millis(350);

struct PromptAnchor {
    screen_after: Instant,
    event: Option<(Instant, bool)>, // arrival time, whether screen order is known
    pending: bool,
    typed: bool,
}

impl PromptAnchor {
    fn new(now: Instant, has_hooks: bool) -> Self {
        Self { screen_after: now, event: if has_hooks { None } else { Some((now, true)) }, pending: true, typed: false }
    }

    fn prompt(&mut self, now: Instant, in_band: bool) {
        self.event = Some((now, in_band));
        // A delayed event must not turn the first key's echo into the anchor.
        self.pending = !self.typed;
    }

    fn input(&mut self) {
        self.typed = true;
        self.pending = false;
    }

    fn resize(&mut self, now: Instant) {
        self.screen_after = now;
        self.event = self.event.map(|_| (now, true));
        self.pending = self.event.is_some() && !self.typed;
    }

    #[cfg(test)]
    fn deadline(&self, last_out: Option<Instant>, quiet: Duration) -> Option<Instant> {
        self.deadline_after(last_out, quiet, PROMPT_EVENT_GRACE)
    }

    /// `grace`: how long after an event that outran the screen to wait for
    /// the screen to follow.
    fn deadline_after(&self, last_out: Option<Instant>, quiet: Duration, grace: Duration) -> Option<Instant> {
        if !self.pending || self.typed {
            return None;
        }
        let (event_at, in_band) = self.event?;
        let lo = last_out.filter(|lo| *lo >= self.screen_after)?;
        if lo < event_at {
            if in_band || event_at.duration_since(lo) > PROMPT_EVENT_LOOKBACK {
                return None;
            }
            Some((lo + quiet).max(event_at + quiet.max(grace)))
        } else {
            Some(lo + quiet)
        }
    }
}

struct Client {
    shared: Arc<Shared>,
    args: Args,
    sess: Session,
    chan: Option<EventChannel>,
    out_fd: libc::c_int,
    out: Vec<u8>,
    buf: Vec<u8>,
    inflight: VecDeque<Inflight>,
    pred: Grid,
    pred_active: bool,
    snap: Grid,
    overlay: Vec<(u16, u16)>,
    overlay_cursor: bool, // the terminal cursor is where the prediction put it
    anchor: Anchor,
    anchor_valid: bool,
    prompt_anchor: PromptAnchor,
    terminal_input: TerminalInput,
    tui: tui::Predictor,
    shell_preview: tui::Predictor,
    last_out: Option<Instant>,
    rtt: f64,
    probes_started: bool,
    last_save: Instant,
    // Native mosh transport only.
    echo: echo::Predictor,
    echo_ack: u64,  // our newest input state the server reports as echoed
    enter_num: u64, // the input state of the key that ended the last command
    shown_num: u64, // the server state on screen
    escape: bool,   // Ctrl-^ was typed; the next key completes the command
    notice: Option<String>,
    started: Instant,
}

/// What mosh-client sends on exit: leave application cursor keys, mouse
/// reporting and the other modes a remote program may have set, then return
/// to the terminal's own screen.
const NATIVE_CLOSE: &[u8] = b"\x1b[?1l\x1b[0m\x1b[?25h\x1b[?1003l\x1b[?1002l\x1b[?1001l\x1b[?1000l\x1b[?1015l\x1b[?1006l\x1b[?1005l\x1b[?2004l\x1b[?1004l\x1b[?1007l\x1b[?5l\x1b[?1049l";
const NO_CONTACT: Duration = Duration::from_millis(6500);

impl Client {
    fn native(&self) -> bool {
        matches!(self.sess.link, Link::Mosh(_))
    }

    /// The native transport validates literal previews with the server's
    /// echo acknowledgments instead (see `echo`).
    fn shell_preview_enabled(&self) -> bool {
        self.args.shell_prediction && self.shared.shell.has_hooks() && !self.native()
    }

    /// A native session's screen model is resized by the server's frames;
    /// until then it does not describe the real terminal.
    fn in_sync(&self) -> bool {
        let g = &self.sess.em.screen.grid;
        (g.cols, g.rows) == (self.sess.cols, self.sess.rows)
    }

    fn log(&self, msg: &str) {
        self.shared.log(msg);
    }

    fn key_of(&self, g: &Grid) -> Key {
        state_hash(g, self.anchor)
    }

    fn start_probes(&self) {
        let shared = self.shared.clone();
        let history = self.args.history;
        let calib = self.args.calib_seconds;
        let nprobes = self.args.probes;
        std::thread::Builder::new()
            .name("coordinator".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                let (tx, rx) = std::sync::mpsc::channel();
                let (h, sh) = (shared.host.clone(), shared.shell);
                std::thread::spawn(move || {
                    let _ = tx.send(remote::history(&h, sh, history));
                });
                let lines: Vec<String> = match rx.recv_timeout(Duration::from_secs(20)) {
                    Ok(l) => l,
                    Err(_) => {
                        shared.log("history fetch timed out");
                        Vec::new()
                    }
                };
                shared.log(&format!("history: {} entries", lines.len()));
                for l in lines.iter().take(history) {
                    shared.enqueue(l, false);
                }
                let deadline = Instant::now() + Duration::from_secs_f64(calib);
                for i in 0..nprobes {
                    Prober::start(shared.clone(), i, deadline);
                }
            })
            .expect("spawn coordinator");
    }

    /// Draw pred over confirmed into `self.out`; remember touched cells.
    fn paint_overlay(&mut self) {
        self.overlay.clear();
        if !self.pred_active {
            return;
        }
        let conf = &self.sess.em.screen.grid;
        let pred = &self.pred;
        let mut w = RunWriter::new(&mut self.out);
        for y in self.anchor.0..conf.rows.min(pred.rows) {
            let (a, b) = (pred.row(y), conf.row(y));
            if a == b {
                continue;
            }
            for x in 0..a.len().min(b.len()) {
                if a[x] != b[x] {
                    w.cell(y, x, &a[x]);
                    self.overlay.push((y as u16, x as u16));
                }
            }
        }
        w.finish();
        cup(&mut self.out, pred.cy, pred.cx);
        self.overlay_cursor = true;
    }

    /// Repaint the confirmed screen under every overlay cell and put the
    /// cursor back where the remote believes it is.  The remote (fish, or
    /// mosh-client in particular) emits output relative to that cursor, so
    /// this must happen before any real bytes are passed through.
    fn restore_overlay(&mut self) {
        if self.overlay.is_empty() && !self.overlay_cursor {
            return;
        }
        let conf = &self.sess.em.screen.grid;
        let mut w = RunWriter::new(&mut self.out);
        for &(y, x) in &self.overlay {
            let (y, mut x) = (y as usize, x as usize);
            if y < conf.rows && x < conf.cols {
                // The right half of a wide character is drawn by its left half.
                if conf.row(y)[x].ch == 0 && x > 0 {
                    x -= 1;
                }
                w.cell(y, x, &conf.row(y)[x]);
            }
        }
        w.finish();
        cup(&mut self.out, conf.cy, conf.cx);
        sgr(&mut self.out, &self.sess.em.screen.attrs());
        self.overlay.clear();
        self.overlay_cursor = false;
    }

    fn rebuild_pred(&mut self) {
        self.pred_active = false;
        let cache = self.shared.cache.lock().unwrap();
        for e in &self.inflight {
            let k = match e.val {
                Some(k) => k,
                None => break,
            };
            if !self.pred_active {
                self.pred.copy_from(&self.sess.em.screen.grid);
                self.pred_active = true;
            }
            cache.apply(k, &mut self.pred, self.anchor);
        }
    }

    fn learn(&mut self, e: &Inflight) {
        self.shared.cache.lock().unwrap().put_diff(key_hash(e.pre_key, &e.unit[..e.ulen as usize]), &self.snap, &self.sess.em.screen.grid, self.anchor);
        self.shared.with_stats(|s| s.learned_live += 1);
    }

    /// `in_band`: the events came with the screen bytes just fed (OSC marks),
    /// so that output already counts as "after the prompt".
    fn handle_events(&mut self, in_band: bool) {
        while let Some(ev) = self.sess.em.events.pop_front() {
            if self.shared.log.lock().map(|g| g.is_some()).unwrap_or(false) {
                let name = match &ev {
                    Event::Prompt => "prompt".to_string(),
                    Event::Exec(c) => format!("exec {:?}", c),
                    Event::Cwd(c) => format!("cwd {:?}", c),
                };
                self.log(&format!("event {} (in_band={}) cursor=({}, {})", name, in_band, self.sess.em.screen.grid.cy, self.sess.em.screen.grid.cx));
            }
            match ev {
                Event::Prompt => {
                    self.shell_preview.restore(&self.sess.em.screen, &mut self.out);
                    self.shell_preview.reset();
                    self.tui.restore(&self.sess.em.screen, &mut self.out);
                    self.tui.reset();
                    let now = if in_band { self.last_out.unwrap_or_else(Instant::now) } else { Instant::now() };
                    self.prompt_anchor.prompt(now, in_band);
                    self.anchor_valid = false;
                    self.clear_prediction();
                    if !self.probes_started {
                        self.probes_started = true;
                        self.start_probes();
                    }
                }
                Event::Exec(cmd) => self.shared.enqueue(&cmd, true),
                Event::Cwd(p) => {
                    self.sess.em.cwd = Some(p.clone());
                    *self.shared.cwd.lock().unwrap() = Some(p);
                }
            }
        }
    }

    /// Text from the anchor to the end of the anchor row (the typed command).
    fn anchor_row_text(&self) -> String {
        let g = &self.sess.em.screen.grid;
        let (ay, ax) = self.anchor;
        if ay >= g.rows {
            return String::new();
        }
        let row = g.row(ay);
        let mut s: String = row[ax.min(g.cols)..].iter().filter_map(|c| c.chr()).collect();
        if let Some(i) = s.find("    ") {
            s.truncate(i);
        }
        s.trim().to_string()
    }

    fn clear_prediction(&mut self) {
        self.inflight.clear();
        self.pred_active = false;
    }

    fn flush(&mut self) {
        if !self.out.is_empty() {
            write_all(self.out_fd, &self.out);
            self.out.clear();
        }
    }

    fn anchor_deadline(&self) -> Option<Instant> {
        if !self.prompt_anchor.pending || !self.terminal_input.pending.is_empty() || self.sess.em.screen.alt || !self.in_sync() || !self.sess.em.screen.grid.cells.iter().any(|c| *c != Grid::BLANK_CELL) {
            return None;
        }
        let mut quiet = Duration::from_millis(if !self.shared.shell.has_hooks() { 250 } else if !self.args.ssh { 120 } else { 40 });
        let mut grace = PROMPT_EVENT_GRACE;
        if self.native() {
            // Frames that do not yet acknowledge the key that ended the last
            // command (late echoes of what was typed before it) are not the
            // screen the prompt event is about.
            if self.echo_ack < self.enter_num {
                return None;
            }
            // mosh-server paces its frames at half the round trip (20-250 ms):
            // a prompt still being drawn arrives one such interval later.
            let pacing = (self.rtt / 2.0).clamp(0.02, 0.25);
            quiet = quiet.max(Duration::from_secs_f64(pacing + 0.04));
            grace = grace.max(Duration::from_secs_f64(pacing + 0.15));
        }
        self.prompt_anchor.deadline_after(self.last_out, quiet, grace)
    }

    /// A lone key waiting to be learned on the native transport, before the
    /// server has said the screen includes its echo.
    fn unacked_single(&self) -> Option<&Inflight> {
        let e = self.inflight.front().filter(|_| self.native() && self.inflight.len() == 1)?;
        (e.expected.is_none() && e.has_snap && self.echo_ack < e.num).then_some(e)
    }

    fn inflight_deadline(&self) -> Option<Instant> {
        if !self.anchor_valid || self.sess.em.screen.alt {
            return None;
        }
        let deadline = inflight_deadline(&self.inflight, self.last_out, self.rtt)?;
        // Quiet output is not yet the key's result; give the acknowledgment
        // the grace a burst gets, then give up on learning it.
        Some(self.unacked_single().map_or(deadline, |e| deadline.max(e.t + Duration::from_secs_f64(0.35f64.max(3.0 * self.rtt)))))
    }

    /// Draw the literal echo previews; the cursor ends on the newest prediction.
    fn paint_echo(&mut self) {
        let conf = &self.sess.em.screen.grid;
        let cursor = if self.pred_active { (self.pred.cy, self.pred.cx) } else { (conf.cy, conf.cx) };
        self.echo.paint(&self.sess.em.screen, &mut self.out, cursor);
    }

    /// Offer a key to the echo predictor (native transport).
    fn echo_key(&mut self, unit: &[u8], num: u64, anchor: Option<Anchor>, preview: bool, enabled: bool) {
        if !enabled || !self.in_sync() {
            self.echo.untracked(num);
        } else if self.echo.input(unit, num, &self.sess.em.screen, self.echo_ack, echo::Context { anchor, preview }) {
            self.log("echo preview");
        }
        self.paint_echo();
        self.flush();
    }

    /// The server's frame resized the screen model (native transport).
    fn model_resized(&mut self, cols: usize, rows: usize) {
        self.sess.em.resize(cols, rows);
        self.pred.resize(cols, rows);
        self.snap.resize(cols, rows);
        self.clear_prediction();
        self.overlay.clear();
        self.overlay_cursor = false;
        self.anchor_valid = false;
        self.prompt_anchor.resize(Instant::now());
        self.echo.reset();
    }

    /// Take every local preview off the terminal: remote output is relative
    /// to the real cursor and attributes.
    fn before_output(&mut self) {
        self.restore_notice();
        self.echo.restore(&self.sess.em.screen, &mut self.out);
        self.shell_preview.restore(&self.sess.em.screen, &mut self.out);
        self.restore_overlay();
        self.tui.restore(&self.sess.em.screen, &mut self.out);
    }

    /// Pass remote bytes to the terminal and the screen model.
    fn feed_output(&mut self, data: &[u8]) {
        let Client { sess, out, terminal_input, tui, shell_preview, .. } = self;
        terminal_input.output(data);
        tui.output(data);
        shell_preview.output(data);
        sess.em.feed(data, out);
        let now = Instant::now();
        self.last_out = Some(now);
        if self.inflight.len() == 1 && !self.inflight[0].seen {
            self.inflight[0].seen = true;
            let t = self.inflight[0].t;
            self.rtt = 0.8 * self.rtt + 0.2 * now.duration_since(t).as_secs_f64();
        }
    }

    /// Reconcile predictions with the new confirmed screen and repaint them.
    fn after_output(&mut self) {
        if let Some(cwd) = self.sess.em.cwd.as_ref() {
            let mut g = self.shared.cwd.lock().unwrap();
            if g.as_deref() != Some(cwd.as_str()) {
                *g = Some(cwd.clone());
            }
        }
        self.handle_events(true);
        let native = self.native();
        if self.args.tui_prediction && !native {
            self.tui.observe(&self.sess.em.screen);
            if self.tui.active() {
                // An application's composer is not a shell prompt.
                self.anchor_valid = false;
                self.prompt_anchor.pending = false;
                self.clear_prediction();
                self.shell_preview.reset();
            }
        }
        if self.anchor_valid && self.shell_preview_enabled() {
            self.shell_preview.observe_shell(&self.sess.em.screen, self.anchor);
        }
        if self.anchor_valid && !self.inflight.is_empty() && !self.sess.em.screen.alt {
            let k = self.key_of(&self.sess.em.screen.grid);
            if let Some(i) = self.inflight.iter().position(|e| e.expected == Some(k)) {
                self.inflight.drain(..=i);
                self.shared.with_stats(|s| s.hit += i as u64 + 1);
            }
            self.rebuild_pred();
        }
        self.paint_overlay();
        self.shell_preview.paint(&self.sess.em.screen, &mut self.out);
        self.tui.paint(&self.sess.em.screen, &mut self.out);
        if native {
            self.paint_echo();
        }
        self.flush();
    }

    /// Read the native transport and show the server's newest screen state.
    /// The server sends states, not a byte stream: its diffs build on
    /// whichever state we last acknowledged, so the terminal is always
    /// updated from the difference between two screen models.
    fn native_receive(&mut self) {
        let Some(m) = self.sess.mosh() else { return };
        m.recv(Instant::now());
        let rtt = m.rtt_known().then(|| m.srtt());
        let latest = m.latest();
        let (num, echo_ack) = (latest.num, latest.echo_ack);
        let screen = (num != self.shown_num).then(|| latest.screen.clone());
        if let Some(rtt) = rtt {
            self.rtt = rtt;
        }
        let Some(screen) = screen else { return };
        self.before_output();
        let shown = &self.sess.em.screen;
        let (cols, rows) = (screen.grid.cols, screen.grid.rows);
        let resized = (cols, rows) != (shown.grid.cols, shown.grid.rows);
        // A state may carry nothing but a newer echo acknowledgment.
        let changed = resized || shown.grid.cells != screen.grid.cells || (shown.grid.cy, shown.grid.cx) != (screen.grid.cy, screen.grid.cx);
        repaint(shown, &screen, &mut self.out);
        if resized {
            self.model_resized(cols, rows);
        }
        self.sess.em.screen = screen;
        if changed {
            // Not the time of the read: a resize above restarted the clock
            // that output is measured against.
            self.last_out = Some(Instant::now());
        }
        self.shown_num = num;
        self.echo_ack = echo_ack;
        self.echo.frame(&self.sess.em.screen, echo_ack, changed);
        self.after_output();
    }

    /// mosh's escape key: Ctrl-^ then "." quits, and Ctrl-^ twice (or
    /// Ctrl-^ ^) sends one Ctrl-^.  Returns false when the user quit.
    fn escape_keys(&mut self, data: &mut Vec<u8>) -> bool {
        let mut keys = Vec::with_capacity(data.len());
        for &b in data.iter() {
            if std::mem::take(&mut self.escape) {
                match b {
                    b'.' => return false,
                    0x1e | b'^' => keys.push(0x1e),
                    _ => keys.extend([0x1e, b]),
                }
            } else if b == 0x1e {
                self.escape = true;
            } else {
                keys.push(b);
            }
        }
        *data = keys;
        true
    }

    /// The first screen row is borrowed for a notice while the server is silent.
    fn update_notice(&mut self) {
        let Link::Mosh(m) = &self.sess.link else { return };
        let now = Instant::now();
        let text = match m.silence(now) {
            None if now.duration_since(self.started) > Duration::from_secs(2) => Some("tns: no reply from the mosh server yet (is its UDP port reachable?). To quit: Ctrl-^ .".to_string()),
            Some(silence) if silence > NO_CONTACT => Some(format!("tns: last contact {} s ago. To quit: Ctrl-^ .", silence.as_secs())),
            _ => None,
        };
        if text == self.notice {
            return;
        }
        self.restore_notice();
        if let Some(text) = &text {
            let width = self.sess.cols;
            let line: String = text.chars().chain(std::iter::repeat(' ')).take(width).collect();
            // DECSC/DECRC keep the cursor and attributes the remote expects.
            self.out.extend_from_slice(b"\x1b7\x1b[1;1H\x1b[0;7m");
            self.out.extend_from_slice(line.as_bytes());
            self.out.extend_from_slice(b"\x1b8");
        }
        self.notice = text;
        self.flush();
    }

    fn restore_notice(&mut self) {
        if self.notice.take().is_none() {
            return;
        }
        let g = &self.sess.em.screen.grid;
        self.out.extend_from_slice(b"\x1b7\x1b[1;1H\x1b[0m\x1b[K");
        let mut w = RunWriter::new(&mut self.out);
        for (x, cell) in g.row(0).iter().enumerate().take(self.sess.cols) {
            w.cell(0, x, cell);
        }
        w.finish();
        self.out.extend_from_slice(b"\x1b8");
    }

    /// End a native session we are leaving first, so the remote shell does
    /// not outlive this client.
    fn native_shutdown(&mut self) {
        let Some(m) = self.sess.mosh() else { return };
        if m.peer_closed() || !m.connected() {
            return;
        }
        m.close(Instant::now());
        let give_up = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < give_up && !m.closed() {
            poll_read(&[m.fd()], 20);
            let now = Instant::now();
            m.recv(now);
            m.tick(now);
        }
    }

    fn run(&mut self) {
        unsafe {
            libc::signal(libc::SIGWINCH, on_winch as extern "C" fn(libc::c_int) as usize);
        }
        if self.native() {
            unsafe {
                libc::signal(libc::SIGHUP, on_quit as extern "C" fn(libc::c_int) as usize);
                libc::signal(libc::SIGTERM, on_quit as extern "C" fn(libc::c_int) as usize);
            }
            // As mosh-client does: a screen of our own, and application
            // cursor keys, which the server translates for each program.
            self.out.extend_from_slice(b"\x1b[?1049h\x1b[?1h\x1b[0m");
        }
        self.out.extend_from_slice(b"\x1b[H\x1b[2J");
        self.flush();
        let t_start = Instant::now();

        loop {
            let now = Instant::now();
            let transport = match &self.sess.link {
                Link::Mosh(m) => m.deadline(now),
                Link::Pty(_) => None,
            };
            let timeout = poll_timeout_ms(now, self.inflight_deadline().into_iter().chain(self.anchor_deadline()).chain(self.tui.deadline(self.rtt)).chain(self.shell_preview.deadline(self.rtt)).chain(transport));
            let wake = self.chan.as_ref().map_or(-1, |c| c.wake_fd);
            let ready = poll_read(&[0, self.sess.fd(), wake], timeout);

            if ready[2] {
                if let Some(c) = &self.chan {
                    c.drain(&mut self.sess.em.events);
                }
                self.handle_events(false);
                self.flush();
            }

            if RESIZED.swap(false, Ordering::Relaxed) {
                let (cols, rows) = term_size();
                *self.shared.size.lock().unwrap() = (cols, rows);
                self.sess.resize(cols, rows);
                if !self.native() {
                    self.pred.resize(cols, rows);
                    self.snap.resize(cols, rows);
                }
                self.clear_prediction();
                self.overlay.clear();
                self.overlay_cursor = false;
                self.anchor_valid = false;
                self.prompt_anchor.resize(Instant::now());
                self.tui.reset();
                self.shell_preview.reset();
                self.echo.reset();
                self.notice = None;
                if self.native() && self.in_sync() {
                    // Nothing else redraws a native session whose size is
                    // what it was (a spurious signal, or there and back):
                    // the server only sends what differs from its last state.
                    redraw(&self.sess.em.screen, &mut self.out);
                    self.flush();
                }
            }

            if ready[1] {
                if self.native() {
                    self.native_receive();
                } else {
                    let n = self.sess.read(&mut self.buf);
                    if n == 0 {
                        break;
                    }
                    let buf = std::mem::take(&mut self.buf);
                    self.before_output();
                    self.feed_output(&buf[..n]);
                    self.after_output();
                    self.buf = buf;
                }
            }

            if ready[0] {
                let n = unsafe { libc::read(0, self.buf.as_mut_ptr() as *mut libc::c_void, 4096) };
                if n <= 0 {
                    break;
                }
                let n = n as usize;
                let mut data: Vec<u8> = self.buf[..n].to_vec();
                let native = self.native();
                if native && !self.escape_keys(&mut data) {
                    break;
                }
                // Split short printable input into per-character units.
                let mut units: Vec<&[u8]> = if data.is_empty() { Vec::new() } else { vec![&data[..]] };
                if data.len() <= 6 {
                    if let Ok(s) = std::str::from_utf8(&data) {
                        if s.chars().all(|c| c as u32 >= 32 && c != '\x7f') {
                            units = s.char_indices().map(|(i, c)| &data[i..i + c.len_utf8()]).collect();
                        }
                    }
                }
                for unit in units {
                    // The native transport numbers the input state that
                    // carries each key; the server acknowledges by number.
                    let num = match &mut self.sess.link {
                        Link::Pty(pty) => {
                            if pty.write(unit).is_err() {
                                break;
                            }
                            0
                        }
                        Link::Mosh(m) => m.push_keys(unit, Instant::now()),
                    };
                    // Output processed earlier in this poll turn must not
                    // count as a response to input we have only just sent.
                    let now = Instant::now();
                    let activity = self.terminal_input.input(unit);
                    self.tui.restore(&self.sess.em.screen, &mut self.out);
                    if !activity.protocol {
                        if self.anchor_valid {
                            self.tui.cancel();
                        } else if self.tui.input(unit, now) {
                            self.log("TUI literal edit preview");
                        }
                    } else if activity.user || !self.terminal_input.pending.is_empty() {
                        self.tui.cancel();
                    }
                    self.tui.paint(&self.sess.em.screen, &mut self.out);
                    self.flush();
                    self.shell_preview.restore(&self.sess.em.screen, &mut self.out);
                    if matches!(unit, b"\r" | b"\n" | b"\x03" | b"\x04" | b"\x0c") {
                        self.shell_preview.reset();
                        self.echo.untracked(num);
                        self.shared.with_stats(|s| s.keys += 1);
                        self.restore_overlay();
                        // Never reuse output from before this command/reset
                        // when its prompt event eventually arrives.
                        self.prompt_anchor = PromptAnchor::new(now, self.shared.shell.has_hooks());
                        self.enter_num = num;
                        if !self.shared.shell.has_hooks() {
                            // no shell hooks: the command line on screen is the executed
                            // command, and the next quiet screen is the next prompt
                            if self.anchor_valid && matches!(unit, b"\r" | b"\n") {
                                let cmd = self.anchor_row_text();
                                self.shared.enqueue(&cmd, true);
                            }
                            if !self.probes_started {
                                self.probes_started = true;
                                self.start_probes();
                            }
                        }
                        self.clear_prediction();
                        self.anchor_valid = false;
                        if native {
                            self.paint_echo();
                        }
                        self.flush();
                        continue;
                    }
                    if activity.user {
                        self.prompt_anchor.input();
                    }
                    if activity.protocol {
                        if activity.user || !self.terminal_input.pending.is_empty() {
                            self.shell_preview.cancel();
                        }
                        self.echo.untracked(num);
                        self.shell_preview.paint(&self.sess.em.screen, &mut self.out);
                        self.flush();
                        continue;
                    }
                    self.shared.with_stats(|s| s.keys += 1);
                    if !self.anchor_valid || self.sess.em.screen.alt {
                        self.shell_preview.cancel();
                        if native {
                            self.echo_key(unit, num, None, true, self.args.tui_prediction);
                        }
                        self.flush();
                        continue;
                    }
                    let pre_key = if self.pred_active { self.key_of(&self.pred) } else { self.key_of(&self.sess.em.screen.grid) };
                    let learnable = self.inflight.is_empty();
                    if learnable {
                        self.snap.copy_rows_from(&self.sess.em.screen.grid, self.anchor.0);
                    }
                    let mut e = Inflight { unit: [0; 8], ulen: unit.len().min(8) as u8, pre_key, t: now, val: None, expected: None, has_snap: learnable, seen: false, num };
                    e.unit[..e.ulen as usize].copy_from_slice(&unit[..e.ulen as usize]);
                    let chain_ok = self.inflight.iter().all(|x| x.val.is_some());
                    let mut predicted = false;
                    if chain_ok {
                        let k = key_hash(pre_key, unit);
                        let cache = self.shared.cache.lock().unwrap();
                        if cache.has(k) {
                            if !self.pred_active {
                                self.pred.copy_from(&self.sess.em.screen.grid);
                                self.pred_active = true;
                            }
                            cache.apply(k, &mut self.pred, self.anchor);
                            e.val = Some(k);
                            predicted = true;
                        }
                    }
                    if predicted {
                        e.expected = Some(self.key_of(&self.pred));
                        self.shared.with_stats(|s| s.predicted += 1);
                        self.paint_overlay();
                        self.flush();
                    } else {
                        self.shared.with_stats(|s| s.unpredicted += 1);
                    }
                    self.inflight.push_back(e);
                    if self.shell_preview_enabled() {
                        if self.shell_preview.input_with_preview(unit, now, !predicted) {
                            self.log("shell literal edit preview");
                        }
                        self.shell_preview.paint(&self.sess.em.screen, &mut self.out);
                        self.flush();
                    }
                    if native {
                        // Only a prompt the shell itself reported is a
                        // prompt; without hooks it is a quiet screen.
                        let anchor = self.shared.shell.has_hooks().then_some(self.anchor);
                        self.echo_key(unit, num, anchor, !predicted, self.args.shell_prediction);
                    }
                }
            }

            // ---- timers: quiescence
            let now = Instant::now();
            if self.shell_preview.deadline(self.rtt).is_some_and(|t| now >= t) {
                self.shell_preview.restore(&self.sess.em.screen, &mut self.out);
                self.shell_preview.cancel();
                self.log("shell preview expired without a matching remote field");
                self.flush();
            }
            if self.tui.deadline(self.rtt).is_some_and(|t| now >= t) {
                self.tui.restore(&self.sess.em.screen, &mut self.out);
                self.tui.cancel();
                self.log("TUI preview expired without a matching remote field");
                self.flush();
            }
            // Both channel orders are possible. Wait for a settled candidate
            // in the current epoch, and never capture a screen after typing.
            if self.anchor_deadline().is_some_and(|t| now >= t) {
                self.prompt_anchor.pending = false;
                let g = &self.sess.em.screen.grid;
                self.anchor = (g.cy, g.cx);
                self.anchor_valid = true;
                if self.shared.shell.has_hooks() {
                    self.echo.prompt(&self.sess.em.screen, self.anchor);
                }
                if self.shell_preview_enabled() {
                    self.shell_preview.observe_shell(&self.sess.em.screen, self.anchor);
                }
                let cwd = self.sess.em.cwd.clone();
                self.log(&format!("anchor={:?} cwd={:?}", self.anchor, cwd));
                if !self.probes_started && !self.shared.shell.has_hooks() {
                    self.probes_started = true;
                    self.start_probes();
                }
            }
            if self.inflight_deadline().is_some_and(|t| now >= t) {
                let single = self.inflight.len() == 1;
                let unacked = self.unacked_single().is_some();
                let e = self.inflight.pop_front().unwrap();
                if e.expected.is_some() {
                    // Predicted, but the confirmed screen never matched.
                    self.shared.with_stats(|s| s.miss += 1);
                    self.log(&format!("miss unit={:?}", e.unit_str()));
                }
                if single && e.has_snap && !unacked {
                    self.learn(&e);
                }
                self.clear_prediction();
                self.restore_overlay();
                self.shell_preview.paint(&self.sess.em.screen, &mut self.out);
                if self.native() {
                    self.paint_echo();
                }
                self.flush();
            }
            if t_start.elapsed() > Duration::from_secs(5) && self.last_save.elapsed() > Duration::from_secs(15) {
                self.last_save = Instant::now();
                let _ = self.shared.cache.lock().unwrap().save();
            }

            // ---- native transport: send what is due, notice silence, leave
            if let Some(m) = self.sess.mosh() {
                if QUIT.load(Ordering::Relaxed) {
                    break;
                }
                m.tick(Instant::now());
                if m.peer_closed() {
                    break;
                }
                self.update_notice();
            }
        }
        self.shell_preview.restore(&self.sess.em.screen, &mut self.out);
        self.shell_preview.cancel();
        self.tui.restore(&self.sess.em.screen, &mut self.out);
        self.tui.cancel();
        if self.native() {
            self.native_shutdown();
            self.out.extend_from_slice(NATIVE_CLOSE);
        }
        self.flush();
    }
}

fn agent_usage() -> ! {
    eprintln!(
        "usage: tns agent <claude|codex|pi|opencode> HOST [--cwd DIR] [--resume ID] [-- AGENT_ARGS...]\n\
         \x20      tns agent <agent> --local [...]        run the agent on this machine\n\n\
         Claude and Codex use Pi locally, connected to the remote agent over SSH.\n\
         Install Pi, then add this extension: pi install /path/to/tns\n\
         --bridge     JSON-lines transport for the Pi extension\n\
         --legacy-ui  use the original TNS interface\n\
         Pi and OpenCode backends retain the original interface."
    );
    std::process::exit(2)
}

fn parse_agent_args(argv: &[String]) -> agent::AgentArgs {
    let mut kind = None;
    let mut host = None;
    let mut a = agent::AgentArgs { kind: agent::proto::Kind::Claude, host: String::new(), local: false, cwd: None, resume: None, extra: Vec::new() };
    let mut it = argv.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--local" => a.local = true,
            "--cwd" => a.cwd = Some(it.next().cloned().unwrap_or_else(|| agent_usage())),
            "--resume" => a.resume = Some(it.next().cloned().unwrap_or_else(|| agent_usage())),
            "--" => {
                a.extra = it.cloned().collect();
                break;
            }
            "-h" | "--help" => agent_usage(),
            s if kind.is_none() => kind = agent::proto::Kind::parse(s).or_else(|| agent_usage()),
            s if host.is_none() && !s.starts_with('-') => host = Some(s.to_string()),
            _ => agent_usage(),
        }
    }
    a.kind = kind.unwrap_or_else(|| agent_usage());
    match host {
        Some(h) if !a.local && !h.starts_with('-') => a.host = h,
        Some(_) => agent_usage(),
        None if a.local => {}
        None => agent_usage(),
    }
    a
}

fn main() {
    let raw: Vec<String> = std::env::args().collect();
    if raw.get(1).map(|s| s.as_str()) == Some("agent") {
        let mut bridge = false;
        let mut legacy = false;
        let mut extra = false;
        let argv: Vec<String> = raw[2..].iter().filter(|s| {
            if s.as_str() == "--" { extra = true; }
            if !extra && s.as_str() == "--bridge" { bridge = true; false }
            else if !extra && s.as_str() == "--legacy-ui" { legacy = true; false }
            else { true }
        }).cloned().collect();
        let a = parse_agent_args(&argv);
        let result = if bridge { agent::bridge::run(a) }
            else if !legacy && matches!(a.kind, agent::proto::Kind::Claude | agent::proto::Kind::Codex) { agent::bridge::launch_pi(a) }
            else { agent::run(a) };
        if let Err(e) = result {
            eprintln!("tns agent: {}", e);
            std::process::exit(1);
        }
        return;
    }
    if raw.get(1).map(|s| s.as_str()) == Some("setup") {
        let code = match setup::run(&raw[2..]) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("tns setup: {}", e);
                1
            }
        };
        std::process::exit(code);
    }
    let args = parse_args();
    if let Some((c, r)) = args.dump {
        dump_screen(c, r);
        return;
    }
    if let Some(p) = &args.bench {
        bench(p);
        return;
    }
    if std::env::var_os("TERM").is_none() {
        std::env::set_var("TERM", "xterm-256color");
    }
    let dir = cache_dir();
    let _ = fs::create_dir_all(&dir);
    let logf: Option<File> = if args.debug { OpenOptions::new().create(true).append(true).open(dir.join("debug.log")).ok() } else { None };
    let size = term_size();
    let session_id = format!("{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0));
    let transport = if args.ssh { Transport::Ssh } else if args.mosh_client { Transport::MoshClient } else { Transport::Mosh };
    let shell_path = match &args.shell {
        Some(s) => s.clone(),
        None => match remote::detect_shell(&args.host) {
            Ok(s) if !s.is_empty() => s,
            Ok(_) => "sh".into(),
            Err(e) => {
                eprintln!("tns: cannot reach {}: {} (is ssh key auth set up? try `tns setup {}`)", args.host, e, args.host);
                std::process::exit(1);
            }
        },
    };
    let shell = Shell::from_name(&shell_path);
    if args.debug {
        eprintln!("tns: remote shell {} ({})", shell_path, shell.name());
    }
    let info = match remote::prepare(&args.host, &session_id, shell, &shell_path) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("tns: cannot prepare {}: {}", args.host, e);
            std::process::exit(1);
        }
    };
    if transport != Transport::Ssh && !info.mosh_server {
        eprintln!("tns: mosh-server is not installed on {}. Run `tns setup {}` for installation instructions, or use `tns --ssh {}`.", args.host, args.host, args.host);
        remote::cleanup(&args.host, &session_id);
        std::process::exit(1);
    }
    let shared = Arc::new(Shared {
        host: args.host.clone(),
        session_id: session_id.clone(),
        shell,
        cache: Mutex::new(Cache::load(&dir.join(format!("{}.bin", args.host)))),
        stats: Mutex::new(Stats::default()),
        stopping: AtomicBool::new(false),
        probes: Mutex::new(Default::default()),
        probe_cv: Default::default(),
        size: Mutex::new(size),
        cwd: Mutex::new(None),
        log: Mutex::new(logf),
        t0: Instant::now(),
    });
    let event_path = remote::event_file(&session_id);
    let sink = if transport == Transport::Ssh { Sink::Osc } else { Sink::File(event_path.clone()) };
    let sess = match Session::open(&args.host, size.0, size.1, transport, &session_id, &sink, None) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("tns: cannot start {}: {}", if args.ssh { "ssh" } else { "mosh" }, e);
            remote::cleanup(&args.host, &session_id);
            std::process::exit(1);
        }
    };
    let chan = if transport != Transport::Ssh {
        match EventChannel::start(&args.host, &event_path) {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("tns: cannot open event channel: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        None
    };
    let raw = RawMode::enter();
    let mut client = Client {
        shared: shared.clone(),
        args,
        sess,
        chan,
        out_fd: 1,
        out: Vec::with_capacity(65536),
        buf: vec![0; 65536],
        inflight: VecDeque::new(),
        pred: Grid::new(size.0, size.1),
        pred_active: false,
        snap: Grid::new(size.0, size.1),
        overlay: Vec::new(),
        overlay_cursor: false,
        anchor: (0, 0),
        anchor_valid: false,
        prompt_anchor: PromptAnchor::new(Instant::now(), shell.has_hooks()),
        terminal_input: TerminalInput::default(),
        tui: tui::Predictor::default(),
        shell_preview: tui::Predictor::default(),
        last_out: None,
        rtt: 0.08,
        probes_started: false,
        last_save: Instant::now(),
        echo: echo::Predictor::default(),
        echo_ack: 0,
        enter_num: 0,
        shown_num: 0,
        escape: false,
        notice: None,
        started: Instant::now(),
    };
    client.run();
    shared.stop();
    if let Some(c) = &client.chan {
        c.stop();
    }
    remote::cleanup(&shared.host, &session_id);
    drop(raw);
    write_all(1, b"\x1b[0m\r\n");
    let _ = shared.cache.lock().unwrap().save();
    let s = *shared.stats.lock().unwrap();
    let cache = shared.cache.lock().unwrap();
    let _ = writeln!(
        io::stdout(),
        "tns: {} keys, {} predicted ({} confirmed, {} mispredicted), {} unpredicted, learned {} live + {} from {} probes, cache {} entries (~{} KB), rtt ~{:.0} ms; {}",
        s.keys,
        s.predicted,
        s.hit,
        s.miss,
        s.unpredicted,
        s.learned_live,
        s.learned_probe,
        s.probed,
        cache.len(),
        cache.mem_bytes() / 1024,
        client.rtt * 1000.0,
        if client.native() {
            format!("echo {} previewed ({} matched, {} discarded)", client.echo.predicted, client.echo.confirmed, client.echo.discarded)
        } else {
            format!(
                "TUI {} previewed ({} matched, {} discarded); shell {} previewed ({} matched, {} discarded)",
                client.tui.predicted, client.tui.confirmed, client.tui.discarded, client.shell_preview.predicted, client.shell_preview.confirmed, client.shell_preview.discarded
            )
        }
    );
    drop(cache);
    drop(client);
}

#[cfg(test)]
mod client_regressions {
    use super::*;

    fn key(t: Instant, has_snap: bool, expected: Option<Key>) -> Inflight {
        Inflight { unit: [b'a'; 8], ulen: 1, pre_key: 0, t, val: expected, expected, has_snap, seen: false, num: 0 }
    }

    #[test]
    fn single_uncached_key_settles_after_output() {
        let t = Instant::now();
        let keys = VecDeque::from([key(t, true, None)]);
        assert_eq!(inflight_deadline(&keys, None, 0.08), None);
        assert_eq!(inflight_deadline(&keys, Some(t - Duration::from_millis(1)), 0.08), None);
        assert_eq!(inflight_deadline(&keys, Some(t + Duration::from_millis(100)), 0.08), Some(t + Duration::from_millis(140)));
    }

    #[test]
    fn burst_settles_without_per_key_seen_flags() {
        let t = Instant::now();
        let keys = VecDeque::from([key(t, true, None), key(t + Duration::from_millis(20), false, None)]);
        assert_eq!(inflight_deadline(&keys, Some(t + Duration::from_millis(10)), 0.08), None);
        assert_eq!(inflight_deadline(&keys, Some(t + Duration::from_millis(100)), 0.08), Some(t + Duration::from_millis(370)));
        // More output extends quiescence, even after the grace period.
        assert_eq!(inflight_deadline(&keys, Some(t + Duration::from_millis(360)), 0.08), Some(t + Duration::from_millis(400)));
    }

    #[test]
    fn predicted_and_snapshotless_suffixes_get_ack_grace() {
        let t = Instant::now();
        for k in [key(t, true, Some(1)), key(t, false, None)] {
            let keys = VecDeque::from([k]);
            assert_eq!(inflight_deadline(&keys, Some(t + Duration::from_millis(100)), 0.2), Some(t + Duration::from_secs_f64(3.0 * 0.2)));
        }
    }

    #[test]
    fn poll_waits_for_actionable_deadlines_without_rounding_down() {
        let t = Instant::now();
        assert_eq!(poll_timeout_ms(t, []), 500);
        assert_eq!(poll_timeout_ms(t, [t + Duration::from_micros(500)]), 1);
        assert_eq!(poll_timeout_ms(t, [t + Duration::from_millis(310)]), 310);
        assert_eq!(poll_timeout_ms(t, [t - Duration::from_millis(1)]), 0);
        let keys = VecDeque::from([key(t, true, None)]);
        // Expired *old* output cannot arm a zero-timeout response timer.
        assert_eq!(poll_timeout_ms(t, inflight_deadline(&keys, Some(t - Duration::from_secs(1)), 0.08)), 500);
    }

    #[test]
    fn terminal_replies_survive_every_stdin_chunk_boundary() {
        for reply in [b"\x1b[?62;22c".as_slice(), b"\x1b[>0;1;0c", b"\x1b[0n", b"\x1b[12;34R", b"\x1b[?12;34R"] {
            for split in 0..=reply.len() {
                let mut input = TerminalInput::default();
                // The query itself may also span multiple output reads.
                input.output(b"\x1b[");
                input.output(b"6n\x1b[?");
                input.output(b"6n");
                let a = input.input(&reply[..split]);
                let b = input.input(&reply[split..]);
                assert!(!a.user && !b.user, "{reply:?} split at {split}");
                assert!(a.protocol || b.protocol);
                assert!(input.pending.is_empty());
            }
        }
    }

    #[test]
    fn keyboard_escapes_and_reply_shaped_function_keys_are_user_input() {
        for key in [b"\x1b[A".as_slice(), b"\x1b[1;2R", b"\x1bOR", b"\x1b[15~", b"\x1bx", b"\x1b]arbitrary\x07"] {
            for split in 0..=key.len() {
                let mut input = TerminalInput::default();
                let a = input.input(&key[..split]);
                let b = input.input(&key[split..]);
                assert!(a.user || b.user, "{key:?} split at {split}");
                assert!(input.pending.is_empty());
            }
        }
        let mut input = TerminalInput::default();
        input.output(b"\x1b[6n");
        assert!(!input.input(b"\x1b[1;2R").user); // solicited CPR
        assert!(input.input(b"\x1b[1;2R").user); // query already consumed
    }

    #[test]
    fn partial_or_mixed_terminal_input_cannot_anchor_over_typing() {
        let mut input = TerminalInput::default();
        assert!(!input.input(b"\x1b[").user);
        assert!(!input.pending.is_empty()); // blocks anchor until classified
        assert!(input.input(b"A").user);
        assert!(input.pending.is_empty());
        let activity = input.input(b"\x1b[?62;22ca");
        assert!(activity.user); // report + real typing in the same read
        assert!(activity.protocol); // never learn the whole read as a key
        assert!(input.pending.is_empty());
    }

    #[test]
    fn prompt_event_before_screen_waits_for_quiet_output() {
        let t = Instant::now();
        let mut anchor = PromptAnchor::new(t, true);
        let quiet = Duration::from_millis(120);
        assert_eq!(anchor.deadline(Some(t), quiet), None); // hooks must arrive
        anchor.prompt(t + Duration::from_millis(100), false);
        assert_eq!(anchor.deadline(None, quiet), None);
        assert_eq!(anchor.deadline(Some(t + Duration::from_millis(200)), quiet), Some(t + Duration::from_millis(320)));
        // A second prompt frame restarts the quiet window.
        assert_eq!(anchor.deadline(Some(t + Duration::from_millis(260)), quiet), Some(t + Duration::from_millis(380)));
    }

    #[test]
    fn prompt_event_after_screen_uses_recent_candidate_with_grace() {
        let t = Instant::now();
        let mut anchor = PromptAnchor::new(t, true);
        anchor.prompt(t + Duration::from_millis(500), false);
        assert_eq!(anchor.deadline(Some(t + Duration::from_millis(10)), Duration::from_millis(120)), Some(t + Duration::from_millis(850)));
        // If the event was actually early, new output replaces that fallback.
        assert_eq!(anchor.deadline(Some(t + Duration::from_millis(600)), Duration::from_millis(120)), Some(t + Duration::from_millis(720)));
    }

    #[test]
    fn prompt_does_not_reuse_old_command_output_or_spin() {
        let t = Instant::now();
        let mut anchor = PromptAnchor::new(t, true);
        let quiet = Duration::from_millis(120);
        anchor.prompt(t + Duration::from_millis(100), false);
        // Even a recent screen from the previous command epoch is ineligible.
        assert_eq!(anchor.deadline(Some(t - Duration::from_millis(10)), quiet), None);
        anchor.prompt(t + Duration::from_secs(2), false);
        // Old output within this epoch is not an unbounded fallback either.
        let deadline = anchor.deadline(Some(t), quiet);
        assert_eq!(deadline, None);
        assert_eq!(poll_timeout_ms(t + Duration::from_secs(3), deadline), 500);
    }

    #[test]
    fn typing_cancels_anchoring_even_when_event_arrives_later() {
        let t = Instant::now();
        for event_first in [true, false] {
            let mut anchor = PromptAnchor::new(t, true);
            if event_first {
                anchor.prompt(t, false);
            }
            anchor.input();
            if !event_first {
                anchor.prompt(t + Duration::from_millis(100), false);
            }
            let deadline = anchor.deadline(Some(t + Duration::from_millis(200)), Duration::from_millis(120));
            assert_eq!(deadline, None);
            assert_eq!(poll_timeout_ms(t + Duration::from_secs(1), deadline), 500);
            anchor.resize(t + Duration::from_millis(300));
            assert_eq!(anchor.deadline(Some(t + Duration::from_millis(400)), Duration::from_millis(120)), None);
        }
    }

    #[test]
    fn in_band_and_hookless_prompts_keep_ordered_output_requirement() {
        let t = Instant::now();
        let mut anchor = PromptAnchor::new(t, true);
        anchor.prompt(t + Duration::from_millis(100), true);
        assert_eq!(anchor.deadline(Some(t), OUTPUT_QUIET), None);
        assert_eq!(anchor.deadline(Some(t + Duration::from_millis(100)), OUTPUT_QUIET), Some(t + Duration::from_millis(140)));
        let anchor = PromptAnchor::new(t, false);
        assert_eq!(anchor.deadline(Some(t - Duration::from_millis(1)), Duration::from_millis(250)), None);
        assert_eq!(anchor.deadline(Some(t), Duration::from_millis(250)), Some(t + Duration::from_millis(250)));
    }
}
