# Benchmarks

These are historical development measurements, not installation requirements or
performance guarantees. Results depend on the host, network, terminal, cache,
and workload. For the Claude composer test, see [in-session prediction](in-session-prediction.md).

## Built-in mosh client compared with mosh-client — October 2, 2026

`tests/bench_latency.py` types an unfamiliar command (`grep -rn zylophone`,
120 ms per key) into bash through tns and through mosh 1.4.0's `mosh-client`.
Each client has its own real `mosh-server` on the same machine; a UDP proxy
delays every datagram in both directions. A key's latency is the time until
the cursor line of a pyte terminal fed with the client's output shows it.
The cache was empty and background calibration off (`--probes 0 --history 0`),
so every tns preview here is literal echo, not a cached redraw.

Median / worst latency per key, in ms, release build:

| Round trip | Client | Fresh line | First key at a new prompt | Typed right after Enter |
|---|---|---:|---:|---:|
| 20 ms | mosh, adaptive (its default) | 38 / 40 | 35 | 37 / 40 |
| | mosh, `--predict=always` | 3 / 36 | 37 | 3 / 37 |
| | tns | 4 / 37 | 3 | 3 / 58 |
| | tns, previews off | 36 / 38 | 36 | 36 / 58 |
| 60 ms | mosh, adaptive | 3 / 81 | 88 | 83 / 113 |
| | mosh, `--predict=always` | 4 / 78 | 78 | 83 / 115 |
| | tns | 3 / 76 | 4 | 3 / 113 |
| | tns, previews off | 80 / 104 | 77 | 82 / 117 |
| 300 ms | mosh, adaptive | 3 / 358 | 319 | 485 / 670 |
| | mosh, `--predict=always` | 4 / 351 | 321 | 417 / 520 |
| | tns | 3 / 315 | 3 | 4 / 538 |
| | tns, previews off | 430 / 520 | 317 | 450 / 535 |

Reading it:

- 3–4 ms is the floor of this measurement (pty, pyte and a 2 ms poll).
- **Fresh line.** Both clients wait one round trip for the first character of
  the session's first line, then echo locally. mosh's adaptive mode does not
  predict at all on a fast link, where the server's own frame pacing still
  costs about 35 ms per key.
- **First key at a new prompt.** mosh hides its guesses after every Enter
  until one is confirmed, so the first key of every command waits a round
  trip. tns knows from the shell's hook that this is a prompt, sees that it
  reads like one that has echoed before, and draws the key at once.
- **Typed right after Enter**, before the next prompt has arrived. mosh cannot
  place its guesses while earlier keys are unanswered and, at these typing
  speeds, never catches up within the command. tns lines the keys in flight
  up against the prompt once it appears; the worst key still waits for that.

These are loopback numbers with an artificial, loss-free delay, one run per
cell, plain bash, and no syntax highlighting or autosuggestions: they compare
the two clients' typing echo, not whole sessions. A separate run with fish
at a 300 ms round trip previewed 39 keys, all of which matched the server's
output. No measurement over a real network link has been made with the
built-in client yet.

## Shell cache misses — October 1, 2026

A live fish/Mosh session reproduced slower ordinary typing with the installed
0.6.1 client: a fresh 15-character token had a median visible echo of 51.5 ms,
with only one cache prediction. Plain Mosh's median was 0.1 ms. Disabling Mosh
prediction without a cache-independent shell fallback caused the difference.

After adding the literal-insertion fallback, two runs of the same development
build used different unfamiliar 18-character tokens and disabled background
calibration with `--probes 0 --history 0`:

| Shell literal fallback | Median visible character echo | Literal previews | Matches / discards |
|---|---:|---:|---:|
| Disabled (`--no-shell-prediction`) | 50.6 ms | 0 | 0 / 0 |
| Enabled | 0.1 ms | 16 | 16 / 0 |

The enabled run's first two characters were cached, establishing echo confidence;
the remaining 16 used the fallback. With a cold cache, the first two literal
echoes still wait for the remote. Tokens were cleared without pressing Enter.
The observer measured PTY output with Python/pyte at 120×40, with about 150 ms
of quiet between characters. These are visible-text timings, not final syntax
colors, autosuggestions, application typing, or GPU presentation. The existing
cache remained available in both runs, so this is a reproduction of unfamiliar
typing rather than a completely empty-cache comparison.

## Earlier measurements

`bench.py` runs a terminal program in a pty, types a command one key at a
time and records, per key, how long until the character is echoed and how
long until the frame stops changing (the frame you actually end up seeing,
including autosuggestion and colours). `--rss` also samples peak memory.

```sh
cargo build --release --locked
./bench.py --cmd "./target/release/tns HOST" --type "ls -la" --warm 4
./bench.py --cmd "./target/release/tns --ssh HOST" --type "ls -la" --warm 4
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
