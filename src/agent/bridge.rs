//! Line-delimited JSON transport for the Pi extension. No terminal ownership.
use super::*;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Input {
    Control { id: String, action: String, #[serde(default)] value: serde_json::Value },
    Send { text: String },
    Interrupt,
    Answer { id: String, reply: Decision },
    Close,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Decision { Allow, AllowAlways, Deny }

pub fn run(args: AgentArgs) -> io::Result<()> {
    let (tx, rx) = channel();
    let input_tx = tx.clone();
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            match line {
                Ok(line) => { if input_tx.send(Msg::Input(line)).is_err() { break; } }
                _ => break,
            }
        }
        let _ = input_tx.send(Msg::InputClosed);
    });
    let (mut link, sid) = connect(&args, args.resume.as_deref(), tx)?;
    let result = (|| -> io::Result<()> {
        let mut stdout = io::stdout().lock();
        let mut emit = |event: Ev| -> io::Result<()> {
            serde_json::to_writer(&mut stdout, &event)?;
            writeln!(stdout)?;
            stdout.flush()
        };
        if let Some(id) = sid { emit(Ev::SessionId(id))?; }
        let mut busy = false;
        for msg in rx {
            match msg {
                Msg::Input(line) => {
                    match serde_json::from_str::<Input>(&line) {
                        Ok(Input::Control { id, action, value }) => {
                            if busy {
                                emit(Ev::ControlResult { id, result: serde_json::Value::Null, error: Some("wait for the remote turn to finish".into()) })?;
                            } else {
                                let mut events = Vec::new();
                                link.control(&id, &action, value, &mut events)?;
                                for event in events { emit(event)?; }
                            }
                        }
                        Ok(Input::Send { text }) if !busy => {
                            link.send_message(&text)?;
                            busy = true;
                        }
                        Ok(Input::Send { .. }) => emit(Ev::Error("a turn is already running".into()))?,
                        Ok(Input::Interrupt) => link.interrupt()?,
                        Ok(Input::Answer { id, reply }) => link.answer(&id, match reply {
                            Decision::Allow => Reply::Allow,
                            Decision::AllowAlways => Reply::AllowAlways,
                            Decision::Deny => Reply::Deny,
                        })?,
                        Ok(Input::Close) => break,
                        Err(e) => emit(Ev::Error(format!("invalid bridge command: {e}")))?,
                    }
                }
                Msg::InputClosed => { let _ = link.interrupt(); break; }
                Msg::Line(line) => {
                    let mut events = Vec::new();
                    link.handle(&line, &mut events);
                    for event in events {
                        if matches!(event, Ev::TurnDone(_) | Ev::Error(_)) { busy = false; }
                        emit(event)?;
                    }
                }
                Msg::Stderr(line) => eprintln!("{line}"),
                Msg::Exit(reason) => {
                    emit(Ev::Error(format!("agent connection closed: {reason}")))?;
                    break;
                }
            }
        }
        Ok(())
    })();
    link.close();
    result
}

/// Pi owns the UI; it loads the installed package or the development extension.
pub fn launch_pi(args: AgentArgs) -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    let config = serde_json::json!({
        "kind": args.kind.name(), "host": args.host, "local": args.local,
        "cwd": args.cwd, "resume": args.resume, "extra": args.extra,
    });
    let mut cmd = Command::new("pi");
    cmd.env("TNS_AGENT_CONFIG", config.to_string());
    cmd.env("TNS_BIN", std::env::current_exe()?);
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("extensions/pi/index.ts");
    if let Some(path) = std::env::var_os("TNS_PI_EXTENSION") {
        cmd.arg("-e").arg(path);
    } else if source.is_file() {
        cmd.arg("-e").arg(source);
    }
    cmd.args(["--provider", "tns", "--model", "remote", "--no-tools"]);
    Err(cmd.exec())
}
