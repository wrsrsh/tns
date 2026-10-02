//! Read-only setup checks with explicit local and remote instructions.
//! Installation, SSH keys, services, and firewall policy remain separate.

use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const HELP: &str = "\
usage: tns setup [--ssh] [HOST]
       tns setup --local [--ssh]

Run this command on your LOCAL machine.

Without HOST, check this machine and explain the remote requirements.
With HOST, also check the remote machine over non-interactive SSH.
HOST can be an SSH config alias or user@hostname.

  --local   check only this machine; do not connect to a remote host
  --ssh     check for plain SSH instead of the default mosh transport
  --help    show this help

These checks do not install packages, create keys, edit configuration,
change firewall rules, or upload session hooks.

Exit status: 0 = checked requirements passed, 1 = action needed, 2 = usage error.
";

// Send a fixed POSIX script, never interpolate a hostname or other user input.
const REMOTE_PROBE: &str = r#"
printf 'TNS_SETUP_V1=1\n'
printf 'OS=%s\n' "$(uname -s)"
printf 'SHELL=%s\n' "${SHELL:-/bin/sh}"
if [ -x "${SHELL:-/bin/sh}" ]; then echo SHELL_OK=1; else echo SHELL_OK=0; fi
printf 'MOSH=%s\n' "$(command -v mosh-server 2>/dev/null)"
printf 'CHARMAP=%s\n' "$(locale charmap 2>/dev/null)"
printf 'UTF8=%s\n' "$(locale -a 2>/dev/null | grep -Eic 'utf[-_]?8')"
for p in apt-get dnf yum pacman apk zypper brew; do
  if command -v "$p" >/dev/null 2>&1; then printf 'PKG=%s\n' "$p"; break; fi
done
printf 'CLAUDE=%s\n' "$(command -v claude 2>/dev/null)"
"#;

#[derive(Debug, Default, PartialEq, Eq)]
struct Options {
    host: Option<String>,
    ssh: bool,
    local: bool,
    help: bool,
}

fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && !host.starts_with('-')
        && host.bytes().all(|b| b.is_ascii_alphanumeric() || b".-_@[]:%".contains(&b))
}

fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    for arg in args {
        match arg.as_str() {
            "-h" | "--help" => options.help = true,
            "--local" => options.local = true,
            "--ssh" => options.ssh = true,
            flag if flag.starts_with('-') => return Err(format!("unknown option: {flag}")),
            host if !valid_host(host) => return Err("HOST must be an SSH alias or user@hostname, without spaces or shell syntax".into()),
            host => {
                if options.host.replace(host.to_string()).is_some() {
                    return Err("provide only one remote host".into());
                }
            }
        }
    }
    if options.local && options.host.is_some() {
        return Err("--local cannot be combined with HOST".into());
    }
    Ok(options)
}

fn clean(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).take(240).collect::<String>().trim().to_string()
}

fn command_path(name: &str) -> Option<String> {
    let out = Command::new("sh")
        .args(["-c", "command -v \"$1\"", "tns-setup", name])
        .stdin(Stdio::null()).output().ok()?;
    if !out.status.success() { return None; }
    let path = clean(&String::from_utf8_lossy(&out.stdout));
    if path.is_empty() { None } else { Some(path) }
}

fn utf8(value: &str) -> bool {
    value.to_ascii_lowercase().replace(['-', '_', '.'], "").contains("utf8")
}

fn local_charmap() -> String {
    Command::new("locale").arg("charmap").stdin(Stdio::null()).output()
        .ok().filter(|out| out.status.success())
        .map(|out| clean(&String::from_utf8_lossy(&out.stdout))).unwrap_or_default()
}

fn mosh_install(pkg: &str) -> Option<&'static str> {
    match pkg {
        "brew" => Some("brew install mosh"),
        "apt-get" => Some("sudo apt-get update && sudo apt-get install mosh"),
        "dnf" => Some("sudo dnf install mosh"),
        "yum" => Some("sudo yum install mosh"),
        "pacman" => Some("sudo pacman -S mosh"),
        "apk" => Some("sudo apk add mosh"),
        "zypper" => Some("sudo zypper install mosh"),
        _ => None,
    }
}

#[derive(Default)]
struct Report { missing: usize }

impl Report {
    fn ok(&self, label: &str, detail: &str) {
        println!("  [ok] {label}: {}", clean(detail));
    }
    fn missing(&mut self, label: &str, detail: &str) {
        self.missing += 1;
        println!("  [missing] {label}: {}", clean(detail));
    }
    fn info(&self, detail: &str) {
        println!("  [info] {}", clean(detail));
    }
}

#[derive(Debug, Default)]
struct Facts {
    os: String,
    shell: String,
    shell_ok: bool,
    mosh: String,
    charmap: String,
    utf8_locales: usize,
    pkg: String,
    claude: String,
}

fn parse_facts(text: &str) -> Result<Facts, String> {
    let mut facts = Facts::default();
    let mut started = false;
    for line in text.lines() {
        if line.trim() == "TNS_SETUP_V1=1" {
            started = true;
            continue;
        }
        if !started { continue; }
        let Some((key, value)) = line.trim_end().split_once('=') else { continue; };
        match key {
            "OS" => facts.os = clean(value),
            "SHELL" => facts.shell = clean(value),
            "SHELL_OK" => facts.shell_ok = value == "1",
            "MOSH" => facts.mosh = clean(value),
            "CHARMAP" => facts.charmap = clean(value),
            "UTF8" => facts.utf8_locales = value.parse().unwrap_or(0),
            "PKG" => facts.pkg = clean(value),
            "CLAUDE" => facts.claude = clean(value),
            _ => {}
        }
    }
    if !started || facts.os.is_empty() || facts.shell.is_empty() {
        return Err("SSH returned no complete setup report; check that the host can run sh and locale".into());
    }
    Ok(facts)
}

fn read_limited(reader: impl Read) -> Vec<u8> {
    let mut bytes = Vec::new();
    let _ = reader.take(64 * 1024).read_to_end(&mut bytes);
    bytes
}

fn remote_facts(host: &str) -> Result<Facts, String> {
    let mut child = Command::new("ssh")
        .args([
            "-T", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes",
            "-o", "UpdateHostKeys=no", "-o", "ControlMaster=no", "-o", "ControlPath=none",
            "-o", "ClearAllForwardings=yes", "-o", "ForwardAgent=no",
            "-o", "ForwardX11=no", "-o", "PermitLocalCommand=no",
            "-o", "ConnectTimeout=8", "-o", "ConnectionAttempts=1",
            "-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=1",
            "--", host, "sh", "-s",
        ])
        .process_group(0)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().map_err(|e| format!("cannot start ssh: {e}"))?;
    let out = std::thread::spawn({
        let stdout = child.stdout.take().unwrap();
        move || read_limited(stdout)
    });
    let err = std::thread::spawn({
        let stderr = child.stderr.take().unwrap();
        move || read_limited(stderr)
    });
    let write_result = child.stdin.take().unwrap().write_all(REMOTE_PROBE.as_bytes());
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            result => {
                // Kill only this check's process group, including SSH helpers.
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL); }
                let _ = child.wait();
                return Err(match result {
                    Err(e) => format!("SSH check failed: {e}"),
                    _ => "SSH check timed out after 20 seconds".into(),
                });
            }
        }
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    if !status.success() {
        let detail = clean(&String::from_utf8_lossy(&stderr));
        return Err(if detail.is_empty() { format!("ssh exited with {status}") } else { detail });
    }
    write_result.map_err(|e| format!("could not send setup check: {e}"))?;
    parse_facts(&String::from_utf8_lossy(&stdout))
}

fn local_checks(report: &mut Report, ssh_only: bool) -> bool {
    println!("\nLocal machine (this computer)\n-----------------------------");
    report.ok("tns", env!("CARGO_PKG_VERSION"));
    let ssh = command_path("ssh");
    match &ssh {
        Some(path) => report.ok("SSH client", path),
        None => {
            report.missing("SSH client", "install OpenSSH on this machine");
            println!("    On Debian/Ubuntu: sudo apt-get install openssh-client");
        }
    }
    // tns speaks mosh's protocol itself; the mosh package is only needed
    // locally for the `--mosh-client` fallback.
    match command_path("mosh") {
        Some(path) => report.ok("mosh client (optional, for --mosh-client)", &path),
        None if ssh_only => report.info("mosh is not required with --ssh"),
        None => report.info("no local mosh client: not required, tns has its own (--mosh-client would need it)"),
    }
    let charmap = local_charmap();
    if utf8(&charmap) {
        report.ok("terminal locale", &charmap);
    } else if ssh_only {
        report.info("a UTF-8 terminal locale is recommended");
    } else {
        report.missing("terminal locale", if charmap.is_empty() { "could not read locale charmap" } else { &charmap });
        println!("    Select an installed UTF-8 locale in your local shell. Check: locale -a");
    }
    ssh.is_some()
}

fn remote_checks(report: &mut Report, host: &str, ssh_only: bool) {
    println!("\nRemote machine ({host})\n-----------------------------");
    println!("  Checking over SSH. No files or packages will be changed.");
    let facts = match remote_facts(host) {
        Ok(facts) => facts,
        Err(e) => {
            report.missing("non-interactive SSH", &e);
            println!("\n  On your LOCAL machine:");
            println!("    1. Run: ssh {host}");
            println!("       Verify the server's host-key fingerprint when first connecting.");
            println!("    2. Configure SSH key authentication; load encrypted keys with ssh-add.");
            println!("       Password-only login does not work with tns's background sessions.");
            println!("    3. Confirm: ssh -o BatchMode=yes {host} true");
            println!("    4. Retry: tns setup {}{host}", if ssh_only { "--ssh " } else { "" });
            println!("  On the REMOTE machine, ensure the SSH service and your authorized key are configured.");
            return;
        }
    };
    report.ok("non-interactive SSH", host);
    report.ok("operating system", &facts.os);
    if facts.shell_ok {
        report.ok("login shell", &facts.shell);
    } else {
        report.missing("login shell", &facts.shell);
        println!("    On the REMOTE machine, configure an installed, executable login shell.");
    }
    if !facts.mosh.is_empty() {
        report.ok("mosh-server", &facts.mosh);
    } else if ssh_only {
        report.info("mosh-server is not required with --ssh");
    } else {
        report.missing("mosh-server", "install the mosh package on the remote machine");
        if let Some(cmd) = mosh_install(&facts.pkg) {
            println!("    Run on the REMOTE machine:\n      {cmd}");
        } else {
            println!("    Use the remote machine's package manager to install mosh.");
        }
    }
    if facts.utf8_locales > 0 || utf8(&facts.charmap) {
        report.ok("UTF-8 locale", &format!("available; current character map: {}", facts.charmap));
    } else if ssh_only {
        report.info("a UTF-8 locale on the remote machine is recommended");
    } else {
        report.missing("UTF-8 locale", "none found on the remote machine");
        println!("    On Debian/Ubuntu: sudo locale-gen en_US.UTF-8");
        println!("    Select an installed UTF-8 locale in the remote shell, then retry.");
    }
    if facts.claude.is_empty() {
        report.info("Claude Code is optional and was not found on the remote PATH");
        println!("    If you use Claude, install it and sign in on the REMOTE machine.");
    } else {
        report.ok("Claude Code (optional)", &facts.claude);
        report.info("Claude authentication and workspace trust were not checked");
    }
    report.info("the remote machine does not need tns or a Rust toolchain");
}

pub fn run(args: &[String]) -> io::Result<i32> {
    let options = match parse_args(args) {
        Ok(options) => options,
        Err(e) => {
            eprintln!("tns setup: {e}\n\n{HELP}");
            return Ok(2);
        }
    };
    if options.help {
        println!("{HELP}");
        return Ok(0);
    }
    println!("tns setup\n=========\nRead-only checks. Commands below are instructions, not automatic changes.");
    let mut report = Report::default();
    let have_ssh = local_checks(&mut report, options.ssh);
    if let Some(host) = &options.host {
        if have_ssh {
            remote_checks(&mut report, host, options.ssh);
        } else {
            println!("\nRemote machine ({host})\n-----------------------------");
            report.info("not checked: install the local SSH client first");
        }
    } else {
        println!("\nRemote machine (not checked)\n-----------------------------");
        if options.ssh {
            println!("  Required there: SSH access and an installed shell. UTF-8 is recommended.");
        } else {
            println!("  Required there: SSH access, an installed shell, mosh-server, and a UTF-8 locale.");
            println!("\n  On a Debian/Ubuntu REMOTE machine:\n    sudo apt-get update\n    sudo apt-get install mosh");
        }
        println!("  Install your tools, such as Claude Code, on that machine too.");
        println!("  Do not install tns or Rust on the remote just to use it as a server.");
        println!("\n  Then, on this LOCAL machine:\n    ssh user@server\n    tns setup {}user@server", if options.ssh { "--ssh " } else { "" });
    }
    println!("\nResult\n------");
    if report.missing > 0 {
        println!("Action needed: {} required check(s) failed. Follow the instructions above, then rerun setup.", report.missing);
        return Ok(1);
    }
    if let Some(host) = &options.host {
        println!("Local and remote requirements passed.");
        if !options.ssh {
            println!("UDP connectivity was not tested. The network must allow UDP 60000-61000 to the remote.");
        }
        println!("\nConnect from this LOCAL machine:\n  tns {}{host}", if options.ssh { "--ssh " } else { "" });
        println!("Then run your tools, such as claude, in the remote shell.");
    } else {
        println!("Local requirements passed. A remote host still needs to be checked.");
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn setup_arguments_have_no_implicit_host_selection() {
        assert_eq!(parse_args(&[]).unwrap(), Options::default());
        assert!(parse_args(&args(&["--help"])).unwrap().help);
        assert!(parse_args(&args(&["--local"])).unwrap().local);
        assert!(parse_args(&args(&["--ssh", "user@server"])).unwrap().ssh);
        for invalid in [&["--local", "server"][..], &["a", "b"], &["--install"], &["a;touch-x"], &["a\nb"], &["-oProxyCommand=x"]] {
            assert!(parse_args(&args(invalid)).is_err(), "{invalid:?}");
        }
        for host in ["work", "user@server.example", "user@[::1]", "fe80::1%eth0"] {
            assert!(valid_host(host));
        }
    }

    #[test]
    fn remote_report_must_be_complete_and_marked() {
        assert!(parse_facts("").is_err());
        assert!(parse_facts("SHELL=/bin/bash\nOS=Linux").is_err());
        assert!(parse_facts("TNS_SETUP_V1=1\nOS=Linux").is_err());
        let f = parse_facts("banner\nTNS_SETUP_V1=1\nOS=Linux\nSHELL=/bin/bash\nSHELL_OK=1\nMOSH=/usr/bin/mosh-server\nUTF8=2\n").unwrap();
        assert!(f.shell_ok);
        assert_eq!(f.utf8_locales, 2);
        assert_eq!(f.mosh, "/usr/bin/mosh-server");
    }

    #[test]
    fn locale_and_package_instructions_are_explicit() {
        for name in ["UTF-8", "utf8", "C.utf8"] { assert!(utf8(name)); }
        assert!(!utf8("ANSI_X3.4-1968"));
        assert!(mosh_install("apt-get").unwrap().contains("install mosh"));
        assert!(mosh_install("unknown").is_none());
        assert!(!clean("\x1b[31mhello\n").contains('\x1b'));
    }
}
