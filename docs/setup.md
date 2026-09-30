# Setting up tns

There are two machines involved. **Local** is the computer running your terminal.
**Remote** is the machine with your project, shell, and tools. Commands below
are labeled by where they run. You only install the tns client locally.

## Local machine

### macOS or Linux with Homebrew

Run on the **local machine**:

```sh
brew install wrsrsh/tap/tns
tns --version
tns setup --local
```

Homebrew installs mosh and the Rust build dependency for you. Install
[Homebrew](https://brew.sh) first if you want this installation method; tns's
installer does not bootstrap a package manager without your involvement.

### Linux without Homebrew

Run on the **local Linux machine**. On Debian/Ubuntu, install the runtime and
build dependencies:

```sh
sudo apt-get update
sudo apt-get install openssh-client mosh build-essential
```

Install a current Rust toolchain using [rustup](https://rustup.rs) or your
preferred toolchain manager. Check `cargo --version` and `cc --version`.
Then build the released client:

```sh
cargo install --git https://github.com/wrsrsh/tns --tag v0.6.0 --locked
```

Cargo normally installs into `~/.cargo/bin`. If `tns` is not found, add that
directory to your local PATH. For bash/zsh:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
```

For fish:

```fish
fish_add_path $HOME/.cargo/bin
```

Then run `tns --version` and `tns setup --local`. Native Windows is not supported;
use the Linux instructions inside WSL.

## Remote machine

Open a shell on the **remote machine** using your existing SSH access or your
server provider's console. Install the `mosh` package, which provides
`mosh-server`:

| Remote system | Command to run on the remote |
|---|---|
| Debian / Ubuntu | `sudo apt-get update && sudo apt-get install mosh` |
| Fedora | `sudo dnf install mosh` |
| Arch | `sudo pacman -S mosh` |
| Alpine | `sudo apk add mosh` |
| openSUSE | `sudo zypper install mosh` |
| macOS / Homebrew | `brew install mosh` |

Omit `sudo` when you are already root. Your SSH service and login shell must
already be configured. tns does not install or enable an SSH service for you.

Check on the **remote machine**:

```sh
command -v mosh-server
printf '%s\n' "$SHELL"
locale charmap
locale -a
```

Mosh needs a UTF-8 locale. If none is installed, generate one using the remote
system's locale tools. For Debian/Ubuntu with the `locales` package installed:

```sh
sudo locale-gen en_US.UTF-8
```

Select an installed UTF-8 locale in your shell configuration if needed. For
bash/zsh, for example, `export LANG=en_US.UTF-8`; for fish,
`set -gx LANG en_US.UTF-8`. Do not select a locale that `locale -a` does not list.

**Do not install tns or Rust on the remote just to connect to it.** Mosh starts
its per-session process over SSH. tns uploads temporary shell hooks when an
actual session starts, and removes them on exit; it does not modify your
remote shell startup files.

### Claude and other tools

Install the programs you intend to run on the **remote machine**. For Claude,
follow its [official installation instructions](https://code.claude.com/docs/en/setup),
then run `claude` there and complete its normal login and workspace-trust flow.

The setup check only reports whether `claude` is on the remote PATH. It does
not log in, accept workspace trust, approve tools, or install Claude. A local
Claude installation is not required for the normal `tns HOST` → `claude` flow.

## SSH and network access

### SSH keys — configure from the local machine

First connect normally from your **local machine**:

```sh
ssh user@server
```

On first connection, verify the host-key fingerprint through a trusted channel.
Do not disable host-key verification. Exit back to your local terminal afterward.

Use your existing SSH key if you have one. If you need a new key, create it on
the **local machine**, choosing a passphrase when prompted:

```sh
ssh-keygen -t ed25519
```

Do not overwrite an existing key. Authorize its **public** key for your remote
account. If `ssh-copy-id` is available on your local machine:

```sh
ssh-copy-id user@server
```

Otherwise add the public key to the remote account's `~/.ssh/authorized_keys`
using your provider's documented process. Never copy your private key to the
server. Load encrypted keys into your local SSH agent with `ssh-add` as needed.

This must succeed from the **local machine** without a password prompt:

```sh
ssh -o BatchMode=yes user@server true
```

Password-only authentication is not sufficient for tns: its background
sessions also use non-interactive SSH. The setup command will report that
failure rather than claim the host is ready.

### Optional SSH alias — local configuration

For a shorter command, add an entry to **local** `~/.ssh/config` yourself:

```sshconfig
Host work
    HostName server.example.com
    User your-username
    IdentityFile ~/.ssh/id_ed25519
    IdentitiesOnly yes
```

You can then use `ssh work`, `tns setup work`, and `tns work`. For a non-default
SSH port, add `Port 2222` (using your actual port) to the same entry.

### Network policy — remote firewall and cloud network

SSH must reach the remote SSH port. Mosh additionally needs UDP traffic from
your client to the remote, normally in the range **60000–61000**. Check both
host firewall rules and any cloud security group. Limit access to the clients
or private network that should be able to connect.

A VPN can provide the route, but its ACLs and the host firewall still apply.
tns does not change these policies or test UDP reachability during setup.
Do not enable, reset, or flush a firewall just to run the installer.

If UDP is not available, you can use plain SSH instead:

```sh
# On the LOCAL machine:
tns setup --ssh user@server
tns --ssh user@server
```

Mosh is not required on either side in this mode. A UTF-8 locale is still
recommended. Plain SSH does not provide mosh's roaming/reconnection behavior.

## Check both machines, then connect

From the **local machine**:

```sh
tns setup user@server
```

The output has separate **Local machine**, **Remote machine**, and **Result**
sections. A missing requirement includes the command to run and which machine
it belongs on. Setup is non-interactive and read-only: it does not create
keys, write SSH config, install packages, or upload tns session hooks.

- `tns setup` or `tns setup --local`: checks the local machine only. No SSH
  alias is selected automatically, even if only one is configured.
- `tns setup HOST`: checks local and remote requirements for mosh.
- `tns setup --ssh HOST`: checks requirements for the SSH-only mode.
- Exit `0`: checked requirements passed; `1`: action is needed; `2`: bad arguments.

An all-pass report is a prerequisite check, not proof of UDP connectivity or
Claude authentication. Connect to test the actual session:

```sh
# On the LOCAL machine:
tns user@server
```

Then run `claude`, your editor, or other commands **in the remote shell**.

## Optional installer

Manual package-manager commands above are the primary setup path. If you want
the helper script, download it onto the machine you are preparing and inspect
it before running it:

```sh
curl -fsSL https://raw.githubusercontent.com/wrsrsh/tns/main/install.sh -o install-tns.sh
```

On the **local machine**:

```sh
sh install-tns.sh local
```

On the **remote machine** (download the script there separately):

```sh
sh install-tns.sh remote
```

The local mode uses Homebrew when available, otherwise a release-pinned Rust
source build on Linux. Rust and a C compiler must already be installed for that
source build. The remote mode installs mosh only. The script uses the system
package manager and may ask for sudo credentials; command failures stop it.
It does not edit shell profiles, create keys, configure services, change
firewalls, or install Claude.

To inspect requirements without any installation:

```sh
sh install-tns.sh local --check
# Or, on the remote:
sh install-tns.sh remote --check
```

For a Linux source install, `TNS_VERSION=0.6.0 sh install-tns.sh local` can pin
a release explicitly. Homebrew installations follow the tap's current version.
The script no longer assumes a local role when called without arguments, and
no longer launches an interactive wizard after installation.

## Updating and removing

**Local / Homebrew:** `brew update && brew upgrade wrsrsh/tap/tns`.
**Local / Cargo:** rerun `cargo install` with the desired release tag.
Start a new tns session to use the updated client. Existing sessions are not
terminated by an upgrade. There is no tns server package to upgrade remotely.
Update remote mosh and your tools through their respective package managers.

Remove the local client with `brew uninstall tns` or `cargo uninstall tns`,
depending on how you installed it. Removing tns does not remove your SSH keys
or remote tools. The local prediction cache is in `~/.cache/tns`; delete it
only if you also want to discard learned predictions and debug logs.

## Troubleshooting

| Symptom | Check or action |
|---|---|
| `tns` is not found, or the version is old | On the local machine, check `command -v tns` and `tns --version`; another install may shadow the new binary. |
| SSH works with a password but setup fails | Configure local SSH-agent/key authentication and test `ssh -o BatchMode=yes HOST true`. |
| Host-key verification fails | Verify the host's fingerprint using normal interactive SSH. Do not bypass verification. |
| mosh-server missing | Install `mosh` on the remote, not tns. |
| Requirements pass but mosh waits for the server | Check UDP routing, firewall rules, VPN ACLs, and cloud security groups. Try `tns --ssh HOST` to isolate the transport. |
| Claude not found or not logged in | Install and authenticate Claude on the remote. |
| Claude typing is not predicted | The composer must match the supported layout. See [prediction limits](in-session-prediction.md). Unknown layouts pass through normally. |
| An old setup command claimed “Ready” despite a failed check | Upgrade to tns 0.6.0 or later. Setup now exits unsuccessfully for missing requirements. |
