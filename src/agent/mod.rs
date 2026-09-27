//! `tns agent`: run a coding agent headless on the remote and render it
//! locally.  The input box, scrolling and permission prompts are local; only
//! submitted messages, interrupts and answers cross the wire, and the agent's
//! output arrives as structured events (token deltas, tool calls) rather than
//! screen cells.

pub mod proto;
pub mod ui;

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use proto::{Adapter, Claude, Codex, Ev, Kind, OpenCode, Pi, Reply};
use ui::View;

pub struct AgentArgs {
    pub kind: Kind,
    pub host: String,
    pub local: bool,
    pub cwd: Option<String>,
    pub resume: Option<String>,
    pub extra: Vec<String>,
}

enum Msg {
    Line(String),
    Stderr(String),
    Exit(String),
}

fn ssh_base(host: &str) -> Vec<String> {
    crate::session::ssh_base(host)
}

fn cwd_ok(c: &str) -> bool {
    c.chars().all(|ch| ch.is_ascii_alphanumeric() || "/._~-".contains(ch))
}

/// Spawn `argv` locally or on the host, with line readers feeding `tx`.
fn spawn(args: &AgentArgs, argv: Vec<String>, tx: Sender<Msg>) -> io::Result<(Child, ChildStdin)> {
    let mut cmd;
    if args.local {
        cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        if let Some(c) = &args.cwd {
            cmd.current_dir(c);
        }
    } else {
        let base = ssh_base(&args.host);
        cmd = Command::new(&base[0]);
        cmd.args(&base[1..]);
        if let Some(c) = &args.cwd {
            if !cwd_ok(c) {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "cwd may only contain letters, digits and /._~-"));
            }
            cmd.args(["cd", c, "&&", "exec"]);
        }
        cmd.args(&argv);
    }
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let tx2 = tx.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).split(b'\n') {
            match line {
                Ok(l) => {
                    if tx2.send(Msg::Line(String::from_utf8_lossy(&l).trim_end_matches('\r').to_string())).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx2.send(Msg::Exit("process closed its output".into()));
    });
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if tx.send(Msg::Stderr(line)).is_err() {
                break;
            }
        }
    });
    Ok((child, stdin))
}

trait Link {
    fn handle(&mut self, line: &str, out: &mut Vec<Ev>);
    fn send_message(&mut self, text: &str) -> io::Result<()>;
    fn interrupt(&mut self) -> io::Result<()>;
    fn answer(&mut self, id: &str, r: Reply) -> io::Result<()>;
    fn close(&mut self);
}

struct StdioLink {
    adapter: Box<dyn Adapter>,
    stdin: ChildStdin,
    child: Child,
}

impl StdioLink {
    fn write(&mut self, lines: Vec<String>) -> io::Result<()> {
        for l in lines {
            self.stdin.write_all(l.as_bytes())?;
            self.stdin.write_all(b"\n")?;
        }
        self.stdin.flush()
    }
}

impl Link for StdioLink {
    fn handle(&mut self, line: &str, out: &mut Vec<Ev>) {
        let mut send = Vec::new();
        self.adapter.on_line(line, out, &mut send);
        if let Err(e) = self.write(send) {
            out.push(Ev::Error(format!("write failed: {}", e)));
        }
    }
    fn send_message(&mut self, text: &str) -> io::Result<()> {
        let l = self.adapter.send_message(text);
        self.write(l)
    }
    fn interrupt(&mut self) -> io::Result<()> {
        let l = self.adapter.interrupt();
        self.write(l)
    }
    fn answer(&mut self, id: &str, r: Reply) -> io::Result<()> {
        let l = self.adapter.answer(id, r);
        self.write(l)
    }
    fn close(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// --------------------------------------------------------------------------- opencode over HTTP

fn http(port: u16, method: &str, path: &str, body: Option<&str>) -> io::Result<(u16, String)> {
    let mut s = TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    let body = body.unwrap_or("");
    let req = format!(
        "{} {} HTTP/1.0\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        method,
        path,
        body.len(),
        body
    );
    s.write_all(req.as_bytes())?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status: u16 = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let chunked = head.to_ascii_lowercase().contains("transfer-encoding: chunked");
    let body = if chunked { dechunk(rest) } else { rest.to_string() };
    Ok((status, body))
}

fn dechunk(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    loop {
        let (size_line, after) = match rest.split_once("\r\n") {
            Some(x) => x,
            None => break,
        };
        let n = usize::from_str_radix(size_line.trim().split(';').next().unwrap_or("0"), 16).unwrap_or(0);
        if n == 0 || after.len() < n {
            break;
        }
        out.push_str(&after[..n]);
        rest = after[n..].trim_start_matches("\r\n");
    }
    out
}

struct HttpLink {
    oc: OpenCode,
    port: u16,
    session: String,
    child: Child,
}

impl HttpLink {
    fn start(args: &AgentArgs, tx: Sender<Msg>) -> io::Result<HttpLink> {
        let port: u16 = 20000 + (std::process::id() % 40000) as u16;
        let mut argv = vec!["opencode".to_string(), "serve".into(), "--port".into(), port.to_string(), "--hostname".into(), "127.0.0.1".into()];
        argv.extend(args.extra.iter().cloned());
        let mut cmd;
        if args.local {
            cmd = Command::new(&argv[0]);
            cmd.args(&argv[1..]);
            if let Some(c) = &args.cwd {
                cmd.current_dir(c);
            }
        } else {
            let base = ssh_base(&args.host);
            cmd = Command::new(&base[0]);
            cmd.args(&base[1..]);
            cmd.args(["-L", &format!("127.0.0.1:{}:127.0.0.1:{}", port, port)]);
            if let Some(c) = &args.cwd {
                if !cwd_ok(c) {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, "cwd may only contain letters, digits and /._~-"));
                }
                cmd.args(["cd", c, "&&", "exec"]);
            }
            cmd.args(&argv);
        }
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        let child = cmd.spawn()?;
        // wait for the server
        let t0 = Instant::now();
        loop {
            if let Ok((200, _)) = http(port, "GET", "/session/status", None) {
                break;
            }
            if let Ok((200, _)) = http(port, "GET", "/doc", None) {
                break;
            }
            if t0.elapsed() > Duration::from_secs(30) {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "opencode server did not come up"));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        let session = match &args.resume {
            Some(s) => s.clone(),
            None => {
                let (_, body) = http(port, "POST", "/session", Some("{}"))?;
                let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                v["id"].as_str().ok_or_else(|| io::Error::new(io::ErrorKind::Other, format!("no session id in {}", body)))?.to_string()
            }
        };
        // SSE reader
        let tx2 = tx.clone();
        std::thread::spawn(move || {
            let run = || -> io::Result<()> {
                let mut s = TcpStream::connect(("127.0.0.1", port))?;
                s.write_all(b"GET /event HTTP/1.0\r\nHost: 127.0.0.1\r\nAccept: text/event-stream\r\nConnection: close\r\n\r\n")?;
                let r = BufReader::new(s);
                for line in r.lines() {
                    let line = line?;
                    if let Some(d) = line.strip_prefix("data:") {
                        if tx2.send(Msg::Line(d.trim().to_string())).is_err() {
                            break;
                        }
                    }
                }
                Ok(())
            };
            let _ = run();
            let _ = tx2.send(Msg::Exit("event stream closed".into()));
        });
        let mut oc = OpenCode::new();
        oc.session = Some(session.clone());
        Ok(HttpLink { oc, port, session, child })
    }
}

impl Link for HttpLink {
    fn handle(&mut self, line: &str, out: &mut Vec<Ev>) {
        self.oc.on_event(line, out);
    }
    fn send_message(&mut self, text: &str) -> io::Result<()> {
        let body = serde_json::json!({"parts":[{"type":"text","text":text}]}).to_string();
        let (st, b) = http(self.port, "POST", &format!("/session/{}/prompt_async", self.session), Some(&body))?;
        if st >= 300 {
            return Err(io::Error::new(io::ErrorKind::Other, format!("HTTP {}: {}", st, b.chars().take(200).collect::<String>())));
        }
        Ok(())
    }
    fn interrupt(&mut self) -> io::Result<()> {
        http(self.port, "POST", &format!("/session/{}/abort", self.session), Some("{}")).map(|_| ())
    }
    fn answer(&mut self, id: &str, r: Reply) -> io::Result<()> {
        let resp = match r {
            Reply::Allow => "once",
            Reply::AllowAlways => "always",
            Reply::Deny => "reject",
        };
        http(self.port, "POST", &format!("/session/{}/permissions/{}", self.session, id), Some(&format!("{{\"response\":\"{}\"}}", resp))).map(|_| ())
    }
    fn close(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn connect(args: &AgentArgs, resume: Option<&str>, tx: Sender<Msg>) -> io::Result<(Box<dyn Link>, Option<String>)> {
    match args.kind {
        Kind::OpenCode => {
            let l = HttpLink::start(&AgentArgs { resume: resume.map(|s| s.to_string()), ..clone_args(args) }, tx)?;
            let sid = l.session.clone();
            Ok((Box::new(l), Some(sid)))
        }
        kind => {
            let mut adapter: Box<dyn Adapter> = match kind {
                Kind::Claude => Box::new(Claude::new()),
                Kind::Codex => Box::new(Codex::new(if args.local { None } else { args.cwd.clone() })),
                Kind::Pi => Box::new(Pi::new()),
                Kind::OpenCode => unreachable!(),
            };
            let argv = adapter.argv(resume, &args.extra);
            let (child, mut stdin) = spawn(args, argv, tx)?;
            for l in adapter.on_start() {
                stdin.write_all(l.as_bytes())?;
                stdin.write_all(b"\n")?;
            }
            stdin.flush()?;
            Ok((Box::new(StdioLink { adapter, stdin, child }), None))
        }
    }
}

fn clone_args(a: &AgentArgs) -> AgentArgs {
    AgentArgs { kind: a.kind, host: a.host.clone(), local: a.local, cwd: a.cwd.clone(), resume: a.resume.clone(), extra: a.extra.clone() }
}

pub fn run(args: AgentArgs) -> io::Result<()> {
    let (tx, rx): (Sender<Msg>, Receiver<Msg>) = channel();
    let host_label = if args.local { "local".to_string() } else { args.host.clone() };
    let mut view = View::new(args.kind.name(), &host_label);
    // raw mode first, so keys typed while the agent starts are not eaten by
    // the line discipline; they are delivered once the loop starts
    let mut terminal = ratatui::init();
    terminal.draw(|f| view.draw(f))?;
    let (mut link, sid) = match connect(&args, args.resume.as_deref(), tx.clone()) {
        Ok(x) => x,
        Err(e) => {
            ratatui::restore();
            return Err(e);
        }
    };
    if let Some(s) = sid {
        view.transcript.session = Some(s);
        view.transcript.status = "ready".into();
    }
    let mut disconnected = false;
    let mut last_stderr: Option<String> = None;
    let result = (|| -> io::Result<()> {
        loop {
            terminal.draw(|f| view.draw(f))?;
            // agent events
            while let Ok(m) = rx.try_recv() {
                match m {
                    Msg::Line(l) => {
                        let mut evs = Vec::new();
                        link.handle(&l, &mut evs);
                        for e in evs {
                            if matches!(e, Ev::TurnDone(_)) {
                                view.toast = None;
                            }
                            view.transcript.apply(e);
                        }
                    }
                    Msg::Stderr(s) => last_stderr = Some(s),
                    Msg::Exit(why) => {
                        if !disconnected {
                            disconnected = true;
                            let mut msg = format!("connection closed ({})", why);
                            if let Some(s) = &last_stderr {
                                msg.push_str(&format!(": {}", s));
                            }
                            view.transcript.apply(Ev::Error(msg));
                            view.transcript.running = false;
                            view.transcript.status = "disconnected · ctrl-r: reconnect".into();
                        }
                    }
                }
            }
            if !event::poll(Duration::from_millis(50))? {
                continue;
            }
            let ev = event::read()?;
            let key = match ev {
                Event::Key(k) if k.kind != KeyEventKind::Release => k,
                Event::Resize(_, _) => continue,
                _ => continue,
            };
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            let alt = key.modifiers.contains(KeyModifiers::ALT);
            if !(ctrl && key.code == KeyCode::Char('c')) {
                view.quit_armed = false;
                view.toast = None;
            }
            // permission prompt
            if let Some(p) = view.transcript.pending.take() {
                let reply = match key.code {
                    KeyCode::Char('y') | KeyCode::Enter => Some(Reply::Allow),
                    KeyCode::Char('a') => Some(Reply::AllowAlways),
                    KeyCode::Char('n') | KeyCode::Esc => Some(Reply::Deny),
                    _ => None,
                };
                match reply {
                    Some(r) => {
                        if let Err(e) = link.answer(&p.id, r) {
                            view.transcript.apply(Ev::Error(format!("answer failed: {}", e)));
                        }
                        view.transcript.status = "thinking".into();
                    }
                    None => view.transcript.pending = Some(p),
                }
                continue;
            }
            match (key.code, ctrl, alt) {
                (KeyCode::Char('c'), true, _) => {
                    if view.transcript.running && !disconnected {
                        let _ = link.interrupt();
                        view.toast = Some("interrupting".into());
                    } else if view.quit_armed {
                        return Ok(());
                    } else {
                        view.quit_armed = true;
                        view.toast = Some("ctrl-c again to quit".into());
                    }
                }
                (KeyCode::Char('d'), true, _) if view.editor.is_empty() => return Ok(()),
                (KeyCode::Char('r'), true, _) if disconnected => {
                    link.close();
                    let sid = view.transcript.session.clone();
                    match connect(&args, sid.as_deref(), tx.clone()) {
                        Ok((l, s)) => {
                            link = l;
                            if s.is_some() {
                                view.transcript.session = s;
                            }
                            disconnected = false;
                            view.transcript.status = "reconnected".into();
                        }
                        Err(e) => view.transcript.apply(Ev::Error(format!("reconnect failed: {}", e))),
                    }
                }
                (KeyCode::Esc, _, _) => {
                    if view.transcript.running && !disconnected {
                        let _ = link.interrupt();
                        view.toast = Some("interrupting".into());
                    } else if view.transcript.scroll_from_bottom > 0 {
                        view.transcript.scroll_from_bottom = 0;
                    }
                }
                (KeyCode::Enter, false, true) | (KeyCode::Char('j'), true, _) => view.editor.insert('\n'),
                (KeyCode::Enter, _, false) => {
                    if view.editor.is_empty() {
                        continue;
                    }
                    if disconnected {
                        view.toast = Some("disconnected: ctrl-r to reconnect".into());
                        continue;
                    }
                    let text = view.editor.take();
                    view.transcript.push_user(&text);
                    if let Err(e) = link.send_message(&text) {
                        view.transcript.apply(Ev::Error(format!("send failed: {}", e)));
                        view.transcript.running = false;
                    }
                }
                (KeyCode::Backspace, _, _) => view.editor.backspace(),
                (KeyCode::Delete, _, _) => view.editor.delete(),
                (KeyCode::Left, _, _) => view.editor.left(),
                (KeyCode::Right, _, _) => view.editor.right(),
                (KeyCode::Home, _, _) | (KeyCode::Char('a'), true, _) => view.editor.home(),
                (KeyCode::End, _, _) | (KeyCode::Char('e'), true, _) => {
                    if view.transcript.scroll_from_bottom > 0 && view.editor.is_empty() {
                        view.transcript.scroll_from_bottom = 0;
                    } else {
                        view.editor.end();
                    }
                }
                (KeyCode::Char('w'), true, _) => view.editor.delete_word(),
                (KeyCode::Char('u'), true, _) => view.editor.kill_line(),
                (KeyCode::Up, _, _) if !view.editor.multiline() => view.editor.history_up(),
                (KeyCode::Down, _, _) if !view.editor.multiline() => view.editor.history_down(),
                (KeyCode::PageUp, _, _) => view.scroll(10),
                (KeyCode::PageDown, _, _) => view.scroll(-10),
                (KeyCode::Char(c), false, false) => view.editor.insert(c),
                _ => {}
            }
        }
    })();
    ratatui::restore();
    link.close();
    if let Some(s) = &view.transcript.session {
        let mut hint = format!("tns agent {} {}", args.kind.name(), if args.local { "--local".to_string() } else { args.host.clone() });
        if let Some(c) = &args.cwd {
            hint.push_str(&format!(" --cwd {}", c));
        }
        println!("session {}  (resume with: {} --resume {})", s, hint, s);
    }
    result
}
