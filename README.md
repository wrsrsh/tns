# tns

tns is a remote terminal for macOS and Linux. It connects to an existing SSH
host, carries the interactive session over mosh's protocol, and previews
typing locally to reduce visible input delay. Your shell, files, and tools
stay on the remote machine.

It works with bash, zsh, and fish, and previews typing inside terminal
applications such as Claude Code once they are seen echoing it. You do not
need a replacement interface or a `tns agent` command to use Claude inside a
session.

## What goes on each machine

| | Local machine — the computer you type on | Remote machine — the server you connect to |
|---|---|---|
| tns | Install here | Not required |
| SSH | Client and working key authentication | SSH server and your authorized key |
| mosh | Not required: tns has its own mosh client | `mosh-server` (from the `mosh` package) |
| Locale | UTF-8 | An available UTF-8 locale |
| Claude and other tools | Not required locally | Install and sign in here, if you use them |
| Rust | Only for building tns; Homebrew handles its build dependency | Not required |

## Getting started

### 1. On your local machine: install tns

On macOS, or Linux with Homebrew:

```sh
brew install wrsrsh/tap/tns
```

Linux without Homebrew has a
[documented source-install path](docs/setup.md#linux-without-homebrew).

### 2. On your remote machine: install mosh

Open a shell on your server. On Debian or Ubuntu, run:

```sh
sudo apt-get update
sudo apt-get install mosh
```

For other systems, see the [remote-machine instructions](docs/setup.md#remote-machine).
You do **not** need to install tns or Rust on this machine.

SSH access and key authentication must already work. The network must also
allow mosh's UDP traffic to the server, normally ports 60000–61000. The setup
guide covers [SSH keys and network requirements](docs/setup.md#ssh-and-network-access).

### 3. Back on your local machine: check both sides

Replace `user@server` with your SSH destination or an alias from `~/.ssh/config`:

```sh
tns setup user@server
```

This prints separate local and remote checklists, tells you exactly where to
run any missing setup commands, and exits unsuccessfully if requirements are
missing. It does not install packages, create keys, or change configuration.

To check just your local machine, use `tns setup --local`.

### 4. Connect from your local machine

```sh
tns user@server
```

You are now in the remote shell. Run your usual commands there, including:

```sh
claude
```

This is Claude's original interface. Install and authenticate Claude on the
remote, not on your local machine, for this workflow.

## Other installation options

The optional installer has explicit machine roles:

```sh
# On the LOCAL machine, from a downloaded copy of this repository's install.sh:
sh install.sh local

# On the REMOTE machine, from a downloaded copy of the same script:
sh install.sh remote
```

The remote mode installs mosh only. Add `--check` to either command to inspect
requirements without changing anything. See the [setup guide](docs/setup.md)
for download commands, prerequisites, manual installation, and troubleshooting.

## Using SSH instead of mosh

Mosh is the default; no extra command or flag is needed. tns talks to the
remote `mosh-server` itself, so it sees which keystrokes the server has
answered. If UDP is unavailable, select plain SSH explicitly:

```sh
tns setup --ssh user@server
tns --ssh user@server
```

`tns --mosh-client user@server` runs the separate `mosh` program underneath
instead, as tns did before it had its own client. That needs mosh installed
locally and predicts less.

In a mosh session, `Ctrl-^ .` quits tns, for instance when the server cannot
be reached; `Ctrl-^ ^` sends a literal `Ctrl-^`.

## Updating

On your local machine:

```sh
brew update
brew upgrade wrsrsh/tap/tns
```

Start a new tns session after upgrading. There is no tns server to upgrade;
remote mosh and your tools are managed with their own package managers.

## What to expect

Cached shell predictions improve as tns learns the shell's redraws: they
include syntax colors and autosuggestions. When the cache misses, tns draws
the typed character itself, as mosh does, and lets the remote output replace
it. The mosh server reports which keystrokes it has answered, so a wrong
guess is withdrawn rather than left on screen.

These previews appear once the program you are typing into has been seen
echoing: after the first character of a line, and from the first character
at a shell prompt that reads like one that has echoed before. A password
prompt does not echo, so what you type into it is not drawn. As with mosh,
the exception is a program that stops echoing partway through a line: keys
already on their way can show for a moment, until the server's answer
withdraws them. Backspace and the left and right arrow keys are previewed
too. `--no-shell-prediction` turns the previews off at shell prompts and
`--no-tui-prediction` inside applications.

With `--ssh` or `--mosh-client` there are no acknowledgments to rely on, and
the previews are limited to plain typing at a shell prompt and in Claude
Code's composer.

The initial calibration uses background SSH sessions and recent shell history; it does
not submit commands. Use `--probes 0 --history 0` to turn off background
calibration. The local cache can contain terminal text, so treat it like shell
history.

Predictions are previews, not changes to the remote application. Unrecognized
input waits for the remote. Nothing speeds up model inference or predicts
permission decisions. See [how tns compares with mosh](docs/benchmarks.md)
and [the supported behavior and limits](docs/in-session-prediction.md).

## Documentation and development

- [Setup, configuration, and troubleshooting](docs/setup.md)
- [Development and tests](docs/development.md)
- [How the terminal and prediction layers work](docs/architecture.md)
- [Historical benchmarks](docs/benchmarks.md)
- [Remote Claude and Codex in Pi](docs/agents.md)

Licensed under the [MIT License](LICENSE).
