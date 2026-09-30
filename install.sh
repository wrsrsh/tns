#!/bin/sh
# Explicit machine roles: sh install.sh local | remote [--check]
# No keys, SSH configuration, services, firewall rules, or shell profiles are changed.
set -eu

REPO=wrsrsh/tns
TAP=wrsrsh/tap/tns
TNS_VERSION=${TNS_VERSION:-0.6.0}
ROLE=
CHECK=false

usage() {
  cat <<'EOF'
usage: sh install.sh local [--check]
       sh install.sh remote [--check]

local   Run on the computer you type on. Install tns and mosh.
remote  Run on the server you connect to. Install mosh only; no tns or Rust.

--check  Report requirements without installing or changing anything.
--help   Show this help.

SSH access, key authentication, UTF-8 locales, and firewall policy are configured
separately. After installation, run `tns setup user@server` on the LOCAL machine.
EOF
}

die() { printf '\nError: %s\n' "$*" >&2; exit 1; }
has() { command -v "$1" >/dev/null 2>&1; }
step() { printf '\n%s\n' "$*"; }
run() {
  printf '  $ %s\n' "$*"
  "$@" || { status=$?; printf 'Command failed (exit %s): %s\n' "$status" "$*" >&2; exit "$status"; }
}

parse_args() {
  for arg do
    case "$arg" in
      local|remote) [ -z "$ROLE" ] || { usage >&2; exit 2; }; ROLE=$arg ;;
      --check) CHECK=true ;;
      -h|--help) usage; exit 0 ;;
      *) printf 'Unknown argument: %s\n\n' "$arg" >&2; usage >&2; exit 2 ;;
    esac
  done
  [ -n "$ROLE" ] || { printf 'Choose the machine you are preparing: local or remote.\n\n' >&2; usage >&2; exit 2; }
}

detect_os() {
  case "$(uname -s)" in
    Darwin) OS=darwin ;;
    Linux) OS=linux ;;
    *) die 'Supported systems are macOS and Linux (including WSL). Nothing was installed.' ;;
  esac
}

detect_pkg() {
  for p in brew apt-get dnf yum pacman apk zypper; do
    if has "$p"; then printf '%s\n' "$p"; return; fi
  done
  printf '\n'
}

as_root() {
  if [ "$(id -u)" -eq 0 ]; then
    run "$@"
  elif has sudo; then
    run sudo "$@"
  else
    die "Root access is needed to run: $*. Install the package as an administrator, then retry."
  fi
}

install_mosh() {
  pkg=$(detect_pkg)
  step "Installing mosh on this $ROLE machine"
  case "$pkg" in
    brew) run brew install mosh ;;
    apt-get) as_root apt-get update; as_root apt-get install -y mosh ;;
    dnf|yum) as_root "$pkg" install -y mosh ;;
    pacman) as_root pacman -S --needed --noconfirm mosh ;;
    apk) as_root apk add mosh ;;
    zypper) as_root zypper --non-interactive install mosh ;;
    *) die 'No supported package manager found. Install mosh manually, then rerun this installer.' ;;
  esac
}

has_utf8() {
  locale -a 2>/dev/null | grep -Eiq 'utf[-_]?8'
}

check_local() {
  missing=0
  step 'Local machine: runtime requirements'
  for tool in tns ssh mosh; do
    if has "$tool"; then
      printf '  [ok] %s: %s\n' "$tool" "$(command -v "$tool")"
    else
      printf '  [missing] %s\n' "$tool"
      missing=1
    fi
  done
  if locale charmap 2>/dev/null | grep -Eiq 'utf[-_]?8'; then
    printf '  [ok] active UTF-8 locale\n'
  else
    printf '  [missing] active UTF-8 locale; choose an installed locale (locale -a)\n'
    missing=1
  fi
  printf '\nRemote machine: not checked.\n'
  printf 'Next, from this LOCAL machine: tns setup user@server\n'
  return "$missing"
}

check_remote() {
  missing=0
  step 'Remote machine: runtime requirements'
  if has mosh-server; then
    printf '  [ok] mosh-server: %s\n' "$(command -v mosh-server)"
  else
    printf '  [missing] mosh-server; install the mosh package on this machine\n'
    missing=1
  fi
  if has_utf8; then
    printf '  [ok] an installed UTF-8 locale is available\n'
  else
    printf '  [missing] UTF-8 locale\n'
    printf '  On Debian/Ubuntu: sudo locale-gen en_US.UTF-8\n'
    missing=1
  fi
  printf '  [info] SSH service, authentication, and UDP connectivity were not checked.\n'
  printf '  [info] tns and Rust are not needed on this remote machine.\n'
  printf '\nNext, on your LOCAL machine:\n  ssh user@server\n  tns setup user@server\n'
  printf 'Install and sign in to tools such as Claude on the REMOTE machine, if you use them.\n'
  return "$missing"
}

install_local() {
  if has brew; then
    step 'Installing the local client with Homebrew (includes mosh)'
    if brew list --versions "$TAP" >/dev/null 2>&1; then
      run brew upgrade "$TAP"
    else
      run brew install "$TAP"
    fi
    tns_bin="$(brew --prefix)/bin/tns"
  else
    [ "$OS" = linux ] || die 'Install Homebrew from https://brew.sh, then rerun: sh install.sh local'
    has cargo || die 'A Rust toolchain is required for the Linux source build. Install it from https://rustup.rs, then retry. No toolchain was installed automatically.'
    has cc || die 'A C compiler/linker (cc) is required. On Debian/Ubuntu: sudo apt-get install build-essential'
    has ssh || die 'Install an SSH client first. On Debian/Ubuntu: sudo apt-get install openssh-client'
    printf '%s\n' "$TNS_VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || die 'TNS_VERSION must be a release version such as 0.6.0'
    has mosh || install_mosh
    root=${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}
    step "Building the local client from release v$TNS_VERSION"
    run cargo install --git "https://github.com/$REPO" --tag "v$TNS_VERSION" --locked --root "$root"
    tns_bin="$root/bin/tns"
  fi
  [ -x "$tns_bin" ] || die "Installation did not produce an executable at $tns_bin"
  version=$("$tns_bin" --version) || die 'The installed tns executable could not run'
  step "Installed: $version"
  printf '  Location: %s\n' "$tns_bin"
  resolved=$(command -v tns 2>/dev/null || true)
  if [ -z "$resolved" ] || ! [ "$resolved" -ef "$tns_bin" ]; then
    printf '\nYour shell does not resolve tns to this installation.\n'
    printf 'For bash/zsh, run: export PATH="%s:$PATH"\n' "$(dirname "$tns_bin")"
    printf 'For fish, run: fish_add_path "%s"\n' "$(dirname "$tns_bin")"
    printf 'Then run: tns setup user@server\n'
    exit 1
  fi
  check_local
}

main() {
  parse_args "$@"
  detect_os
  printf 'tns installation\n================\nTarget: %s machine (this computer).\n' "$ROLE"
  printf 'No SSH keys, service configuration, firewall rules, or shell profiles will be changed.\n'
  if [ "$CHECK" = true ]; then
    printf 'Check only: no packages will be installed.\n'
    case "$ROLE" in local) check_local ;; remote) check_remote ;; esac
    return
  fi
  case "$ROLE" in
    local) install_local ;;
    remote)
      step 'Preparing the remote machine: mosh only'
      has mosh-server || install_mosh
      check_remote
      ;;
  esac
}

main "$@"
