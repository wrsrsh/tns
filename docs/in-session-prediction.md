# Predicting an existing TUI

## Workflow

Run a normal `tns HOST` session and type `claude` in its remote shell.
The application is Claude Code's original TUI. There is no headless agent
adapter, local transcript renderer, command wrapper, or second interface.

Available in tns 0.5.0 and later. For a source build:

```sh
cargo build --release --locked
./target/release/tns HOST
# At the remote shell prompt:
claude
```

## What this implementation does

The first recognizer targets Claude Code's single-line, horizontally bordered
composer, its `❯` prompt, visible cursor, and version banner. After recognizing
the application, the banner need not remain visible while its transcript
scrolls. A return to the shell or resize resets recognition.

Two observed literal-edit matches establish confidence, normally following an
initial unpredicted character that replaces the placeholder. Subsequent
ordinary ASCII insertion and backspace are painted over the existing input
row immediately. All input bytes are still sent to the original application,
unchanged and exactly once. No messages, Enter keys, permission decisions,
API calls, or hidden calibration sessions are generated.

Incoming remote output is authoritative:

- Restore the real cells, cursor, and SGR attributes before passing it through.
- Match text and cursor against pending edits, including coalesced echoes.
- Retain pending previews when unrelated output leaves the field unchanged.
- Discard the editing epoch when the field is rewritten or its layout changes.
- Never confirm an edit cycle against an indistinguishable old screen.
- Bound pending edits to 64 and their lifetime to 0.5–2 seconds, depending on
  the existing RTT estimate.
- Never insert a preview inside a fragmented UTF-8 or terminal control sequence.

This is deliberately not generic speculation about an application's behavior.
It will not predict menus, permission prompts, submission, slash/bang modes,
mention pickers, pasted blocks, Unicode/grapheme editing, wrapping, or multiline
layout. Navigation keys reset confidence. The first edit at column zero is
unpredicted because it is ambiguous with a placeholder.

Text/cursor matches are **observations, not server-side input acknowledgments**.
They do not prove arbitrary remote application semantics. The conservative
recognizer, confidence resets, bounds, and opt-out are important. Layout
changes in other Claude versions can disable prediction rather than requiring
tns to invent a new UI. This is initial experimental support, not a promise
that every Claude interaction or other TUI will be accelerated.

`tns --no-tui-prediction HOST` disables this layer without changing the
transport or shell prediction. Mosh remains the default transport; its own
prediction remains disabled to avoid treating guesses as real remote output.
TUI input is held only in the current in-memory editing epoch, not written to
the persistent shell prediction cache. Debug logs record preview/expiry events,
not the contents of the TUI input.

## Live validation — September 30, 2026

Tested the installed **Claude Code v2.1.285** inside a normal tns/Mosh session
to the existing remote, in an already-trusted workspace. No trust dialog was
accepted and no prompt was submitted. Typed 16 ASCII characters into Claude's
composer, cleared them, and exited.

Two runs used the same development binary, with prediction disabled/enabled:

| Measurement | Disabled | Enabled |
|---|---:|---:|
| Median key-to-visible-echo after the first 3 characters | 65.05 ms | 5.16 ms |
| Locally previewed edits | 0 | 13 |
| Previews matching subsequent real output | 0 | 13 |
| Discarded previews | 0 | 0 |

The timings include the Python/pyte observer, terminal I/O, and scheduling.
They measure this typing workload, not model inference, tool execution,
whole-screen GPU presentation, or universal TUI performance. Cold characters
still waited for the remote; no claims of zero network latency are made.

## Automated validation

```sh
cargo test --locked
cargo build --release --locked
TNS_BIN=target/release/tns uv run --script tests/tui_prediction.py
TNS_BIN=target/release/tns python3 tests/client_regressions.py
```

The new PTY tests type `claude` at a fake remote shell prompt, then exercise
a terminal application with the observed composer shape. They do **not** use
`tns agent` or call Claude's API.

A held-echo test proves that the preview appears before any real echo and
verifies the exact bytes received by the application. Other tests cover cold
fields, opt-out, missing echoes, rollback, input rewrites, menus, backspace,
submission, and ambiguous edit cycles. Unit tests additionally cover control
string boundaries, attributes, cursor visibility, width limits, styled/Unicode
fields, resizing, pending limits, and coalesced confirmations.
