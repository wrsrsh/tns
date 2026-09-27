//! `tns setup [HOST]`: an interactive wizard that gets a host ready for tns.
//!
//! It picks or defines a host, makes sure you can connect (ssh key or
//! password), installs mosh on the remote if it is missing, checks the login
//! shell and UTF-8 locale, and offers to save an ~/.ssh/config entry.  Every
//! step explains what it will run before it runs it.

use std::fs;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::remote::{self, Shell};

// ----- tiny ANSI helpers (setup output only)
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const CYAN: &str = "\x1b[36m";
const RST: &str = "\x1b[0m";

fn step(n: usize, total: usize, title: &str) {
    println!("\n{}[{}/{}]{} {}{}{}", CYAN, n, total, RST, BOLD, title, RST);
}
fn ok(msg: &str) {
    println!("  {}✓{} {}", GREEN, RST, msg);
}
fn warn(msg: &str) {
    println!("  {}!{} {}", YELLOW, RST, msg);
}
fn info(msg: &str) {
    println!("  {}{}{}", DIM, msg, RST);
}
fn fail(msg: &str) {
    println!("  {}✗{} {}", RED, RST, msg);
}

fn prompt(q: &str, default: Option<&str>) -> String {
    match default {
        Some(d) => print!("  {}{}{} [{}]: ", BOLD, q, RST, d),
        None => print!("  {}{}{}: ", BOLD, q, RST),
    }
    io::stdout().flush().ok();
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line).unwrap_or(0) == 0 {
        // EOF: fall back to the default or empty
        return default.unwrap_or("").to_string();
    }
    let line = line.trim().to_string();
    if line.is_empty() {
        default.unwrap_or("").to_string()
    } else {
        line
    }
}

fn yes(q: &str, default_yes: bool) -> bool {
    let d = if default_yes { "Y/n" } else { "y/N" };
    loop {
        let a = prompt(q, Some(d)).to_ascii_lowercase();
        match a.as_str() {
            "y" | "yes" => return true,
            "n" | "no" => return false,
            _ if a == d.to_ascii_lowercase() => return default_yes,
            _ => {}
        }
    }
}

fn choose(q: &str, options: &[String]) -> usize {
    println!("  {}{}{}", BOLD, q, RST);
    for (i, o) in options.iter().enumerate() {
        println!("    {}{}{}) {}", CYAN, i + 1, RST, o);
    }
    loop {
        let a = prompt("choice", Some("1"));
        if let Ok(n) = a.parse::<usize>() {
            if n >= 1 && n <= options.len() {
                return n - 1;
            }
        }
    }
}

fn ssh_config_path() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".ssh").join("config")
}

/// Host aliases from ~/.ssh/config (skipping wildcard patterns).
fn config_hosts() -> Vec<String> {
    let mut hosts = Vec::new();
    if let Ok(text) = fs::read_to_string(ssh_config_path()) {
        for line in text.lines() {
            let t = line.trim();
            let lower = t.to_ascii_lowercase();
            if let Some(rest) = lower.strip_prefix("host ") {
                let _ = rest;
                for h in t[5..].split_whitespace() {
                    if !h.contains('*') && !h.contains('?') && !hosts.contains(&h.to_string()) {
                        hosts.push(h.to_string());
                    }
                }
            }
        }
    }
    hosts
}

/// Base ssh args for interactive use (no BatchMode, so a password can be typed).
fn ssh_interactive(host: &str) -> Command {
    let mut c = Command::new("ssh");
    c.args([
        "-o",
        "ControlMaster=auto",
        "-o",
        "ControlPath=~/.ssh/tns-%C",
        "-o",
        "ControlPersist=120",
        host,
    ]);
    c
}

/// True if key-based (non-interactive) login already works.
fn key_login_works(host: &str) -> bool {
    Command::new("ssh")
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=8", host, "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn have_command(name: &str) -> bool {
    Command::new("sh").arg("-c").arg(format!("command -v {}", name)).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
}

fn install_mosh_cmd(pkg: &str) -> Option<String> {
    let c = match pkg {
        "apt-get" => "sudo apt-get update && sudo apt-get install -y mosh",
        "dnf" => "sudo dnf install -y mosh",
        "yum" => "sudo yum install -y mosh",
        "pacman" => "sudo pacman -S --noconfirm mosh",
        "apk" => "sudo apk add mosh",
        "zypper" => "sudo zypper install -y mosh",
        "brew" => "brew install mosh",
        _ => return None,
    };
    Some(c.to_string())
}

pub fn run(host_arg: Option<String>) -> io::Result<()> {
    println!("{}{}tns setup{}  — get a host ready for a predictive, roaming shell", BOLD, CYAN, RST);
    let total = 4;

    // ---- 1. pick the host
    step(1, total, "Choose the host");
    let host = match host_arg {
        Some(h) => {
            info(&format!("using {}", h));
            h
        }
        None => {
            let mut opts = config_hosts();
            let type_new = "type a host or user@host".to_string();
            opts.push(type_new.clone());
            let idx = choose("Which host?", &opts);
            if idx == opts.len() - 1 {
                let h = prompt("host or user@host (e.g. me@server.example.com)", None);
                if h.is_empty() {
                    fail("no host given");
                    return Ok(());
                }
                maybe_save_config(&h);
                h
            } else {
                opts[idx].clone()
            }
        }
    };

    // ---- 2. connectivity / auth
    step(2, total, "Connect");
    if key_login_works(&host) {
        ok("ssh key login already works");
    } else {
        info(&format!("cannot log in to {} without a password yet", host));
        let idx = choose(
            "How do you want to authenticate?",
            &["Set up an ssh key (recommended, passwordless from now on)".into(), "Use a password each time".into()],
        );
        if idx == 0 {
            setup_key(&host)?;
        } else {
            warn("mosh and ssh will prompt for your password on every connect.");
            warn("An ssh key is strongly recommended; re-run `tns setup` any time to add one.");
            // verify the password login at least works now
            info("Testing the connection (you may be prompted for your password)...");
            let status = ssh_interactive(&host).arg("true").status()?;
            if status.success() {
                ok("password login works");
            } else {
                fail("could not connect; check the host and try again");
                return Ok(());
            }
        }
    }

    // ---- 3. probe the remote and install mosh if needed
    step(3, total, "Check the remote and install mosh");
    let shell_path = remote::detect_shell(&host).unwrap_or_default();
    let shell = Shell::from_name(&shell_path);
    let session_id = format!("setup-{}", std::process::id());
    let info_res = remote::prepare(&host, &session_id, shell, &shell_path);
    remote::cleanup(&host, &session_id);
    let facts = match info_res {
        Ok(f) => f,
        Err(e) => {
            fail(&format!("could not probe the remote: {}", e));
            return Ok(());
        }
    };
    if shell_path.is_empty() {
        warn("could not read the remote login shell; tns will fall back to sh");
    } else {
        ok(&format!("login shell: {} ({}{})", shell_path, shell.name(), if shell.has_hooks() { ", predictions supported" } else { ", generic fallback" }));
    }
    if facts.utf8_locales == 0 {
        warn("no UTF-8 locale found on the remote; mosh needs one.");
        info("On the server: sudo locale-gen en_US.UTF-8  (Debian: sudo dpkg-reconfigure locales), then set LANG.");
    } else {
        ok("UTF-8 locale present");
    }
    if facts.mosh_server {
        ok("mosh-server is already installed");
    } else {
        warn("mosh-server is not installed on the remote");
        match install_mosh_cmd(&facts.pkg) {
            Some(cmd) => {
                info(&format!("This will run, over ssh:  {}", cmd));
                if yes("Install mosh now? (you may be prompted for the remote sudo password)", true) {
                    let status = ssh_interactive(&host).arg("-t").arg(&cmd).status()?;
                    if status.success() {
                        ok("mosh installed");
                    } else {
                        fail("mosh install did not complete; you can run the command above by hand");
                    }
                } else {
                    warn("skipped; tns will need `--ssh` until mosh is installed");
                }
            }
            None => {
                warn(&format!("unknown package manager ({}); install mosh yourself", if facts.pkg.is_empty() { "none detected" } else { &facts.pkg }));
                info("See https://mosh.org/#getting for per-distro instructions.");
            }
        }
    }

    // local mosh
    if !have_command("mosh") {
        warn("mosh is not installed on THIS machine.");
        #[cfg(target_os = "macos")]
        info("Install it with:  brew install mosh");
        #[cfg(not(target_os = "macos"))]
        info("Install it with your package manager, e.g.  sudo apt-get install -y mosh");
    } else {
        ok("mosh is installed locally");
    }

    // ---- 4. done
    step(4, total, "Ready");
    let cmd = if host.contains('@') || config_hosts().contains(&host) { format!("tns {}", host) } else { format!("tns {}", host) };
    ok(&format!("You're set. Connect with:  {}{}{}", BOLD, cmd, RST));
    info("Add --ssh to use plain ssh instead of mosh, or --shell NAME to force a shell.");
    info(&format!("Run an agent with a local UI:  tns agent claude {}", host));
    Ok(())
}

fn setup_key(host: &str) -> io::Result<()> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let ed = home.join(".ssh").join("id_ed25519");
    let rsa = home.join(".ssh").join("id_rsa");
    let key = if ed.exists() {
        ed
    } else if rsa.exists() {
        rsa
    } else {
        info("No ssh key found. Creating one (ed25519, no passphrase).");
        if !yes("Create ~/.ssh/id_ed25519 now?", true) {
            warn("skipped; cannot set up key auth without a key");
            return Ok(());
        }
        fs::create_dir_all(home.join(".ssh")).ok();
        let status = Command::new("ssh-keygen").args(["-t", "ed25519", "-N", "", "-f"]).arg(&ed).status()?;
        if !status.success() {
            fail("ssh-keygen failed");
            return Ok(());
        }
        ok("created ~/.ssh/id_ed25519");
        ed
    };
    info(&format!("Copying your public key to {} (you'll be asked for your password once).", host));
    let pub_key = key.with_extension("pub");
    let status = if have_command("ssh-copy-id") {
        Command::new("ssh-copy-id").args(["-i"]).arg(&pub_key).arg(host).status()?
    } else {
        // portable fallback
        let pk = fs::read_to_string(&pub_key)?;
        let remote = format!("mkdir -p ~/.ssh && chmod 700 ~/.ssh && printf '%s\\n' '{}' >> ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys", pk.trim());
        ssh_interactive(host).arg(&remote).status()?
    };
    if status.success() && key_login_works(host) {
        ok("ssh key login works now");
    } else {
        fail("key copy did not verify; you may need to do it manually");
    }
    Ok(())
}

fn maybe_save_config(entry: &str) {
    // entry is user@host or host; offer an alias so `tns web` works later
    if !yes("Save this host to ~/.ssh/config as an alias?", false) {
        return;
    }
    let (user, hostname) = match entry.split_once('@') {
        Some((u, h)) => (Some(u.to_string()), h.to_string()),
        None => (None, entry.to_string()),
    };
    let alias = prompt("alias (short name to type)", Some(hostname.split('.').next().unwrap_or(&hostname)));
    let port = prompt("port", Some("22"));
    let mut block = format!("\nHost {}\n    HostName {}\n", alias, hostname);
    if let Some(u) = user {
        block += &format!("    User {}\n", u);
    }
    if port != "22" {
        block += &format!("    Port {}\n", port);
    }
    let path = ssh_config_path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).ok();
    }
    match fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => {
            if f.write_all(block.as_bytes()).is_ok() {
                ok(&format!("saved; you can now use `{}` as the host", alias));
            }
        }
        Err(e) => warn(&format!("could not write ~/.ssh/config: {}", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mosh_commands_per_manager() {
        assert!(install_mosh_cmd("apt-get").unwrap().contains("apt-get install -y mosh"));
        assert!(install_mosh_cmd("pacman").unwrap().contains("pacman -S"));
        assert!(install_mosh_cmd("brew").unwrap().contains("brew install mosh"));
        assert!(install_mosh_cmd("nonsense").is_none());
    }
}
