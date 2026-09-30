# Benchmarks

These are historical development measurements, not installation requirements or
performance guarantees. Results depend on the host, network, terminal, cache,
and workload. For the Claude composer test, see [in-session prediction](in-session-prediction.md).


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
