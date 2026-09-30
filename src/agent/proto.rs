//! Structured protocols of the supported agents, normalised to one event
//! stream.  Each adapter turns the agent's stdout lines (or SSE records) into
//! `Ev`s and turns our actions into the lines to write back.
//!
//! Verified against: Claude Code 2.1 (`--input-format stream-json`), Codex
//! 0.156 (`codex app-server`, JSON lines), pi 0.85 (`--mode rpc`), opencode
//! 1.18 (`opencode serve` HTTP + SSE).

use serde_json::{json, Value};

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Ev {
    SessionId(String),
    Status(String),
    TextDelta(String),
    TextDone,
    Thinking,
    ToolStart { id: String, name: String, summary: String },
    ToolDone { id: String, ok: bool, summary: String },
    Permission { id: String, title: String, detail: String },
    TurnDone(Option<String>),
    Error(String),
    ControlResult { id: String, result: Value, error: Option<String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    Allow,
    AllowAlways,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Claude,
    Codex,
    Pi,
    OpenCode,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "claude" | "claude-code" => Some(Kind::Claude),
            "codex" => Some(Kind::Codex),
            "pi" => Some(Kind::Pi),
            "opencode" => Some(Kind::OpenCode),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Kind::Claude => "claude",
            Kind::Codex => "codex",
            Kind::Pi => "pi",
            Kind::OpenCode => "opencode",
        }
    }
}

/// A stdio-speaking agent.
pub trait Adapter: Send {
    /// Configure the session and return its command (cwd is handled by the caller).
    fn argv(&mut self, resume: Option<&str>, extra: &[String]) -> Vec<String>;
    /// Lines to send right after start.
    fn on_start(&mut self) -> Vec<String> {
        Vec::new()
    }
    fn on_line(&mut self, line: &str, out: &mut Vec<Ev>, send: &mut Vec<String>);
    fn send_message(&mut self, text: &str) -> Vec<String>;
    fn interrupt(&mut self) -> Vec<String>;
    fn answer(&mut self, id: &str, reply: Reply) -> Vec<String>;
    fn control(&mut self, id: &str, _action: &str, _value: Value, out: &mut Vec<Ev>) -> Vec<String> {
        out.push(Ev::ControlResult { id: id.into(), result: Value::Null, error: Some("control not supported by this agent".into()) });
        Vec::new()
    }
}

fn compact(v: &Value, max: usize) -> String {
    let s = match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    let s: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if s.chars().count() > max {
        let cut: String = s.chars().take(max).collect();
        format!("{}…", cut)
    } else {
        s
    }
}

fn tool_summary(name: &str, input: &Value) -> String {
    // The interesting field first, for the tools people see most.
    for key in ["command", "file_path", "path", "pattern", "query", "url", "description"] {
        if let Some(v) = input.get(key) {
            if v.is_string() {
                return compact(v, 160);
            }
        }
    }
    let _ = name;
    compact(input, 160)
}

// --------------------------------------------------------------------------- Claude Code

pub struct Claude {
    req: u64,
    blocks: Vec<(String, String)>, // index -> (type, tool_use_id)
    streamed_text: bool,
    turn_has_text: bool,
    prompts: std::collections::HashMap<String, (Value, Value)>, // request id -> (input, permission_suggestions)
    controls: std::collections::HashMap<String, (String, String)>,
}

impl Claude {
    pub fn new() -> Claude {
        Claude { req: 0, blocks: Vec::new(), streamed_text: false, turn_has_text: false, prompts: Default::default(), controls: Default::default() }
    }
    fn next_req(&mut self) -> String {
        self.req += 1;
        format!("tns-{}", self.req)
    }
}

impl Adapter for Claude {
    fn argv(&mut self, resume: Option<&str>, extra: &[String]) -> Vec<String> {
        let mut v: Vec<String> = [
            "claude",
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--permission-prompt-tool",
            "stdio",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        if let Some(id) = resume {
            v.push("--resume".into());
            v.push(id.into());
        }
        v.extend(extra.iter().cloned());
        v
    }

    fn on_line(&mut self, line: &str, out: &mut Vec<Ev>, _send: &mut Vec<String>) {
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return,
        };
        let t = v["type"].as_str().unwrap_or("");
        if t == "control_response" {
            let response = &v["response"];
            if let Some((id, action)) = self.controls.remove(response["request_id"].as_str().unwrap_or("")) {
                let data = &response["response"];
                let result = match action.as_str() {
                    "models" => data["models"].clone(),
                    "commands" => data["commands"].clone(),
                    _ => data.clone(),
                };
                let error = (response["subtype"] == "error").then(|| response["error"].as_str().unwrap_or("agent rejected control request").to_string());
                out.push(Ev::ControlResult { id, result, error });
            }
            return;
        }
        match t {
            "system" => {
                if v["subtype"] == "init" {
                    if let Some(id) = v["session_id"].as_str() {
                        out.push(Ev::SessionId(id.to_string()));
                    }
                    out.push(Ev::Status(format!("{} · {}", v["model"].as_str().unwrap_or("claude"), v["cwd"].as_str().unwrap_or(""))));
                } else if v["subtype"] == "status" {
                    if let Some(s) = v["status"].as_str() {
                        out.push(Ev::Status(s.to_string()));
                    }
                }
            }
            "stream_event" => {
                let e = &v["event"];
                match e["type"].as_str().unwrap_or("") {
                    "message_start" => {
                        self.blocks.clear();
                        self.streamed_text = false;
                    }
                    "content_block_start" => {
                        let idx = e["index"].as_u64().unwrap_or(0) as usize;
                        let cb = &e["content_block"];
                        let ty = cb["type"].as_str().unwrap_or("").to_string();
                        if ty == "thinking" {
                            out.push(Ev::Thinking);
                        }
                        let id = cb["id"].as_str().unwrap_or("").to_string();
                        if self.blocks.len() <= idx {
                            self.blocks.resize(idx + 1, (String::new(), String::new()));
                        }
                        self.blocks[idx] = (ty, id);
                    }
                    "content_block_delta" => {
                        let d = &e["delta"];
                        if d["type"] == "text_delta" {
                            if let Some(s) = d["text"].as_str() {
                                self.streamed_text = true;
                                self.turn_has_text = true;
                                out.push(Ev::TextDelta(s.to_string()));
                            }
                        }
                    }
                    "content_block_stop" => {
                        let idx = e["index"].as_u64().unwrap_or(0) as usize;
                        if self.blocks.get(idx).map_or(false, |b| b.0 == "text") {
                            out.push(Ev::TextDone);
                        }
                    }
                    _ => {}
                }
            }
            "assistant" => {
                if let Some(content) = v["message"]["content"].as_array() {
                    for c in content {
                        match c["type"].as_str().unwrap_or("") {
                            "tool_use" => out.push(Ev::ToolStart {
                                id: c["id"].as_str().unwrap_or("").to_string(),
                                name: c["name"].as_str().unwrap_or("tool").to_string(),
                                summary: tool_summary(c["name"].as_str().unwrap_or(""), &c["input"]),
                            }),
                            "text" if !self.streamed_text => {
                                if let Some(s) = c["text"].as_str() {
                                    self.turn_has_text = true;
                                    out.push(Ev::TextDelta(s.to_string()));
                                    out.push(Ev::TextDone);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            "user" => {
                if let Some(content) = v["message"]["content"].as_array() {
                    for c in content {
                        if c["type"] == "tool_result" {
                            let text = match &c["content"] {
                                Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join(" "),
                                other => compact(other, 200),
                            };
                            out.push(Ev::ToolDone {
                                id: c["tool_use_id"].as_str().unwrap_or("").to_string(),
                                ok: !c["is_error"].as_bool().unwrap_or(false),
                                summary: compact(&Value::String(text), 200),
                            });
                        }
                    }
                }
            }
            "result" => {
                // Headless slash commands can return their output only in result.
                if !self.turn_has_text && !v["is_error"].as_bool().unwrap_or(false) {
                    if let Some(text) = v["result"].as_str().filter(|s| !s.is_empty()) {
                        out.push(Ev::TextDelta(text.into()));
                        out.push(Ev::TextDone);
                    }
                }
                let mut note = Vec::new();
                if let Some(c) = v["total_cost_usd"].as_f64() {
                    note.push(format!("${:.3}", c));
                }
                if let Some(ms) = v["duration_ms"].as_u64() {
                    note.push(format!("{:.1}s", ms as f64 / 1000.0));
                }
                if v["is_error"].as_bool().unwrap_or(false) {
                    let msg = compact(&v["result"], 300);
                    out.push(Ev::Error(if msg.is_empty() { "interrupted".into() } else { msg }));
                }
                out.push(Ev::TurnDone(if note.is_empty() { None } else { Some(note.join(" · ")) }));
            }
            "control_request" => {
                let r = &v["request"];
                let id = v["request_id"].as_str().unwrap_or("").to_string();
                if r["subtype"] == "can_use_tool" {
                    let name = r["tool_name"].as_str().unwrap_or("tool").to_string();
                    self.prompts.insert(id.clone(), (r["input"].clone(), r["permission_suggestions"].clone()));
                    let detail = match r["description"].as_str() {
                        Some(d) if !d.is_empty() => compact(&Value::String(d.to_string()), 160),
                        _ => tool_summary(&name, &r["input"]),
                    };
                    out.push(Ev::Permission { id, title: name, detail });
                }
            }
            _ => {}
        }
    }

    fn control(&mut self, id: &str, action: &str, value: Value, out: &mut Vec<Ev>) -> Vec<String> {
        let request = match action {
            "models" | "commands" => json!({"subtype":"initialize"}),
            "model" if value.as_str().is_some_and(|s| !s.is_empty()) => json!({"subtype":"set_model","model":value}),
            _ => {
                out.push(Ev::ControlResult { id: id.into(), result: Value::Null, error: Some("unsupported Claude control".into()) });
                return Vec::new();
            }
        };
        let wire_id = self.next_req();
        self.controls.insert(wire_id.clone(), (id.into(), action.into()));
        vec![json!({"type":"control_request","request_id":wire_id,"request":request}).to_string()]
    }

    fn send_message(&mut self, text: &str) -> Vec<String> {
        self.turn_has_text = false;
        vec![json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":text}]}}).to_string()]
    }

    fn interrupt(&mut self) -> Vec<String> {
        let id = self.next_req();
        vec![json!({"type":"control_request","request_id":id,"request":{"subtype":"interrupt"}}).to_string()]
    }

    fn answer(&mut self, id: &str, reply: Reply) -> Vec<String> {
        let (input, suggestions) = self.prompts.remove(id).unwrap_or((json!({}), Value::Null));
        let response = match reply {
            Reply::Allow => json!({"behavior":"allow","updatedInput":input}),
            Reply::AllowAlways => {
                let mut r = json!({"behavior":"allow","updatedInput":input});
                if suggestions.is_array() {
                    r["updatedPermissions"] = suggestions;
                }
                r
            }
            Reply::Deny => json!({"behavior":"deny","message":"denied by user"}),
        };
        vec![json!({"type":"control_response","response":{"subtype":"success","request_id":id,"response":response}}).to_string()]
    }
}

// --------------------------------------------------------------------------- Codex

pub struct Codex {
    next_id: u64,
    thread: Option<String>,
    turn: Option<String>,
    resume: Option<String>,
    cwd: Option<String>,
    queued: Vec<String>,
    turn_req: Option<u64>,
    legacy_approval: std::collections::HashMap<String, bool>, // request id -> uses approved/denied words
    model: Option<String>,
    controls: std::collections::HashMap<u64, String>,
    queued_controls: Vec<(String, String, Value)>,
}

impl Codex {
    pub fn new(cwd: Option<String>) -> Codex {
        Codex { next_id: 0, thread: None, turn: None, resume: None, cwd, queued: Vec::new(), turn_req: None, legacy_approval: Default::default(), model: None, controls: Default::default(), queued_controls: Vec::new() }
    }
    fn req(&mut self, method: &str, params: Value) -> (u64, String) {
        self.next_id += 1;
        (self.next_id, json!({"id": self.next_id, "method": method, "params": params}).to_string())
    }
    fn start_turn(&mut self, text: &str) -> String {
        let thread = self.thread.clone().unwrap_or_default();
        if text == "/compact" {
            return self.req("thread/compact/start", json!({"threadId":thread})).1;
        }
        let mut params = json!({"threadId": thread, "input": [{"type":"text","text":text}]});
        if let Some(model) = &self.model { params["model"] = json!(model); }
        let (id, line) = self.req("turn/start", params);
        self.turn_req = Some(id);
        line
    }
}

impl Adapter for Codex {
    fn argv(&mut self, resume: Option<&str>, extra: &[String]) -> Vec<String> {
        // app-server resumes through the handshake, not a CLI argument.
        self.resume = resume.map(str::to_string);
        let mut v = vec!["codex".to_string(), "app-server".to_string()];
        v.extend(extra.iter().cloned());
        v
    }

    fn on_start(&mut self) -> Vec<String> {
        let (_, init) = self.req("initialize", json!({"clientInfo": {"name": "tns", "title": "tns", "version": env!("CARGO_PKG_VERSION")}}));
        vec![init]
    }

    fn on_line(&mut self, line: &str, out: &mut Vec<Ev>, send: &mut Vec<String>) {
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return,
        };
        // responses to our requests
        if let (Some(id), true) = (v["id"].as_u64(), v.get("method").is_none()) {
            if let Some(client_id) = self.controls.remove(&id) {
                out.push(Ev::ControlResult { id: client_id, result: v["result"].clone(), error: v.get("error").map(|e| e["message"].as_str().unwrap_or("agent rejected control request").to_string()) });
                return;
            }
            if let Some(err) = v.get("error") {
                out.push(Ev::Error(compact(&err["message"], 300)));
                return;
            }
            if id == 1 {
                let line = match self.resume.clone() {
                    Some(t) => self.req("thread/resume", json!({"threadId": t})).1,
                    None => {
                        let mut p = json!({});
                        if let Some(c) = &self.cwd {
                            p["cwd"] = Value::String(c.clone());
                        }
                        self.req("thread/start", p).1
                    }
                };
                send.push(line);
            } else if id == 2 {
                if let Some(t) = v["result"]["thread"]["id"].as_str() {
                    self.thread = Some(t.to_string());
                    out.push(Ev::SessionId(t.to_string()));
                    out.push(Ev::Status("ready".into()));
                    for (id, action, value) in std::mem::take(&mut self.queued_controls) {
                        send.extend(self.control(&id, &action, value, out));
                    }
                    for q in std::mem::take(&mut self.queued) {
                        let l = self.start_turn(&q);
                        send.push(l);
                    }
                }
            } else if Some(id) == self.turn_req {
                if let Some(t) = v["result"]["turn"]["id"].as_str() {
                    self.turn = Some(t.to_string());
                }
            }
            return;
        }
        let method = v["method"].as_str().unwrap_or("");
        let p = &v["params"];
        // server requests (have an id): approvals
        if let Some(rid) = v.get("id") {
            let rid_s = rid.to_string();
            match method {
                "item/commandExecution/requestApproval" | "execCommandApproval" => {
                    self.legacy_approval.insert(rid_s.clone(), method == "execCommandApproval");
                    let cmd = match &p["command"] {
                        Value::Array(a) => a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" "),
                        other => compact(other, 200),
                    };
                    let mut detail = cmd;
                    if let Some(r) = p["reason"].as_str() {
                        detail = format!("{}  ({})", detail, r);
                    }
                    out.push(Ev::Permission { id: rid_s, title: "run command".into(), detail });
                }
                "item/fileChange/requestApproval" | "applyPatchApproval" => {
                    self.legacy_approval.insert(rid_s.clone(), method == "applyPatchApproval");
                    let files = match &p["changes"] {
                        Value::Array(a) => a.iter().filter_map(|c| c["path"].as_str()).collect::<Vec<_>>().join(", "),
                        Value::Object(o) => o.keys().cloned().collect::<Vec<_>>().join(", "),
                        _ => String::new(),
                    };
                    out.push(Ev::Permission { id: rid_s, title: "edit files".into(), detail: compact(&Value::String(files), 200) });
                }
                "item/permissions/requestApproval" => {
                    out.push(Ev::Permission { id: rid_s, title: "grant permissions".into(), detail: compact(&p["reason"], 200) });
                }
                "item/tool/requestUserInput" | "mcpServer/elicitation/request" => {
                    // cannot render arbitrary forms yet: decline politely
                    send.push(json!({"id": rid, "error": {"code": -32000, "message": "not supported by tns"}}).to_string());
                }
                _ => {}
            }
            return;
        }
        match method {
            "turn/started" => {
                self.turn = p["turn"]["id"].as_str().map(str::to_string);
            }
            "item/agentMessage/delta" => {
                if let Some(d) = p["delta"].as_str() {
                    out.push(Ev::TextDelta(d.to_string()));
                }
            }
            "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => out.push(Ev::Thinking),
            "item/started" | "item/completed" => {
                let item = &p["item"];
                let ty = item["type"].as_str().unwrap_or("");
                let id = item["id"].as_str().unwrap_or("").to_string();
                let started = method == "item/started";
                match ty {
                    "agentMessage" if !started => out.push(Ev::TextDone),
                    "commandExecution" => {
                        let cmd = match &item["command"] {
                            Value::Array(a) => a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" "),
                            other => compact(other, 160),
                        };
                        if started {
                            out.push(Ev::ToolStart { id, name: "shell".into(), summary: cmd });
                        } else {
                            let ok = item["status"].as_str() != Some("failed") && item["exitCode"].as_i64().unwrap_or(0) == 0;
                            out.push(Ev::ToolDone { id, ok, summary: compact(&item["aggregatedOutput"], 200) });
                        }
                    }
                    "fileChange" => {
                        let files = match &item["changes"] {
                            Value::Array(a) => a.iter().filter_map(|c| c["path"].as_str()).collect::<Vec<_>>().join(", "),
                            _ => String::new(),
                        };
                        if started {
                            out.push(Ev::ToolStart { id, name: "edit".into(), summary: files });
                        } else {
                            out.push(Ev::ToolDone { id, ok: item["status"].as_str() != Some("failed"), summary: String::new() });
                        }
                    }
                    "mcpToolCall" => {
                        let name = format!("{}", item["tool"].as_str().unwrap_or("mcp"));
                        if started {
                            out.push(Ev::ToolStart { id, name, summary: compact(&item["arguments"], 160) });
                        } else {
                            out.push(Ev::ToolDone { id, ok: item["status"].as_str() != Some("failed"), summary: compact(&item["result"], 200) });
                        }
                    }
                    "webSearch" if started => out.push(Ev::ToolStart { id, name: "web search".into(), summary: compact(&item["query"], 160) }),
                    "webSearch" => out.push(Ev::ToolDone { id, ok: true, summary: String::new() }),
                    _ => {}
                }
            }
            "turn/completed" => {
                self.turn = None;
                if p["turn"]["status"] == "failed" {
                    out.push(Ev::Error(p["turn"]["error"]["message"].as_str().unwrap_or("remote turn failed").into()));
                }
                out.push(Ev::TurnDone(None));
            }
            "thread/tokenUsage/updated" => {
                if let Some(t) = p["tokenUsage"]["total"]["totalTokens"].as_u64() {
                    out.push(Ev::Status(format!("{} tokens", t)));
                }
            }
            "error" => out.push(Ev::Error(compact(&p["message"], 300))),
            _ => {}
        }
    }

    fn control(&mut self, id: &str, action: &str, value: Value, out: &mut Vec<Ev>) -> Vec<String> {
        if self.thread.is_none() {
            self.queued_controls.push((id.into(), action.into(), value));
            return Vec::new();
        }
        match action {
            "model" if value.as_str().is_some_and(|s| !s.is_empty()) => {
                self.model = value.as_str().map(str::to_string);
                out.push(Ev::ControlResult { id: id.into(), result: json!({"model":self.model,"applies":"next turn"}), error: None });
                Vec::new()
            }
            "models" => {
                let (wire_id, line) = self.req("model/list", json!({"cursor":value.as_str(),"limit":100}));
                self.controls.insert(wire_id, id.into());
                vec![line]
            }
            _ => {
                out.push(Ev::ControlResult { id: id.into(), result: Value::Null, error: Some("unsupported Codex control; use /tns native for native slash commands".into()) });
                Vec::new()
            }
        }
    }

    fn send_message(&mut self, text: &str) -> Vec<String> {
        if self.thread.is_none() {
            self.queued.push(text.to_string());
            return Vec::new();
        }
        vec![self.start_turn(text)]
    }

    fn interrupt(&mut self) -> Vec<String> {
        match (&self.thread, &self.turn) {
            (Some(t), Some(u)) => vec![self.req("turn/interrupt", json!({"threadId": t, "turnId": u})).1],
            _ => Vec::new(),
        }
    }

    fn answer(&mut self, id: &str, reply: Reply) -> Vec<String> {
        let legacy = self.legacy_approval.remove(id).unwrap_or(false);
        let decision = match (reply, legacy) {
            (Reply::Allow, false) => "accept",
            (Reply::AllowAlways, false) => "acceptForSession",
            (Reply::Deny, false) => "decline",
            (Reply::Allow, true) | (Reply::AllowAlways, true) => "approved",
            (Reply::Deny, true) => "denied",
        };
        let id_v: Value = serde_json::from_str(id).unwrap_or(Value::String(id.to_string()));
        vec![json!({"id": id_v, "result": {"decision": decision}}).to_string()]
    }
}

// --------------------------------------------------------------------------- pi

pub struct Pi {
    req: u64,
}

impl Pi {
    pub fn new() -> Pi {
        Pi { req: 0 }
    }
}

impl Adapter for Pi {
    fn on_start(&mut self) -> Vec<String> {
        vec![json!({"id":"tns-state","type":"get_state"}).to_string()]
    }

    fn argv(&mut self, resume: Option<&str>, extra: &[String]) -> Vec<String> {
        let mut v = vec!["pi".to_string(), "--mode".into(), "rpc".into()];
        if let Some(id) = resume {
            v.push("--session".into());
            v.push(id.into());
        }
        v.extend(extra.iter().cloned());
        v
    }

    fn on_line(&mut self, line: &str, out: &mut Vec<Ev>, send: &mut Vec<String>) {
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return,
        };
        let t = v["type"].as_str().unwrap_or("");
        match t {
            "session" | "session_start" | "session_header" => {
                if let Some(id) = v["sessionId"].as_str().or(v["id"].as_str()) {
                    out.push(Ev::SessionId(id.to_string()));
                }
            }
            "message_update" => {
                let e = &v["assistantMessageEvent"];
                match e["type"].as_str().unwrap_or("") {
                    "text_delta" => {
                        if let Some(d) = e["delta"].as_str() {
                            out.push(Ev::TextDelta(d.to_string()));
                        }
                    }
                    "thinking_delta" => out.push(Ev::Thinking),
                    _ => {}
                }
            }
            "message_end" => {
                if v["message"]["role"] == "assistant" {
                    out.push(Ev::TextDone);
                }
            }
            "tool_execution_start" => out.push(Ev::ToolStart {
                id: v["toolCallId"].as_str().unwrap_or("").to_string(),
                name: v["toolName"].as_str().unwrap_or("tool").to_string(),
                summary: tool_summary(v["toolName"].as_str().unwrap_or(""), &v["args"]),
            }),
            "tool_execution_end" => {
                let text = match &v["result"]["content"] {
                    Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join(" "),
                    other => compact(other, 200),
                };
                out.push(Ev::ToolDone {
                    id: v["toolCallId"].as_str().unwrap_or("").to_string(),
                    ok: !v["isError"].as_bool().unwrap_or(false),
                    summary: compact(&Value::String(text), 200),
                });
            }
            "agent_settled" => out.push(Ev::TurnDone(None)),
            "response" => {
                if v["success"] == false {
                    out.push(Ev::Error(compact(&v["error"], 300)));
                } else if v["command"] == "get_state" {
                    let d = &v["data"];
                    let id = d["sessionId"].as_str().or(d["session"]["id"].as_str()).or(d["sessionFile"].as_str());
                    if let Some(id) = id {
                        out.push(Ev::SessionId(id.to_string()));
                    }
                    if let Some(m) = d["model"]["id"].as_str().or(d["model"].as_str()) {
                        out.push(Ev::Status(m.to_string()));
                    }
                }
            }
            "extension_ui_request" => {
                let id = v["id"].as_str().unwrap_or("").to_string();
                match v["method"].as_str().unwrap_or("") {
                    "confirm" => out.push(Ev::Permission { id, title: compact(&v["title"], 60), detail: compact(&v["message"], 200) }),
                    "notify" => {}
                    _ => send.push(json!({"type":"extension_ui_response","id":id,"cancelled":true}).to_string()),
                }
            }
            _ => {}
        }
    }

    fn send_message(&mut self, text: &str) -> Vec<String> {
        self.req += 1;
        vec![json!({"id": format!("tns-{}", self.req), "type":"prompt","message":text}).to_string()]
    }

    fn interrupt(&mut self) -> Vec<String> {
        vec![json!({"type":"abort"}).to_string()]
    }

    fn answer(&mut self, id: &str, reply: Reply) -> Vec<String> {
        vec![json!({"type":"extension_ui_response","id":id,"confirmed": reply != Reply::Deny}).to_string()]
    }
}

// --------------------------------------------------------------------------- opencode (SSE records)

/// opencode is driven over HTTP; this adapter only turns SSE `data:` JSON
/// into events.  Sending is done by the HTTP link in `mod.rs`.
pub struct OpenCode {
    pub session: Option<String>,
    assistant_msgs: std::collections::HashSet<String>,
    tools_seen: std::collections::HashSet<String>,
    reasoning_parts: std::collections::HashSet<String>,
}

impl OpenCode {
    pub fn new() -> OpenCode {
        OpenCode { session: None, assistant_msgs: Default::default(), tools_seen: Default::default(), reasoning_parts: Default::default() }
    }

    pub fn on_event(&mut self, data: &str, out: &mut Vec<Ev>) {
        let v: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return,
        };
        let p = &v["properties"];
        if let Some(sid) = p["sessionID"].as_str() {
            if self.session.as_deref() != Some(sid) {
                return; // another session on the same server
            }
        }
        match v["type"].as_str().unwrap_or("") {
            "message.updated" => {
                if p["info"]["role"] == "assistant" {
                    if let Some(id) = p["info"]["id"].as_str() {
                        self.assistant_msgs.insert(id.to_string());
                    }
                }
            }
            "message.part.delta" => {
                let from_assistant = p["messageID"].as_str().map_or(false, |m| self.assistant_msgs.contains(m));
                let reasoning = p["partID"].as_str().map_or(false, |id| self.reasoning_parts.contains(id));
                if p["field"] == "text" && from_assistant {
                    if reasoning {
                        out.push(Ev::Thinking);
                    } else if let Some(d) = p["delta"].as_str() {
                        out.push(Ev::TextDelta(d.to_string()));
                    }
                }
            }
            "message.part.updated" => {
                let part = &p["part"];
                let mid = part["messageID"].as_str().unwrap_or("");
                if !self.assistant_msgs.contains(mid) {
                    return;
                }
                match part["type"].as_str().unwrap_or("") {
                    "text" => {
                        if part["time"]["end"].is_number() {
                            out.push(Ev::TextDone);
                        }
                    }
                    "reasoning" => {
                        if let Some(id) = part["id"].as_str() {
                            self.reasoning_parts.insert(id.to_string());
                        }
                        out.push(Ev::Thinking);
                    }
                    "tool" => {
                        let id = part["id"].as_str().unwrap_or("").to_string();
                        let name = part["tool"].as_str().unwrap_or("tool").to_string();
                        let st = &part["state"];
                        let has_input = st["input"].as_object().map_or(false, |o| !o.is_empty());
                        match st["status"].as_str().unwrap_or("") {
                            "running" | "pending" if has_input => {
                                if self.tools_seen.insert(id.clone()) {
                                    out.push(Ev::ToolStart { id, name: name.clone(), summary: tool_summary(&name, &st["input"]) });
                                }
                            }
                            "running" | "pending" => {}
                            "completed" => {
                                if self.tools_seen.insert(id.clone()) {
                                    out.push(Ev::ToolStart { id: id.clone(), name: name.clone(), summary: tool_summary(&name, &st["input"]) });
                                }
                                out.push(Ev::ToolDone { id, ok: true, summary: compact(&st["output"], 200) });
                            }
                            "error" => {
                                if self.tools_seen.insert(id.clone()) {
                                    out.push(Ev::ToolStart { id: id.clone(), name: name.clone(), summary: tool_summary(&name, &st["input"]) });
                                }
                                out.push(Ev::ToolDone { id, ok: false, summary: compact(&st["error"], 200) });
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            "permission.asked" | "permission.updated" => {
                let id = p["id"].as_str().unwrap_or("").to_string();
                let title = p["title"].as_str().or(p["type"].as_str()).unwrap_or("permission").to_string();
                out.push(Ev::Permission { id, title, detail: compact(&p["metadata"], 200) });
            }
            "session.idle" => out.push(Ev::TurnDone(None)),
            "session.error" => out.push(Ev::Error(compact(&p["error"], 300))),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_streams_text_and_tools() {
        let mut a = Claude::new();
        let mut out = Vec::new();
        let mut send = Vec::new();
        a.on_line(r#"{"type":"system","subtype":"init","session_id":"s1","model":"m","cwd":"/x"}"#, &mut out, &mut send);
        a.on_line(r#"{"type":"stream_event","event":{"type":"message_start"}}"#, &mut out, &mut send);
        a.on_line(r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#, &mut out, &mut send);
        a.on_line(r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}}"#, &mut out, &mut send);
        a.on_line(r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#, &mut out, &mut send);
        a.on_line(r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#, &mut out, &mut send);
        a.on_line(r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"a b","is_error":false}]}}"#, &mut out, &mut send);
        a.on_line(r#"{"type":"result","total_cost_usd":0.01,"duration_ms":1200}"#, &mut out, &mut send);
        let kinds: Vec<String> = out.iter().map(|e| format!("{:?}", e).split(['(', ' ']).next().unwrap().to_string()).collect();
        assert_eq!(kinds, ["SessionId", "Status", "TextDelta", "TextDone", "ToolStart", "ToolDone", "TurnDone"]);
        assert!(a.send_message("x")[0].contains("\"type\":\"user\""));
        assert!(a.answer("r1", Reply::Deny)[0].contains("deny"));
    }

    #[test]
    fn codex_handshake_and_deltas() {
        let mut c = Codex::new(Some("/w".into()));
        c.argv(None, &[]);
        let mut out = Vec::new();
        let mut send = Vec::new();
        assert!(c.on_start()[0].contains("initialize"));
        assert!(c.send_message("hello").is_empty()); // queued until the thread exists
        c.on_line(r#"{"id":1,"result":{}}"#, &mut out, &mut send);
        assert!(send[0].contains("thread/start") && send[0].contains("/w"));
        c.on_line(r#"{"id":2,"result":{"thread":{"id":"th1"}}}"#, &mut out, &mut send);
        assert!(send[1].contains("turn/start") && send[1].contains("hello"));
        c.on_line(r#"{"method":"item/agentMessage/delta","params":{"delta":"yo"}}"#, &mut out, &mut send);
        c.on_line(r#"{"id":9,"method":"item/commandExecution/requestApproval","params":{"command":["rm","-rf","x"]}}"#, &mut out, &mut send);
        assert!(matches!(out.last(), Some(Ev::Permission { id, .. }) if id == "9"));
        assert_eq!(c.answer("9", Reply::Deny)[0], r#"{"id":9,"result":{"decision":"decline"}}"#);
    }

    #[test]
    fn codex_resumes_on_initial_connect_and_reconnect() {
        // Reconnection constructs a fresh adapter with the saved session ID.
        for _ in 0..2 {
            let mut c = Codex::new(None);
            assert_eq!(c.argv(Some("saved-thread"), &[]), ["codex", "app-server"]);
            c.on_start();
            assert!(c.send_message("continue").is_empty());
            let (mut out, mut send) = (Vec::new(), Vec::new());
            c.on_line(r#"{"id":1,"result":{}}"#, &mut out, &mut send);
            let request: Value = serde_json::from_str(&send[0]).unwrap();
            assert_eq!(request["method"], "thread/resume");
            assert_eq!(request["params"]["threadId"], "saved-thread");
            c.on_line(r#"{"id":2,"result":{"thread":{"id":"saved-thread"}}}"#, &mut out, &mut send);
            let turn: Value = serde_json::from_str(&send[1]).unwrap();
            assert_eq!(turn["method"], "turn/start");
            assert_eq!(turn["params"]["threadId"], "saved-thread");
        }
    }

    #[test]
    fn opencode_filters_by_session() {
        let mut o = OpenCode::new();
        o.session = Some("s".into());
        let mut out = Vec::new();
        o.on_event(r#"{"type":"message.updated","properties":{"sessionID":"s","info":{"id":"m1","role":"assistant"}}}"#, &mut out);
        o.on_event(r#"{"type":"message.part.delta","properties":{"sessionID":"s","messageID":"m1","field":"text","delta":"a"}}"#, &mut out);
        o.on_event(r#"{"type":"message.part.delta","properties":{"sessionID":"other","messageID":"m9","field":"text","delta":"b"}}"#, &mut out);
        o.on_event(r#"{"type":"session.idle","properties":{"sessionID":"s"}}"#, &mut out);
        assert!(matches!(&out[..], [Ev::TextDelta(a), Ev::TurnDone(None)] if a == "a"));
    }
}
