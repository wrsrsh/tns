# tns

A predictive terminal for a remote shell. The real shell stays on the
server; the local side keeps a cache of how that shell redraws the screen for
each keystroke and paints from the cache instantly, then reconciles when the
real bytes arrive. It runs over mosh by default (UDP, roaming, reconnect) and
falls back to ssh.

Mosh predicts only plain character echo. With fish, starship and friends every
key triggers a redraw (autosuggestion, syntax colours, abbreviations), so mosh
either waits a round trip or shows an underlined guess. tns learns the whole
redraw, so the frame you see after a keystroke is the final one. bash, zsh and
fish get full prompt/command tracking; any other shell works with a generic
fallback.

The Rust implementation is the real thing; the original Python prototype
(`tns.py`) is kept as the reference for differential tests and benchmarks.

## Install

One line installs tns and mosh, then walks you through your first host:

```sh
curl -fsSL https://raw.githubusercontent.com/wrsrsh/tns/main/install.sh | sh
```

Or install by hand:

```sh
brew install wrsrsh/tap/tns                          # macOS or Linux (Homebrew); pulls mosh + a rust build toolchain
cargo install --git https://github.com/wrsrsh/tns    # with a Rust toolchain (install mosh separately)
```

The remote host needs a shell (bash, zsh, fish, or anything else) and, for the
default mosh transport, `mosh` on both ends. `tns setup` installs mosh for you.

```sh
tns setup           # interactive: pick a host, set up an ssh key, install mosh, save an alias
tns HOST            # connect (mosh by default). HOST is anything ssh accepts; ~/.ssh/config aliases work
tns --ssh HOST      # use plain ssh instead of mosh
tns --shell bash HOST   # force a remote shell instead of the login shell
tns HOST --debug    # also log to ~/.cache/tns/debug.log
```

Type as usual. The first connect to a host spends ~12 s calibrating in the
background, so predictions get better over the first half minute; the cache
is kept in `~/.cache/tns/` and reused. Exit with `exit` or ctrl-d; a one-line
stats summary is printed.

```
tns [--ssh] [--shell NAME] [--probes N] [--calib-seconds S] [--history N] [--debug] HOST
  --ssh               carry the session over plain ssh instead of mosh (the default)
  --shell NAME        remote shell to start (bash, zsh, fish, ...); default: the login shell
  --probes N          hidden calibration sessions (default 6)
  --calib-seconds S   burst calibration time; afterwards one probe keeps learning (default 12)
  --history N         how many recent history entries to learn (default 400)
```

### tns setup

`tns setup [HOST]` is an interactive wizard. It lists your `~/.ssh/config`
hosts (or takes a new `user@host`), checks whether you can log in, and if not
offers to create an ed25519 key and copy it over (one password prompt), or to
use a password each time. It then probes the remote for its login shell and
UTF-8 locale, installs mosh with the remote's package manager if it is
missing, and can save a short `~/.ssh/config` alias so `tns web` works later.
Every step prints the command it will run first. `./setup-demo.sh [HOST]`
is a narrated walkthrough that runs the real wizard.

### Any shell

tns starts your login shell by default and adapts how it tracks the prompt:

- **bash, zsh, fish** get an uploaded hook (a `PROMPT_COMMAND`, a `precmd`
  hook, or fish events) that marks each prompt and reports the command you ran
  and the working directory. Predictions and per-command re-probing are fully
  supported.
- **any other shell** runs unhooked: tns treats the command line on screen as
  the executed command and the next quiet screen as the next prompt. Echo and
  autosuggestions are still predicted; there is just no cwd tracking.

Hooks are uploaded to a per-session directory under `/tmp/tns-<id>` on the
remote and removed on exit; they never touch your dotfiles.

### Claude's existing TUI, inside a normal session

```sh
tns HOST
# In the remote shell:
claude
```

No `tns agent` command or replacement interface is needed. tns recognizes
Claude Code's bordered, single-line composer and learns whether literal edits
are echoed as expected. After a few confirmed keystrokes it previews ordinary
ASCII typing and backspace locally, then reconciles with Claude's real output.
Only the input row is overlaid; Claude still renders its own transcript,
menus, tools, and permission dialogs.

This is conservative, experimental, layout-aware support—not prediction of
arbitrary TUI behavior. It falls back to passthrough for unrecognized layouts,
hidden cursors, multiline/wrapping or non-ASCII input, styled mentions, bulk
pastes, menus, and control keys. It never predicts Enter or permission answers,
submits a message, or runs a probe against Claude. A mismatched or expired
preview is discarded. TUI text is not saved to the shell prediction cache.

Disable this layer with `tns --no-tui-prediction HOST`. The exit summary reports
TUI previews separately from shell predictions. See
[in-session prediction](docs/in-session-prediction.md) for validation and limits.

### Transport: mosh (default) or ssh

By default the session runs over `mosh --predict=never`, so you get everything
mosh provides natively: UDP with roaming across networks and sleep, automatic
reconnection, frame-rate screen sync that never falls behind a flood of
output, and the "mosh: waiting" status line when the link is down. tns predicts
on top of what mosh-client draws. `--ssh` uses a plain `ssh -t` pty instead.

Mosh's own prediction (the underlined local echo) is switched off: mosh
only guesses the bare character, while tns paints the full redraw, and if
mosh's speculative echo were visible tns would learn it as the shell's real
output. Keys tns does not know wait one round trip, as with ssh.

Two things differ under the hood in mosh mode. Mosh's server-side terminal
emulator drops the OSC marks tns uses to detect prompts, so in mosh mode the
shell hook appends its prompt / command / cwd events to a file on the server
(inside `/tmp/tns-<id>`, removed on exit) and tns follows it with `tail -F`
over a multiplexed ssh side channel that reconnects by itself. And mosh-client
draws inside the alternate screen, so the model treats that as the primary
screen; remote TUIs are still passed through, prediction just pauses because no
prompt is on screen. The calibration probes always use ssh multiplexing,
whatever the main transport.

### Setting up mosh by hand

`tns setup` does this for you, but if you prefer:

1. Local: `brew install mosh`, or your package manager on Linux.
2. Remote: `sudo apt install mosh` (Debian/Ubuntu), `sudo dnf install mosh`
   (Fedora), `sudo pacman -S mosh` (Arch), `brew install mosh` (macOS).
3. Open UDP ports 60000-61000 inbound on the server if it has a firewall,
   e.g. `sudo ufw allow 60000:61000/udp`. Over Tailscale or WireGuard nothing
   needs opening.
4. Both sides need a UTF-8 locale. If mosh complains, on the server run
   `sudo locale-gen en_US.UTF-8` (Debian: `sudo dpkg-reconfigure locales`) and
   set `LANG`.
5. Check that plain `mosh HOST` works, then use `tns HOST`.

## How it works

1. **Passthrough.** One ssh pty carries your real session. Every byte from the
   server goes to your terminal untouched, so scrollback, colours and TUIs are
   exactly what ssh would give you. A local terminal model (`src/term.rs`)
   tracks the confirmed remote screen.
2. **Anchor.** The remote shell is started with an uploaded hook that emits a
   mark before each prompt and reports the executed command line after each
   command (bash/zsh/fish; other shells derive the same from Enter and screen
   quiescence). When a prompt settles, the cursor position becomes the
   *anchor*: where your input starts.
3. **Cache.** A state is the text from the anchor to the end of the
   autosuggestion (the right prompt clock is masked out), plus the cursor
   offset and any rows below. The cache maps `hash(state, key)` to the cell
   diff fish drew and the new cursor offset. Diffs are relative to the anchor,
   so they survive a different exit-status marker, a different clock, or a
   different directory in the prompt.
4. **Predict.** When you press a key and the cache knows `(state, key)`, the
   diff is painted over the screen immediately and the key is sent. Only the
   changed cells are painted, so the prompt is never repainted from the local
   model. When the real bytes arrive, the confirmed model is updated; if it
   matches the predicted state the overlay is simply dropped, otherwise the
   overlay is restored and the real bytes win. Several keys can be in flight;
   each carries its expected state so bursts of typing ack correctly.
5. **Calibrate.** On connect, N hidden ssh sessions (multiplexed over the same
   connection, so they start in ~0.5 s) open the same shell in the same
   directory and type your history into it, char by char, never pressing
   Enter, learning every transition. By default 6 probes run for 12 s, then
   one keeps going in the background. Every command you execute is re-probed
   immediately (its autosuggestion just changed), and every unknown key you
   press is learned live from the real session. The cache is saved to
   `~/.cache/tns/<host>.bin` and reloaded next time. History comes from the
   remote shell's own history file (bash/zsh/fish).

Anything that owns the terminal (nvim, htop, ...) is passed through with plain
ssh latency; the alternate screen is detected and prediction pauses until the
prompt is back.

### Memory layout

The Rust version is built to stay small and allocation-free on the keystroke
path:

- A screen cell is 12 bytes (`char`, packed fg colour + attribute flags,
  packed bg colour); a 120x40 grid is 58 KB and copies with one memcpy. The
  predicted screen and the pre-keystroke snapshot are two persistent grids
  that are overwritten, never reallocated.
- Cache keys are 128-bit xxh3 hashes of the state text, not the text itself.
  Diffs are run-length encoded (one 14-byte run per stretch of changed cells
  with the same attributes, characters in a shared UTF-8 arena), so a typical
  entry costs ~50 bytes in memory and on disk.
- 256-colour and truecolour SGR are kept as indexed / RGB values, so predicted
  cells are painted with the same codes fish used (pyte turns indexed colours
  into RGB, which can differ from the terminal's palette).
- While an app holds the alternate screen the model ignores its output
  instead of keeping a second grid; the primary screen and cursor are simply
  left untouched until the app exits.

## Agents: local UI for a remote Claude Code, Codex, pi or opencode

A streaming agent TUI is the worst case for any remote terminal: the input
box, scrolling and permission prompts all wait a round trip, and the token
stream arrives in RTT-sized chunks. `tns agent` sidesteps the terminal
entirely: the agent runs headless on the remote in its structured stdio mode
and tns renders it locally.

```sh
tns agent claude obl --cwd ~/proj        # Claude Code (stream-json)
tns agent codex obl --cwd ~/proj         # Codex (app-server protocol)
tns agent pi obl                         # pi (RPC mode)
tns agent opencode obl                   # opencode (serve + SSE over an ssh port forward)
tns agent claude obl --resume SESSION    # continue a session (id is printed on exit)
tns agent claude --local                 # run the agent on this machine instead
tns agent claude obl -- --model sonnet   # anything after -- goes to the agent
```

What is local: the editor (multi-line, history, word ops), scrolling, the
permission prompt (`y` once, `a` always, `n` deny), and interrupts (Esc).
What crosses the wire: your submitted message, the answer to a prompt, an
interrupt, and the agent's events (token deltas, tool calls and results,
cost). Tool output is summarised to a few lines; anything the agent runs
that needs a real terminal is not shown here, use a plain `tns HOST` for
that. If the ssh link drops, `ctrl-r` reconnects and resumes the session.

Transport is ssh (multiplexed with the shell sessions), since the agents
speak over reliable pipes; roaming for agent sessions is on the list.

## Measure it

`bench.py` runs a terminal program in a pty, types a command one key at a
time and records, per key, how long until the character is echoed and how
long until the frame stops changing (the frame you actually end up seeing,
including autosuggestion and colours). `--rss` also samples peak memory.

```
./demo.sh HOST                 # ssh vs mosh vs tns (rust, ssh and mosh) vs tns (python)
./compare.sh HOST              # python vs rust: startup, micro benchmarks, live latency, memory
./bench.py --cmd "tns HOST" --type "cd co\t" --type "ls -lx\x7fa" --warm 4
```

Results on a ~60 ms Tailscale link to obl, typing `arelay --help` (a command
in history) with a warm cache:

| | char echo, median | final frame, median | final frame, p90 | peak RSS |
|---|---|---|---|---|
| ssh | 63 ms | 63 ms | 117 ms | |
| mosh | 0 ms | 0 ms | 129 ms | |
| tns (python) | 6 ms | 6 ms | 30 ms | 33 MB |
| tns (rust) | 0 ms | 0 ms | 0 ms | 3.9 MB |
| tns (rust, mosh) | 0 ms | 0 ms | 0 ms | 3.9 MB + mosh's 19 MB |

Mosh's 0 ms is the underlined bare character; its p90 is the autosuggestion
and colours arriving a round trip later (its *median* final frame for
`ls -la`, where every key changes the suggestion, is 100 ms). tns over mosh
keeps mosh's transport and replaces its prediction: in two runs of the same
workload all 22 keys were predicted and confirmed, and every frame was final
at 0 ms. The Python version spends ~2 ms per
key in the interpreter (pyte screen copy, JSON state key); the Rust version
paints before `bench.py` can measure a difference. Both use the same
calibration strategy, so hit rates are identical (21 of 22 keys predicted in
this run for both).

Hot-path micro benchmarks (`tns --bench` vs `tests/bench_micro.py`, same
278 KB capture of a real fish session, same 120x40 screen):

| | python (pyte) | rust | ratio |
|---|---|---|---|
| terminal emulation throughput | 3.6 MB/s | 420 MB/s | 115x |
| one keystroke: state key + lookup + apply diff + paint | 1650 µs | 3.9 µs | 420x |
| cache with 5000 entries, save + load | 52 ms | 0.7 ms | 75x |
| cache with 5000 entries on disk | 2.3 MB | 0.3 MB | 7x |
| process startup (`--help`) | 72 ms | 18 ms | 4x |

## Testing against the Python version

`tests/diff_pyte.sh` feeds a captured byte stream, and every prefix of it,
through pyte (exactly as `tns.py` does) and through `tns --dump-screen`, then
compares every cell (character, colours, attributes) and the cursor. Two
captures of real sessions are checked in under `tests/fixtures/`: a local
fish 4.8 session (wrapping, wide characters, ICH/DCH/IL/DL, scroll regions,
alternate screen) and a 100x30 session over ssh (fish 3.7, less).

```
cargo test                                            # unit tests
tests/diff_pyte.sh tests/fixtures/local.bin 80x24     # 147/147 prefixes match
tests/diff_pyte.sh tests/fixtures/remote.bin 100x30   # 272/272 prefixes match
tests/capture.py out.bin 80x24 -- fish                # record a new fixture (keys in $KEYS)
```

One known pyte divergence is filtered out by the reference dumper: pyte
parses xterm's `CSI > 4;1 m` (modifyOtherKeys, sent by fish 4) as SGR
bold+underline and then erases with those attributes.

## Known limits

- bash, zsh and fish get full prompt/command hooks; other shells use a generic
  fallback (echo and suggestions are predicted, but there is no cwd tracking).
- The state key ignores the working directory, so a `cd` suggestion learned in
  one directory may be predicted in another; the real bytes correct it within
  one round trip and the cache is updated.
- The screen is cleared on connect so the local model and the terminal agree
  on row numbers.
- Transport is ssh with multiplexing, so there is no roaming or UDP
  resilience yet. State lives on the server, so reconnecting is cheap, but
  automatic reconnect is not implemented.
- The cache contains screen text of your history (as hashes for the states,
  but the diffs hold the typed characters), so treat `~/.cache/tns/` like
  your fish history file. The Rust cache (`<host>.bin`) is separate from the
  Python one (`<host>.json`); each calibrates its own.
- Hidden probes never press Enter, but they do type your history into a live
  shell; abbreviations expand there as they would for you.
