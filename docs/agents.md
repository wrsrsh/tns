# Remote agents in Pi

`tns agent claude` and `tns agent codex` use **Pi's local interface**. A Pi
extension connects to TNS's SSH transport and existing agent adapters. Claude
Code or Codex runs on the remote machine, with its own authentication, tools,
configuration, and conversation history.

This is optional. To use an agent's original interface inside a terminal,
continue to run `tns HOST` and then `claude` or `codex` there.

## Install locally

Install [Pi](https://github.com/earendil-works/pi) (tested with 0.87.1), TNS,
and the extension on the computer where you type. Install and authenticate
Claude Code or Codex on the remote machine. SSH key authentication must work.
Mosh is not needed for this mode.

From this checkout:

```sh
cargo build --release --locked
pi install /absolute/path/to/tns
./target/release/tns agent claude user@server --cwd /path/on/server
./target/release/tns agent codex user@server --cwd /path/on/server
```

Once a version containing this extension is published, the Pi package can also
be installed with `pi install git:github.com/wrsrsh/tns`. For now, use the local
checkout so the extension and binary come from the same revision.

Pi needs no local model API key for these connections. Remote agent subscription
or API authentication stays on the remote machine.

## Connect and use

```sh
# With the updated tns binary on PATH:
tns agent claude user@server --cwd '/path/with spaces'
tns agent codex user@server --cwd /project
tns agent claude user@server --resume SESSION_ID
tns agent claude --local --cwd /project
tns agent claude user@server -- --model sonnet
```

Or start Pi normally and run `/tns`. Choose Claude or Codex, enter an SSH alias
(or `--local`), and enter the working directory. The extension uses this checkout's
debug or release binary when available, otherwise `tns` on PATH. Set
`TNS_BIN=/absolute/path/to/tns` to override it. `/tns` starts
a new remote conversation; it does not copy an existing local Pi conversation
to the remote agent. The SSH process starts with your first message.

Use Pi's normal editor and transcript navigation. Responses stream into native
Pi assistant messages. Remote tool activity appears as brief transcript notes;
Pi does not execute those tools locally. Approval requests open a Pi selector
with **Deny**, **Allow once**, and **Allow for session**. Dismissing the dialog
denies the request. Non-interactive runs deny approval requests automatically.

Esc cancels through Pi's normal abort handling. After a disconnect or interrupt,
your next message reconnects and resumes the saved remote session when its ID
is available. A failed prompt is not automatically replayed by the extension.
Resume the local Pi session with `pi --continue` or Pi's session selector to
restore its connection details and remote session ID. `/new` starts fresh remote
history on the same endpoint. To resume by a remote ID without a local transcript,
use `tns agent ... --resume ID`.

## Commands without collisions

Only `/tns` is registered by the extension. Pi keeps its own `/model`, `/help`,
`/settings`, `/new`, and other commands. Remote controls use subcommands:

| Command | Effect |
| --- | --- |
| `/tns model` | Open the remote agent's model picker |
| `/tns model ID` | Set the remote model for subsequent prompts |
| `/tns models` | Fetch the model catalog from the remote agent |
| `/tns compact` | Compact the remote conversation |
| `/tns commands` | Show commands supported through this connection |
| `/tns run /COMMAND args` | Run a discovered Claude command, skill, or MCP prompt |
| `/tns status` | Show the endpoint, saved session ID, and model override |
| `/tns native` | Temporarily open the original agent terminal interface |
| `/tns help` | Show this command guide |

Model selection is persisted with the Pi session and restored on reconnect.
Codex applies the selected model through its next `turn/start` request; Claude
acknowledges a `set_model` control request. Neither changes Pi's local model
selection. Model catalogs come from the agent on the connected machine.

Claude advertises the commands available in its headless session. `/tns run`
validates against that list and bypasses Pi's command/template expansion, so a
remote `/context` or custom skill does not become a Pi command. Unknown commands
are rejected rather than sent as ordinary model prompts. Codex's native slash
commands are implemented by its TUI; the extension maps model selection and
compaction to app-server requests instead of forwarding slash-command text.

Not every native feature exists in these headless protocols. Use `/tns native`
for native settings, permissions configuration, reasoning controls, login flows,
interactive tools, and other original UI features. Pi releases the terminal while
the native interface runs and returns when it exits. With a saved remote session
ID, the original UI resumes that conversation, and the next Pi prompt resumes it
again. Native turns stay in the remote history; they are not copied into Pi's
transcript. If no remote ID exists yet, native mode opens a separate session;
use its ID with `tns agent ... --resume ID` to bring it back into Pi.

Native mode carries the host, directory, session ID, and selected model. Additional
headless CLI arguments are not copied because they may be invalid in the native
UI. On return, TNS clears its model override so a model change in the native
session can take effect. Authentication and other defaults come from that agent's
configuration on the remote host.

Protocol references: [Codex App Server](https://learn.chatgpt.com/docs/app-server)
and [Claude SDK commands](https://code.claude.com/docs/en/agent-sdk/slash-commands).

## Limits

- Text prompts only; images and other attachments are not forwarded.
- Remote agents own tools, instructions, model settings, and history. Pi's local
  tools are disabled while the TNS provider is selected. Pi's system prompt,
  local history, and thinking selector are not forwarded.
- Pi's fork, tree-rewind, and compaction operations are blocked for the TNS
  provider because they cannot rewrite the remote conversation. Use `/tns compact`
  for remote compaction or `/tns native` for native history controls.
- Pi's token/cost counters are zero placeholders, not billing information.
  Remote status/cost summaries are shown when the adapter provides them.
- Interactive terminal programs and arbitrary agent input forms are not supported
  in the Pi transcript. Use `/tns native` for the original agent UI.
- Cancellation sends an interrupt and closes the transport; remote work may have
  already changed files. Resume and inspect the remote session before retrying.

## Development and compatibility

To load the extension without installing the package:

```sh
TNS_BIN="$PWD/target/debug/tns" pi -e ./extensions/pi/index.ts
```

The development binary finds its source extension automatically. An installed
binary uses the Pi package; `TNS_PI_EXTENSION=/absolute/path/to/index.ts` overrides
extension discovery. Other Pi extensions remain enabled.

The original TNS interface remains available with `--legacy-ui`:

```sh
tns agent claude user@server --legacy-ui
tns agent codex user@server --legacy-ui
tns agent pi user@server
tns agent opencode user@server
```

Pi and OpenCode backends continue to use that original interface by default.

`tns agent --bridge <claude|codex> HOST` is the extension's JSON-lines transport.
It accepts `send` (`text`), `answer` (`id`, `reply`: `allow`, `allow_always`, or
`deny`), `interrupt`, `control` (`id`, `action`, optional `value`), and `close`
commands. Controls support `models` and `model` for both agents, and `commands`
for Claude. Replies are `control_result` events with `id`, `result`, and `error`.
All events use `type` and optional `data`.
