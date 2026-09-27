#!/bin/sh
# tns installer.  Usage:
#   curl -fsSL https://raw.githubusercontent.com/wrsrsh/tns/main/install.sh | sh
#
# Installs tns and mosh, then runs `tns setup` to get your first host ready.
# Set TNS_INTERACTIVE=false to install without launching setup.
set -eu

REPO="wrsrsh/tns"
TAP="wrsrsh/tap/tns"

main() {
  os=$(detect_os)
  say_banner
  case "$os" in
  darwin) install_macos ;;
  linux) install_linux ;;
  esac
  ensure_path
  finish
}

detect_os() {
  case "$(uname -s)" in
  Linux*) echo linux ;;
  Darwin*) echo darwin ;;
  *)
    echo "tns: unsupported OS: $(uname -s). Build from source: https://github.com/$REPO" >&2
    exit 1
    ;;
  esac
}

has() { command -v "$1" >/dev/null 2>&1; }

say() { printf '%s\n' "$*"; }
step() { printf '\n\033[36m==>\033[0m \033[1m%s\033[0m\n' "$*"; }

say_banner() {
  printf '\033[1m\033[36mtns\033[0m — a predictive, roaming terminal for a remote shell\n'
}

# ---------- macOS: Homebrew tap (pulls mosh + a rust build toolchain)
install_macos() {
  if has tns; then
    step "tns is already installed ($(tns --version 2>/dev/null || echo present)); upgrading"
    brew upgrade "$TAP" 2>/dev/null || true
    return
  fi
  if ! has brew; then
    step "Installing Homebrew (needed to build tns)"
    /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
    eval "$(/opt/homebrew/bin/brew shellenv 2>/dev/null || /usr/local/bin/brew shellenv)"
  fi
  step "Installing tns and mosh with Homebrew"
  brew install "$TAP"
}

# ---------- Linux: mosh from the package manager, tns from source
install_linux() {
  pkg=$(detect_pkg)
  if [ -n "$pkg" ] && ! has mosh; then
    step "Installing mosh with $pkg"
    run_pkg_install "$pkg" mosh || say "tns: could not install mosh automatically; install it yourself later"
  fi
  if ! has cargo; then
    step "Installing the Rust toolchain (to build tns)"
    curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal
    # shellcheck disable=SC1090
    . "$HOME/.cargo/env"
  fi
  step "Building and installing tns"
  cargo install --git "https://github.com/$REPO" --locked
}

detect_pkg() {
  for p in apt-get dnf yum pacman apk zypper; do
    if has "$p"; then
      echo "$p"
      return
    fi
  done
  echo ""
}

run_pkg_install() {
  pkg=$1
  shift
  sudo=""
  [ "$(id -u)" -eq 0 ] || sudo="sudo"
  case "$pkg" in
  apt-get) $sudo apt-get update && $sudo apt-get install -y "$@" ;;
  dnf) $sudo dnf install -y "$@" ;;
  yum) $sudo yum install -y "$@" ;;
  pacman) $sudo pacman -S --noconfirm "$@" ;;
  apk) $sudo apk add "$@" ;;
  zypper) $sudo zypper install -y "$@" ;;
  esac
}

ensure_path() {
  # cargo installs to ~/.cargo/bin; make sure it is reachable this session
  case ":$PATH:" in
  *":$HOME/.cargo/bin:"*) : ;;
  *)
    if [ -x "$HOME/.cargo/bin/tns" ]; then
      export PATH="$HOME/.cargo/bin:$PATH"
      say "Add this to your shell profile so tns is always on your PATH:"
      say "  export PATH=\"\$HOME/.cargo/bin:\$PATH\""
    fi
    ;;
  esac
}

finish() {
  if ! has tns; then
    say "tns: install finished but 'tns' is not on your PATH yet; open a new shell and try 'tns setup'."
    return
  fi
  step "Installed: $(tns --version)"
  if [ "${TNS_INTERACTIVE:-}" = "false" ] || [ ! -t 0 ]; then
    say "Next: run 'tns setup' to connect your first host."
    return
  fi
  say "Launching setup..."
  tns setup </dev/tty
}

main "$@"
