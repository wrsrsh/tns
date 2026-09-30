# tns

tns is a remote terminal for macOS and Linux. It connects to an existing SSH
host, uses mosh for the interactive session, and previews supported typing
locally to reduce visible input delay. Your shell, files, and tools stay on
the remote machine.

It works with bash, zsh, and fish. It also has experimental typing previews
for Claude Code's existing single-line composer. You do not need a replacement
interface or a `tns agent` command to use Claude inside a session.

## What goes on each machine

| | Local machine — the computer you type on | Remote machine — the server you connect to |
|---|---|---|
| tns | Install here | Not required |
| SSH | Client and working key authentication | SSH server and your authorized key |
| mosh | `mosh` client | `mosh-server` (from the `mosh` package) |
| Locale | UTF-8 | An available UTF-8 locale |
| Claude and other tools | Not required locally | Install and sign in here, if you use them |
| Rust | Only for building tns; Homebrew handles its build dependency | Not required |

## Getting started

### 1. On your local machine: install tns

On macOS, or Linux with Homebrew:

```sh
brew install wrsrsh/tap/tns
```

This also installs mosh. Linux without Homebrew has a
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

Mosh is the default; no extra command or flag is needed. If UDP is unavailable,
select plain SSH explicitly:

```sh
tns setup --ssh user@server
tns --ssh user@server
```

## Updating

On your local machine:

```sh
brew update
brew upgrade wrsrsh/tap/tns
```

Start a new tns session after upgrading. There is no tns server to upgrade;
remote mosh and your tools are managed with their own package managers.

## What to expect

Shell predictions improve as tns learns the shell's redraws. The initial
calibration uses background SSH sessions and recent shell history; it does
not submit commands. Use `--probes 0 --history 0` to turn off background
calibration. The local cache can contain terminal text, so treat it like shell
history.

Predictions are previews, not changes to the remote application. Unrecognized
input waits for the remote. Claude support is deliberately limited; it does
not speed up model inference or predict permission decisions. See
[the supported behavior and limits](docs/in-session-prediction.md).

## Documentation and development

- [Setup, configuration, and troubleshooting](docs/setup.md)
- [Development and tests](docs/development.md)
- [How the terminal and prediction layers work](docs/architecture.md)
- [Historical benchmarks](docs/benchmarks.md)
- [Remote Claude and Codex in Pi](docs/agents.md)

Licensed under the [MIT License](LICENSE).
