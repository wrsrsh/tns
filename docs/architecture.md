# How tns works

tns is a local client. Your remote shell and applications continue running on
the server. It does not provide a hosted service, require a server-side tns
daemon, or replace applications with a local UI in the normal connection path.

## Transport

SSH authenticates the connection and uploads temporary shell hooks. The default
interactive transport is mosh, with its roaming and state synchronization.
`--ssh` explicitly selects a plain SSH PTY instead. There is no automatic switch
of a live session between transports.

Mosh's own speculative echo is disabled because tns needs to distinguish the
real remote screen from its own previews. Unknown input still waits for the
remote response.

The mosh terminal model does not pass the shell's prompt markers through.
For that transport, hooks write events into the session's temporary directory;
tns follows them over an SSH side channel. Shell calibration sessions also use
SSH. This is why working non-interactive SSH authentication is required even
when the visible session uses mosh.

## Shell prediction

bash, zsh, and fish get per-session hooks that identify prompts, executed
commands, and working directories. Hooks live under `/tmp/tns-*` on the remote
and are removed when the session exits; they do not edit your shell profiles.
Other shells use a less capable generic fallback.

A local terminal model follows the confirmed screen. At a settled shell prompt,
tns identifies where editable input begins and looks up previously observed
screen changes for a given state and keystroke. A matching change can be painted
locally before the real reply arrives. Real output restores and replaces the
preview; uncertain or mismatched predictions are discarded.

The initial calibration uses up to six background shell sessions for roughly
12 seconds, then one continues learning. It types recent history but does not
press Enter. These are real interactive shells, so abbreviations and shell
configuration can still react to input. Use `--probes 0 --history 0` to disable
background calibration.

## Existing application TUIs

Normal remote TUIs are passed through rather than replaced. Initial application
prediction support targets Claude Code's recognizable single-line composer.
That layer uses a short-lived literal-edit model rather than the persisted
shell cache. It does not predict application decisions, model output, or tools.
See [in-session prediction](in-session-prediction.md) for its limits and tests.

The optional [agent interface](agents.md) is a different mode and is not used
when you run `claude` inside a normal `tns HOST` session.

## Local data

The shell prediction cache is stored under `~/.cache/tns`. Although lookup keys
are hashes, cached screen differences can contain typed text. Treat the cache
and debug logs like shell history. Working-directory changes can invalidate
assumptions; the remote display remains authoritative.

The original Python client is retained for differential tests. Current users
run the Rust executable. Implementation details and benchmark experiments are
kept out of the main installation path.
