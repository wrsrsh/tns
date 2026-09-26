#!/bin/sh
# Python (tns.py) vs Rust (target/release/tns): the same workload, side by side.
# Usage: ./compare.sh [host]      -- needs a warm cache for both (run each once first)
set -e
cd "$(dirname "$0")"
HOST=${1:?usage: compare.sh HOST}
PY="uv run --script tns.py $HOST"
RS="./target/release/tns $HOST"
RSM="./target/release/tns --mosh $HOST"
FIX=tests/fixtures/remote.bin

echo "### startup: time to print --help"
for c in "uv run --script tns.py" "./target/release/tns"; do
  s=$(python3 -c 'import time;print(time.time())'); $c --help >/dev/null 2>&1 || true
  python3 -c "import time;print('  %-28s %.0f ms' % ('$c'.split()[-1], (time.time()-$s)*1000))"
done

echo; echo "### micro benchmarks (emulator throughput, per-keystroke work, cache I/O)"
echo "python:"; uv run --script tests/bench_micro.py $FIX 2>/dev/null | sed 's/^/  /'
echo "rust:";   ./target/release/tns --bench $FIX | sed 's/^/  /'

echo; echo "### end to end against $HOST: keystroke latency and memory (warm cache)"
for name in python rust rust-mosh; do
  case $name in python) cmd=$PY;; rust) cmd=$RS;; rust-mosh) cmd=$RSM;; esac
  echo "--- $name"
  uv run --script bench.py --rss --cmd "$cmd" --type "arelay --help" --type "ls -la" --warm 4 2>&1 | grep -E 'median|peak rss|tns:'
done
