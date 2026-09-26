#!/bin/sh
# Differential test: feed a captured byte stream (and every prefix at STEP
# byte intervals) through pyte (tns.py's model) and tns --dump-screen; report
# mismatches.   usage: tests/diff_pyte.sh CAPTURE.bin COLSxROWS [STEP]
set -e
cd "$(dirname "$0")/.."
BIN=${TNS_BIN:-target/release/tns}
f=$1; size=$2; step=${3:-512}
d=$(mktemp -d)
cuts=$(uv run --script tests/pyte_dump.py "$size" "$f" "$step" "$d" 2>/dev/null)
fail=0; total=0
for cut in $cuts; do
  total=$((total+1))
  head -c "$cut" "$f" | "$BIN" --dump-screen "$size" > "$d/rust-$cut.txt"
  if ! cmp -s "$d/pyte-$cut.txt" "$d/rust-$cut.txt"; then
    fail=$((fail+1))
    echo "MISMATCH at prefix $cut bytes:"
    diff "$d/pyte-$cut.txt" "$d/rust-$cut.txt" | head -6
  fi
done
rm -rf "$d"
echo "$f: $((total-fail))/$total prefixes match"
