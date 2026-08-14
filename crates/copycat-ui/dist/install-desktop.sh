#!/bin/sh
# Register copycat-ui as a desktop app for the CURRENT user, so searching
# "copycat" in your application menu launches the tray/history window. No root.
#
#   sh crates/copycat-ui/dist/install-desktop.sh              # add to app menu
#   sh crates/copycat-ui/dist/install-desktop.sh --autostart  # + launch on login
#   sh crates/copycat-ui/dist/install-desktop.sh --uninstall  # remove it
#
# Prereq: the `copycat-ui` binary should be on your PATH
#   (cargo install --path crates/copycat-ui --features gui). If it isn't yet,
#   this still installs the launcher entry and warns you.
set -eu

APP_ID="copycat-ui"
ICON_NAME="copycat"
DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
APPS="$DATA/applications"
ICONS="$DATA/icons/hicolor/scalable/apps"
AUTOSTART="${XDG_CONFIG_HOME:-$HOME/.config}/autostart"
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

refresh() {
  command -v update-desktop-database >/dev/null 2>&1 && \
    update-desktop-database "$APPS" 2>/dev/null || true
}

if [ "${1:-}" = "--uninstall" ]; then
  rm -f "$APPS/$APP_ID.desktop" "$ICONS/$ICON_NAME.svg" "$AUTOSTART/$APP_ID.desktop"
  refresh
  echo "Removed copycat from your app menu."
  exit 0
fi

# Resolve the binary so Exec= is an absolute path when we can find it; otherwise
# fall back to a bare PATH lookup at launch time.
bin=$(command -v copycat-ui 2>/dev/null || true)
[ -n "$bin" ] || bin="copycat-ui"

mkdir -p "$APPS" "$ICONS"
cp "$here/$ICON_NAME.svg" "$ICONS/$ICON_NAME.svg"
# Rewrite Exec= to the resolved binary path.
sed "s|^Exec=.*|Exec=$bin|" "$here/$APP_ID.desktop" > "$APPS/$APP_ID.desktop"
chmod 0644 "$APPS/$APP_ID.desktop"
refresh

if [ "${1:-}" = "--autostart" ]; then
  mkdir -p "$AUTOSTART"
  cp "$APPS/$APP_ID.desktop" "$AUTOSTART/$APP_ID.desktop"
  echo "Autostart enabled — copycat will launch on login."
fi

echo "Installed 'copycat' to your application menu ($APPS/$APP_ID.desktop)."
echo "If it doesn't show up immediately, log out/in (or restart your shell/panel)."
if ! command -v copycat-ui >/dev/null 2>&1; then
  echo
  echo "NOTE: 'copycat-ui' isn't on your PATH yet — build & install it first:"
  echo "  cargo install --path crates/copycat-ui --features gui"
  echo "  (needs GTK3 + GL dev libs — see crates/copycat-ui/dist/README.md)"
fi
