# How tns works

tns is a local client. Your remote shell and applications continue running on
the server. It does not provide a hosted service, require a server-side tns
daemon, or replace applications with a local UI in the normal connection path.

## Transport

SSH authenticates the connection, uploads temporary shell hooks and starts
`mosh-server`. The interactive session then runs over mosh's protocol, with
its roaming and state synchronization. tns is the mosh client: it exchanges
the encrypted UDP datagrams with the unmodified server itself, and no
`mosh-client` process is involved. `--ssh` explicitly selects a plain SSH PTY
instead, and `--mosh-client` the older arrangement of running the `mosh`
program in a PTY. There is no automatic switch of a live session between
transports.

The server does not send a byte stream. It sends numbered screen states, each
as a difference from a state the client has acknowledged, and with each one
the number of the newest keystroke state it has passed to the application and
given 50 ms to answer. tns keeps the acknowledged states, applies each
difference to the one it refers to, and updates the terminal from the
difference between the state on screen and the newest one. Lost or reordered
datagrams therefore never corrupt the display. Input travels the same way in
the other direction, at most 512 KiB ahead of what the server has
acknowledged. While the server is silent for
more than a few seconds, the first screen row says so; `Ctrl-^ .` quits.

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
preview; uncertain or mismatched predictions are discarded. On the built-in
mosh transport a prompt is only located once the server has acknowledged the
key that ended the previous command, so late echoes of earlier typing are
not mistaken for the new prompt's screen.

## Literal echo

Keys the cache does not know are echoed locally on the built-in mosh
transport: a printable character is drawn at the cursor, Backspace erases, and
the left and right arrows move, before the server replies. Cached redraws keep
priority. Every key travels in a numbered input state, and the server's
acknowledgment says when the confirmed screen includes the application's
answer to it. A guess is then either on the screen or wrong; a wrong guess
withdraws all guesses in flight.

Guesses are grouped in epochs. Enter, control keys, escape sequences, pastes
and anything else whose effect is not modelled start a new epoch; so does the
application moving the cursor on its own. An epoch's guesses stay hidden until
one of them appears on the real screen, in a cell that did not already hold
that character, with the cursor directly behind it. Text that merely lands on
a guessed cell, such as a prompt being printed, does not count, and neither
does a mask character like `*`. A guess the cursor has passed without leaving
the typed character is wrong at once, before any acknowledgment. Input into
something that does not echo is therefore not drawn.

The one shortcut is a shell prompt reported by the shell's own hook. The
report and the screen travel separately, so the settled screen is only
trusted if the text in front of the cursor is the same as at a prompt that
has echoed before, in a session where such prompts have echoed twice without
a wrong guess. There the first key is drawn at once. A prompt that reads
differently (another directory, another program's prompt reached by typing
ahead) has to be seen echoing first. Shells without hooks never get the
shortcut.

What remains is the limit mosh has too: when a program stops echoing in the
middle of a confirmed epoch, with no unmodelled key and no cursor movement in
between, keys already in flight are drawn until the program's next frame or
their acknowledgment (a round trip plus 50 ms) withdraws them.

A guess needs the cursor position its key will meet. While earlier keys are
unanswered, as when typing straight after Enter, plain characters are only
remembered. Once Enter is acknowledged, they are lined up against the text in
front of the cursor: those already on screen are skipped and the rest become
ordinary hidden guesses, shown as soon as the next one is echoed. If the
screen had not caught up yet, they are lined up again. Wrapping at the right
margin, wide characters and hidden cursors are not guessed.
`--no-shell-prediction` disables literal echo at shell prompts and
`--no-tui-prediction` everywhere else. Exit statistics report it separately
from cached redraws.

Without acknowledgments (`--ssh`, `--mosh-client`), cache misses have a
narrower in-memory fallback at confirmed bash, zsh, and fish prompts. Two
observed text/cursor matches establish confidence within the current prompt;
then ordinary ASCII characters can be previewed without a transition for that
exact command. Confirmation compares the typed prefix and cursor, ignoring
changing syntax colors and autosuggestions; existing confirmed cells keep
their attributes. Controls, backspace, navigation,
bulk input, Unicode, wrapping, hidden cursors, rewrites, prompt changes, and
resizing cancel this editing epoch. Pending edits are bounded to 64 and expire
after 0.5–2 seconds; fragmented terminal sequences block painting.
`--no-shell-prediction` disables this fallback as well.

The initial calibration uses up to six background shell sessions for roughly
12 seconds, then one continues learning. It types recent history but does not
press Enter. These are real interactive shells, so abbreviations and shell
configuration can still react to input. Use `--probes 0 --history 0` to disable
background calibration.

## Existing application TUIs

Normal remote TUIs are shown as they are rather than replaced. On the built-in
mosh transport, typing into them is covered by the literal echo above, for any
application that echoes at the cursor. On the other transports, application
prediction targets Claude Code's recognizable single-line composer with a
short-lived literal-edit model. Neither uses the persisted shell cache or
predicts application decisions, model output, or tools.
See [in-session prediction](in-session-prediction.md) for limits and tests.

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
