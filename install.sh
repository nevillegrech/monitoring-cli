#!/bin/sh
# install.sh — install dedaub-monitoring (macOS / Linux).
#
#   curl -LsSf https://raw.githubusercontent.com/Dedaub/monitoring-cli/main/install.sh | sh
#
# It builds the CLI from the public repository with cargo, installs the agent
# skill, and starts the login. Running it again upgrades an existing install.
set -eu

REPO="${DEDAUB_MONITORING_REPO:-https://github.com/Dedaub/monitoring-cli}"

info() { printf '\033[1;34m==>\033[0m %s\n' "$1"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$1" >&2; }
err()  { printf '\033[1;31merror:\033[0m %s\n' "$1" >&2; exit 1; }

# cargo installs into ~/.cargo/bin, which a fresh rustup has not put on PATH yet.
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

command -v cargo >/dev/null 2>&1 || err "cargo is required. Install Rust from https://rustup.rs, open a new terminal, and run this installer again."

info "Building dedaub-monitoring from $REPO (a few minutes the first time)…"
cargo install --locked --force --git "$REPO"

BIN="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}/bin/dedaub-monitoring"
[ -x "$BIN" ] || BIN="$(command -v dedaub-monitoring || true)"
[ -n "$BIN" ] || err "the build finished but dedaub-monitoring was not found. Add cargo's bin directory (~/.cargo/bin) to PATH."

# The name a new terminal can call it by. The Python CLI of 0.7 and earlier lives in
# ~/.local/bin, which often comes before cargo's directory on PATH.
DM=dedaub-monitoring
FIRST="$(command -v dedaub-monitoring || true)"
if [ -z "$FIRST" ]; then
  warn "$(dirname "$BIN") is not on PATH; add it to call the tool as dedaub-monitoring."
  DM="$BIN"
elif [ "$FIRST" != "$BIN" ] && ! [ "$FIRST" -ef "$BIN" ] 2>/dev/null; then
  warn "another dedaub-monitoring comes first on PATH: $FIRST"
  warn "if it is the Python CLI, remove it with: uv tool uninstall monitoring-cli"
  DM="$BIN"
fi

info "Installing the agent skill…"
"$BIN" install-skill || warn "could not install the skill; run '$DM install-skill' later."

info "Logging in (prints a URL to open in a browser; skip with Ctrl-C and run '$DM login' later)…"
"$BIN" login || warn "not logged in; run '$DM login' when ready."

info "Done. Try: $DM status && $DM guide"
