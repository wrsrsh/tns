#!/bin/sh
# Start each locally installed shell with the generated hooks in a pty and
# check that the prompt mark (OSC 133;A) and cwd mark (OSC 7771) come out.
cd "$(dirname "$0")/.."
BIN=${TNS_BIN:-target/release/tns}
d=$(mktemp -d); fail=0
for sh in bash zsh fish; do
  command -v $sh >/dev/null || { echo "$sh: not installed, skipped"; continue; }
  mkdir -p "$d/$sh"
  case $sh in
    bash) "$BIN" --print-hooks bash osc > "$d/$sh/init.bash"; cmd="bash --rcfile $d/$sh/init.bash -i";;
    zsh)  "$BIN" --print-hooks zsh osc > "$d/$sh/.zshrc"; cmd="env ZDOTDIR=$d/$sh zsh -i";;
    fish) "$BIN" --print-hooks fish osc > "$d/$sh/init.fish"; cmd="fish -C 'source $d/$sh/init.fish'";;
  esac
  DEADLINE=20 KEYS='echo tns-smoke|cd /tmp' python3 tests/capture.py "$d/$sh.bin" 80x24 -- sh -c "exec $cmd" >/dev/null 2>&1
  p=$(grep -c $'\x1b]133;A' "$d/$sh.bin" 2>/dev/null); c=$(grep -ac $'\x1b]7771;' "$d/$sh.bin"); x=$(grep -ac $'\x1b]7770;echo%20tns-smoke' "$d/$sh.bin")
  if [ "${p:-0}" -ge 2 ] && [ "${c:-0}" -ge 1 ] && [ "${x:-0}" -ge 1 ]; then echo "$sh: ok (prompt marks=$p cwd marks=$c exec marks=$x)"; else echo "$sh: FAIL (prompt=$p cwd=$c exec=$x)"; fail=1; fi
done
rm -rf "$d"; exit $fail
