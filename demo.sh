#!/bin/sh
# Head-to-head keystroke latency: ssh vs mosh vs tns (rust) vs tns (python) on the same host.
# Usage: ./demo.sh [host] [text to type]
set -e
cd "$(dirname "$0")"
HOST=${1:?usage: demo.sh HOST}
TEXT=${2:-"arelay --help"}
RS=./target/release/tns
[ -x "$RS" ] || cargo build --release

run() {
  name=$1; shift
  printf '\n### %s\n' "$name"
  uv run --script bench.py --type "$TEXT" "$@" 2>&1 | grep -E 'final row|median|peak rss|tns:'
}

run "ssh  $HOST" --cmd "ssh $HOST"
run "mosh $HOST" --cmd "mosh $HOST" --warm 3
run "tns  $HOST rust   (cold cache would take ~12 s to calibrate)" --cmd "$RS $HOST" --warm 4 --rss
run "tns  $HOST rust over mosh" --cmd "$RS --mosh $HOST" --warm 4 --rss
run "tns  $HOST python (reference implementation)" --cmd "uv run --script tns.py $HOST" --warm 4 --rss

printf '\n### tns (rust): a command that is not in history, typed twice\n'
uv run --script bench.py --cmd "$RS $HOST" --warm 3 --run --quiet-screen \
  --type "echo the quick brown fox $(date +%s)" --type "echo the quick brown fox $(date +%s)" 2>&1 \
  | grep -E 'typing|median|tns:'
