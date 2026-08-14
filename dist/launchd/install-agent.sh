#!/bin/sh
# Install (or remove) the copycatd launchd user agent — the macOS equivalent of
# `systemctl --user enable --now copycatd`.
#
#   sh dist/launchd/install-agent.sh              # install and start
#   sh dist/launchd/install-agent.sh --uninstall  # stop and remove
#
# A LaunchAgent (not a LaunchDaemon) is the right unit: copycatd is per-user and
# must reach the pasteboard, which only exists inside a GUI login session. A
# system-wide LaunchDaemon runs outside any session and could never attach one.
#
# Env overrides:
#   COPYCATD_BIN=/path/to/copycatd   default: first copycatd found on PATH,
#                                    then ~/.local/bin, then ~/.cargo/bin
set -eu

LABEL="io.github.deanott.copycatd"
PLIST_DIR="$HOME/Library/LaunchAgents"
PLIST="$PLIST_DIR/$LABEL.plist"
LOG_DIR="$HOME/Library/Logs/copycat"
TEMPLATE=$(dirname "$0")/"$LABEL.plist"

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = "Darwin" ] || die "this installs a launchd agent; it is macOS-only.
  On Linux use dist/systemd/copycatd.service instead."

# `bootout` on a not-loaded agent exits non-zero; that is not an error here.
unload_agent() {
  if launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1; then
    launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
  fi
}

if [ "${1:-}" = "--uninstall" ]; then
  unload_agent
  rm -f "$PLIST"
  say "Removed $LABEL. copycatd will not start at login."
  say "(Any copycatd started by hand is still running: pkill -f copycatd)"
  exit 0
fi

# --- locate the binary; the plist needs an absolute path ---
bin="${COPYCATD_BIN:-}"
if [ -z "$bin" ]; then
  for cand in "$(command -v copycatd 2>/dev/null || true)" \
              "$HOME/.local/bin/copycatd" \
              "$HOME/.cargo/bin/copycatd"; do
    [ -n "$cand" ] && [ -x "$cand" ] && { bin="$cand"; break; }
  done
fi
[ -n "$bin" ] || die "no copycatd found. Install it first (dist/install.sh, or
  'cargo install --path crates/copycatd'), or set COPYCATD_BIN=/path/to/copycatd."
[ -x "$bin" ] || die "'$bin' is not executable"

# launchd resolves nothing: a relative path here yields a silent boot failure.
case "$bin" in
  /*) ;;
  *) bin=$(cd "$(dirname "$bin")" && pwd)/$(basename "$bin") ;;
esac

[ -f "$TEMPLATE" ] || die "template '$TEMPLATE' not found (run this from the repo)"

mkdir -p "$PLIST_DIR" "$LOG_DIR"

# Replace an existing agent rather than layering a second copy on top.
unload_agent

sed -e "s|__COPYCATD_BIN__|$bin|g" \
    -e "s|__LOG_DIR__|$LOG_DIR|g" \
    "$TEMPLATE" > "$PLIST"

plutil -lint "$PLIST" >/dev/null || die "generated plist is malformed: $PLIST"

launchctl bootstrap "gui/$(id -u)" "$PLIST" \
  || die "launchctl bootstrap failed. If macOS blocked it, grant your terminal
  Developer Tools / Full Disk Access, or load it from a login shell."
launchctl enable "gui/$(id -u)/$LABEL" 2>/dev/null || true

say ""
say "Installed $LABEL"
say "  binary : $bin"
say "  plist  : $PLIST"
say "  log    : $LOG_DIR/copycatd.log"
say ""
say "copycatd now starts at login and restarts if it exits."
say "  status    : launchctl print gui/$(id -u)/$LABEL | head -20"
say "  stop      : launchctl bootout gui/$(id -u)/$LABEL"
say "  uninstall : sh dist/launchd/install-agent.sh --uninstall"
say ""
say "Verify with:  copycat status"
