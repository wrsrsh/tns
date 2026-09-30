# Development

These instructions are for changing tns itself. To install it as a user, follow
[the local and remote setup guide](setup.md). You do not need a development
checkout or Rust toolchain on a server just to connect to it.

## Prerequisites

Use macOS or Linux, with:

- A current stable Rust toolchain and C compiler/linker.
- Python 3 with `venv` and `pip` for the terminal tests.
- Git to clone the repository.

SSH and mosh are needed for real remote-session testing, not for the fake-host
regression tests. bash, zsh, and fish are useful for the optional hook smoke tests.

## Set up the checkout

```sh
git clone https://github.com/wrsrsh/tns.git
cd tns
bin/setup
```

`bin/setup` creates a project-local `.venv`, installs the Python test dependencies,
and builds the debug client. It does not install system packages, connect to a
remote host, change SSH configuration, or replace your installed Homebrew client.

Run the development binary explicitly:

```sh
./target/debug/tns --help
./target/debug/tns setup --local
```

## Run the checks

```sh
bin/check
```

This runs shell syntax checks, Rust unit tests, setup/installer regression tests,
and the existing client/TUI PTY tests. The setup and installer tests use fake
commands in temporary directories: they do not install packages or contact a
real server. The TUI tests do not call an agent provider or approve permissions.
The same command runs in CI on Linux and macOS.

Individual suites:

```sh
cargo test --locked
cargo build --locked
.venv/bin/python tests/setup_regressions.py
.venv/bin/python tests/install_regressions.py
.venv/bin/python tests/client_regressions.py
.venv/bin/python tests/tui_prediction.py
```

The test runners accept `TNS_BIN=/absolute/path/to/tns` to test another build.

## Release build and additional terminal checks

```sh
cargo build --release --locked
TNS_BIN=target/release/tns sh tests/hooks_smoke.sh
```

The optional differential tests use [uv](https://docs.astral.sh/uv/) to run the
Python reference emulator:

```sh
TNS_BIN=target/release/tns sh tests/diff_pyte.sh tests/fixtures/local.bin 80x24
TNS_BIN=target/release/tns sh tests/diff_pyte.sh tests/fixtures/remote.bin 100x30
```

Review the reported prefix matches; a terminal stream can be temporarily wrong
even if its final screenshot looks correct. Historical timing measurements are
in [benchmarks](benchmarks.md), separate from the user installation instructions.

## Before a release

- Run `bin/check` and the relevant live-session checks without submitting agent
  prompts or approving permissions unnecessarily.
- Keep the default source-install version in `install.sh` aligned with
  `Cargo.toml` and `Cargo.lock`; the installer suite checks this.
- Update the setup guide's release-tag examples when changing that version.
- Tag the tested commit, then update the Homebrew formula's URL and SHA-256.
- Test an installation from the published archive, not just the working tree.
- Confirm `tns setup --local` and `tns setup HOST` with the installed executable.

Do not include private terminal captures, shell history, SSH keys, or agent
credentials in commits or issue reports. The older `tns.py` implementation and
`tests/fixtures` are reference material for development, not required installs.

## Pi extension checks

Install Node.js 24 or newer, then run:

```sh
npm ci --ignore-scripts
cargo build --locked
npm run check
npm test
```

These tests drive the real Rust bridge with simulated Claude, Codex, and SSH
processes, and load the extension in the installed development Pi CLI. They test
streaming, approval replies, cancellation, reconnect/resume, new-session isolation,
SSH argument quoting, namespaced remote controls, model persistence, and native
terminal handoff without contacting model providers or remote hosts. A Pi RPC
test also checks that `/tns compact` reaches the remote adapter instead of Pi's
local compaction handler.
