#!/bin/sh
# A guided demo of `tns setup`, the interactive wizard that gets a host ready.
# Usage: ./setup-demo.sh [HOST]
#
# This runs the REAL wizard against the host you give (or lets you pick one
# from ~/.ssh/config). Nothing here is faked: every ✓ is a real check, and any
# key or mosh install is the wizard doing it, prompting you before each action.
set -e
cd "$(dirname "$0")"
TNS=${TNS:-./target/release/tns}
[ -x "$TNS" ] || TNS=tns

cat <<'TXT'
tns setup — walkthrough

The wizard runs four steps and prints the command it will run before each one:

  1. Choose the host   pick a ~/.ssh/config alias, or type user@host
  2. Connect           if ssh key login fails, offer to create an ed25519 key
                       and copy it (one password prompt), or use a password
  3. Check & install   probe the remote login shell and UTF-8 locale, then
                       install mosh with the remote's package manager if missing
  4. Ready             print how to connect

Against a host that is already reachable with mosh installed, all four steps
pass immediately, like this:

  [1/4] Choose the host
    using myserver
  [2/4] Connect
    ✓ ssh key login already works
  [3/4] Check the remote and install mosh
    ✓ login shell: /usr/bin/fish (fish, predictions supported)
    ✓ UTF-8 locale present
    ✓ mosh-server is already installed
    ✓ mosh is installed locally
  [4/4] Ready
    ✓ You're set. Connect with:  tns myserver

On a fresh host, step 2 shows an auth menu and step 3 offers to run, e.g.,
`sudo apt-get install -y mosh` over ssh (it asks first).

TXT

printf 'Run the real wizard now? [Y/n]: '
read ans || ans=n
case "$ans" in
n | N | no | NO) echo "Skipped. Run it any time with:  tns setup"; exit 0 ;;
esac

echo "----------------------------------------------------------------"
exec "$TNS" setup "$@"
