//! Everything that runs on the remote host, for any login shell.
//!
//! Rules that keep this shell-agnostic:
//! - scripts are sent to `ssh HOST sh -s` on stdin, so the login shell never
//!   parses them (POSIX sh exists everywhere);
//! - the interactive shell is started by an uploaded `run` script, so the only
//!   inline remote command is `sh /tmp/tns-ID/run ENCODED_CWD`, which contains
//!   no quotes and means the same thing in fish, bash, zsh and sh;
//! - per-shell hook files are uploaded, never passed on a command line.

use std::io::{self, Write};
use std::process::{Command, Stdio};

/// Which shell to start on the remote and how to hook it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    /// Anything else: started as is, no hooks; the client falls back to
    /// deriving prompt/exec events from Enter + quiescence.
    Other,
}

impl Shell {
    pub fn from_name(name: &str) -> Shell {
        match name.rsplit('/').next().unwrap_or(name) {
            "bash" => Shell::Bash,
            "zsh" => Shell::Zsh,
            "fish" => Shell::Fish,
            _ => Shell::Other,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Shell::Bash => "bash",
            Shell::Zsh => "zsh",
            Shell::Fish => "fish",
            Shell::Other => "other",
        }
    }
    pub fn has_hooks(self) -> bool {
        self != Shell::Other
    }
}

/// Where the hooks report events: in-band OSC marks (ssh transport) or a
/// file on the remote followed over a side channel (mosh transport).
#[derive(Clone, Debug)]
pub enum Sink {
    Osc,
    File(String),
}

/// What `prepare` learned about the host.
#[derive(Clone, Debug, Default)]
pub struct HostInfo {
    pub login_shell: String,
    pub mosh_server: bool,
    pub utf8_locales: usize,
    pub pkg: String,
    pub os: String,
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

/// Run a POSIX script on the host via `sh -s`; returns stdout.
pub fn run_script(host: &str, script: &str, extra: &[&str]) -> io::Result<String> {
    let base = ssh_base(host);
    let mut cmd = Command::new(&base[0]);
    cmd.args(&base[1..]).args(extra).arg("sh").arg("-s");
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn()?;
    {
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(script.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() && out.stdout.is_empty() {
        return Err(io::Error::new(io::ErrorKind::Other, format!("ssh exited with {}", out.status)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Percent-encode so the result is safe as an argument in any shell.
pub fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'/' || b == b'.' || b == b'_' || b == b'-' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// The remote directory holding this session's files.
pub fn session_dir(id: &str) -> String {
    format!("/tmp/tns-{}", id)
}

/// A POSIX heredoc-safe embedding: the delimiter never appears in our files.
fn heredoc(path: &str, body: &str) -> String {
    format!("cat > {} <<'TNS_EOF'\n{}\nTNS_EOF\n", path, body)
}

/// The hook file for `shell`, reporting to `sink`.  Each shell's prompt hook
/// emits: X <url(last command)> (if it changed), D <url(cwd)>, P.
pub fn hooks(shell: Shell, sink: &Sink) -> String {
    // emitter: $1 = event letter, $2 = already url-encoded payload
    let emit_posix = match sink {
        Sink::Osc => "__tns_emit() { case \"$1\" in P) printf '\\033]133;A\\a';; X) printf '\\033]7770;%s\\a' \"$2\";; D) printf '\\033]7771;%s\\a' \"$2\";; esac; }".to_string(),
        Sink::File(p) => format!("__tns_emit() {{ printf '%s %s\\n' \"$1\" \"$2\" >> {}; }}", p),
    };
    // url-encode with shell builtins only (bash/zsh substring syntax)
    // Percent-encode using only syntax bash and zsh share: ${s:$i:1} slicing
    // and $(printf '%%%02X' "'$c") for the hex (the leading quote makes printf
    // use the byte's numeric value).  Avoids bash-only `printf -v` and `+=`.
    let url_posix = "__tns_url() { local s=$1 out= c i=0 n; n=${#s}; while [ $i -lt $n ]; do c=${s:$i:1}; case $c in [a-zA-Z0-9/._-]) out=$out$c ;; *) out=$out$(printf '%%%02X' \"'$c\") ;; esac; i=$((i+1)); done; printf '%s' \"$out\"; }";
    match shell {
        Shell::Bash => format!(
            "[ -f \"$HOME/.bashrc\" ] && . \"$HOME/.bashrc\"\n{emit}\n{url}\n\
             __tns_last=$(HISTTIMEFORMAT= builtin history 1 2>/dev/null | sed 's/^ *[0-9]* *//')\n\
             __tns_prompt() {{ local last; last=$(HISTTIMEFORMAT= builtin history 1 2>/dev/null | sed 's/^ *[0-9]* *//'); \
             if [ \"$last\" != \"$__tns_last\" ]; then __tns_last=$last; __tns_emit X \"$(__tns_url \"$last\")\"; fi; \
             __tns_emit D \"$(__tns_url \"$PWD\")\"; __tns_emit P; }}\n\
             PROMPT_COMMAND=\"__tns_prompt${{PROMPT_COMMAND:+;$PROMPT_COMMAND}}\"\n",
            emit = emit_posix,
            url = url_posix
        ),
        Shell::Zsh => format!(
            "ZDOTDIR=$HOME\n[ -f \"$HOME/.zshrc\" ] && . \"$HOME/.zshrc\"\n{emit}\n{url}\n\
             __tns_last=$(fc -ln -1 2>/dev/null | sed 's/^[[:space:]]*//')\n\
             __tns_prompt() {{ local last; last=$(fc -ln -1 2>/dev/null | sed 's/^[[:space:]]*//'); \
             if [ \"$last\" != \"$__tns_last\" ]; then __tns_last=$last; __tns_emit X \"$(__tns_url \"$last\")\"; fi; \
             __tns_emit D \"$(__tns_url \"$PWD\")\"; __tns_emit P; }}\n\
             autoload -Uz add-zsh-hook; add-zsh-hook precmd __tns_prompt\n",
            emit = emit_posix,
            url = url_posix
        ),
        Shell::Fish => {
            let emit = match sink {
                Sink::Osc => "function __tns_emit; switch $argv[1]; case P; printf \"\\e]133;A\\a\"; case X; printf \"\\e]7770;%s\\a\" $argv[2]; case D; printf \"\\e]7771;%s\\a\" $argv[2]; end; end".to_string(),
                Sink::File(p) => format!("function __tns_emit; printf \"%s %s\\n\" $argv[1] $argv[2] >> {}; end", p),
            };
            format!(
                "{emit}\n\
                 function __tns_prompt --on-event fish_prompt; __tns_emit D (string escape --style=url -- $PWD); __tns_emit P; end\n\
                 function __tns_post --on-event fish_postexec; __tns_emit X (string escape --style=url -- $argv[1]); end\n",
                emit = emit
            )
        }
        Shell::Other => String::new(),
    }
}

/// The `run` script: `sh DIR/run SINK [ENCODED_CWD]` decodes the cwd and
/// execs the hooked shell with the hooks for SINK (`osc` or `file`).
fn run_script_body(dir: &str, shell: Shell, shell_path: &str) -> String {
    let launch = match shell {
        Shell::Bash => "exec bash --rcfile $d/init.bash -i".to_string(),
        Shell::Zsh => "ZDOTDIR=$d exec zsh -i".to_string(),
        Shell::Fish => "exec fish -C \"source $d/init.fish\"".to_string(),
        Shell::Other => format!("exec {}", shell_path),
    };
    format!(
        "#!/bin/sh\n\
         d={dir}/$1\n\
         if [ -n \"$2\" ]; then c=$(printf '%b' \"$(printf '%s' \"$2\" | sed 's/%/\\\\x/g')\"); cd \"$c\" 2>/dev/null; fi\n\
         {launch}\n"
    )
}

/// Path of the event file for the file sink of session `id`.
pub fn event_file(id: &str) -> String {
    format!("{}/events", session_dir(id))
}

/// The setup script: uploads the session files (hooks for both sinks) and
/// reports host facts.
fn prepare_script(id: &str, shell: Shell, shell_path: &str) -> String {
    let dir = session_dir(id);
    let mut s = format!("umask 077; mkdir -p {dir}/osc {dir}/file && cd {dir} || exit 1\n");
    for (sub, sink) in [("osc", Sink::Osc), ("file", Sink::File(event_file(id)))] {
        match shell {
            Shell::Bash => s += &heredoc(&format!("{}/init.bash", sub), &hooks(shell, &sink)),
            Shell::Zsh => {
                s += &heredoc(&format!("{}/.zshrc", sub), &hooks(shell, &sink));
                s += &heredoc(&format!("{}/.zshenv", sub), "[ -f \"$HOME/.zshenv\" ] && . \"$HOME/.zshenv\"");
            }
            Shell::Fish => s += &heredoc(&format!("{}/init.fish", sub), &hooks(shell, &sink)),
            Shell::Other => {}
        }
    }
    s += &heredoc("run", &run_script_body(&dir, shell, shell_path));
    s += &format!("touch {}\n", event_file(id));
    s += "echo SHELL=$SHELL\n\
          echo MOSH=$(command -v mosh-server)\n\
          echo UTF8=$(locale -a 2>/dev/null | grep -ic utf)\n\
          for p in apt-get dnf pacman apk zypper brew; do if command -v $p >/dev/null 2>&1; then echo PKG=$p; break; fi; done\n\
          echo OS=$(uname -s)\n";
    s
}

fn parse_info(out: &str) -> HostInfo {
    let mut h = HostInfo::default();
    for l in out.lines() {
        if let Some(v) = l.strip_prefix("SHELL=") {
            h.login_shell = v.trim().to_string();
        } else if let Some(v) = l.strip_prefix("MOSH=") {
            h.mosh_server = !v.trim().is_empty();
        } else if let Some(v) = l.strip_prefix("UTF8=") {
            h.utf8_locales = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = l.strip_prefix("PKG=") {
            h.pkg = v.trim().to_string();
        } else if let Some(v) = l.strip_prefix("OS=") {
            h.os = v.trim().to_string();
        }
    }
    h
}

/// Ask the host which login shell it has (one round trip).
pub fn detect_shell(host: &str) -> io::Result<String> {
    let out = run_script(host, "echo $SHELL\n", &[])?;
    Ok(out.trim().to_string())
}

/// Upload this session's files for `shell` and return what the host reported.
pub fn prepare(host: &str, id: &str, shell: Shell, shell_path: &str) -> io::Result<HostInfo> {
    let out = run_script(host, &prepare_script(id, shell, shell_path), &[])?;
    Ok(parse_info(&out))
}

/// Remove the session directory (hooks and event file) on exit.
pub fn cleanup(host: &str, id: &str) {
    let _ = run_script(host, &format!("rm -rf {}\n", session_dir(id)), &[]);
}

/// The inline command that starts the hooked shell (no quotes: safe in any
/// login shell).  `cwd` is percent-encoded and decoded by the run script.
pub fn launch_argv(id: &str, sink: &Sink, cwd: Option<&str>) -> Vec<String> {
    let mut v = vec!["sh".to_string(), format!("{}/run", session_dir(id)), if matches!(sink, Sink::Osc) { "osc".into() } else { "file".into() }];
    if let Some(c) = cwd {
        v.push(url_encode(c));
    }
    v
}

/// Recent history, newest first, for the shell in use.
pub fn history(host: &str, shell: Shell, limit: usize) -> Vec<String> {
    let script = match shell {
        Shell::Fish => "fish -c history 2>/dev/null\n".to_string(),
        Shell::Bash => "f=$(bash -ic 'echo $HISTFILE' 2>/dev/null); [ -f \"$f\" ] || f=$HOME/.bash_history; [ -f \"$f\" ] && tail -n 2000 \"$f\"\n".to_string(),
        Shell::Zsh => "f=$(zsh -ic 'echo $HISTFILE' 2>/dev/null); [ -f \"$f\" ] || f=$HOME/.zsh_history; [ -f \"$f\" ] && tail -n 2000 \"$f\"\n".to_string(),
        Shell::Other => "for f in $HOME/.bash_history $HOME/.zsh_history $HOME/.history $HOME/.sh_history; do [ -f \"$f\" ] && tail -n 2000 \"$f\" && break; done; fish -c history 2>/dev/null\n".to_string(),
    };
    let out = run_script(host, &script, &[]).unwrap_or_default();
    let mut lines: Vec<String> = out
        .lines()
        .map(|l| {
            // zsh extended history: ": 1700000000:0;command"
            let l = if l.starts_with(": ") {
                l.splitn(2, ';').nth(1).unwrap_or("")
            } else {
                l
            };
            l.trim().to_string()
        })
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    if shell != Shell::Fish {
        lines.reverse(); // files are oldest first; fish prints newest first
    }
    let mut seen = std::collections::HashSet::new();
    lines.retain(|l| seen.insert(l.clone()));
    lines.truncate(limit);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_is_shell_safe() {
        let e = url_encode("/tmp/it's a \"dir\" $x\\y");
        assert!(e.chars().all(|c| c.is_ascii_alphanumeric() || "/._-%".contains(c)));
        assert_eq!(url_encode("/home/me"), "/home/me");
    }

    #[test]
    fn hooks_have_no_heredoc_delimiter() {
        for sh in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            for sink in [Sink::Osc, Sink::File("/tmp/x".into())] {
                assert!(!hooks(sh, &sink).contains("TNS_EOF"));
            }
        }
        assert!(hooks(Shell::Bash, &Sink::Osc).contains("PROMPT_COMMAND"));
        assert!(hooks(Shell::Zsh, &Sink::Osc).contains("add-zsh-hook precmd"));
        assert!(hooks(Shell::Fish, &Sink::File("/tmp/e".into())).contains(">> /tmp/e"));
    }

    #[test]
    fn launch_has_no_quotes() {
        let v = launch_argv("abc", &Sink::Osc, Some("/home/me/dir with space"));
        assert!(v.iter().all(|a| !a.contains('\'') && !a.contains('"') && !a.contains(' ')));
    }

    #[test]
    fn info_parses() {
        let h = parse_info("SHELL=/usr/bin/fish\nMOSH=/usr/bin/mosh-server\nUTF8=2\nPKG=apt-get\nOS=Linux\n");
        assert_eq!(h.login_shell, "/usr/bin/fish");
        assert!(h.mosh_server);
        assert_eq!(h.pkg, "apt-get");
    }
}
