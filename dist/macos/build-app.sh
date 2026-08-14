#!/bin/sh
# Build Copycat.app — the macOS bundle for the copycat-ui tray/menu-bar client.
#
#   sh dist/macos/build-app.sh                  # build into target/macos/Copycat.app
#   sh dist/macos/build-app.sh --install        # ...and copy into /Applications
#   sh dist/macos/build-app.sh --menubar-only   # LSUIElement: no Dock tile at all
#
# Why a bundle at all: without one, copycat-ui is a bare Mach-O executable.
# macOS will run it, but it is not an "app" — Spotlight and Launchpad cannot see
# it, Finder cannot launch it, System Settings cannot open it at login, and the
# Dock shows a generic executable icon. All of that comes from Contents/Info.plist
# and Contents/Resources, not from anything the Rust side can set: eframe's
# ViewportBuilder::with_icon is a no-op on macOS.
#
# Env overrides:
#   COPYCAT_UI_BIN=/path/to/copycat-ui   use a prebuilt binary instead of cargo
set -eu

REPO=$(cd "$(dirname "$0")/../.." && pwd)
OUT_DIR="$REPO/target/macos"
APP="$OUT_DIR/Copycat.app"
ICON_SRC="$REPO/dist/macos/copycat-icon-1024.png"
PLIST_SRC="$REPO/dist/macos/Info.plist"

install_it=0
menubar_only=0
for arg in "$@"; do
  case "$arg" in
    --install)      install_it=1 ;;
    --menubar-only) menubar_only=1 ;;
    *) printf 'error: unknown option %s\n' "$arg" >&2; exit 1 ;;
  esac
done

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = "Darwin" ] || die "macOS only (needs sips, iconutil, codesign)"
for t in sips iconutil codesign plutil; do
  command -v "$t" >/dev/null 2>&1 || die "missing '$t' (install the Xcode Command Line Tools)"
done
[ -f "$ICON_SRC" ]  || die "icon master not found: $ICON_SRC"
[ -f "$PLIST_SRC" ] || die "Info.plist template not found: $PLIST_SRC"

# --- the executable ---
bin="${COPYCAT_UI_BIN:-}"
if [ -z "$bin" ]; then
  command -v cargo >/dev/null 2>&1 \
    || die "cargo not found. Install Rust, or set COPYCAT_UI_BIN to a prebuilt copycat-ui."
  say "Building copycat-ui (release)…"
  ( cd "$REPO" && cargo build --release --locked -p copycat-ui )
  bin="$REPO/target/release/copycat-ui"
fi
[ -x "$bin" ] || die "copycat-ui binary not found or not executable: $bin"

version=$(awk -F'"' '/^version = /{print $2; exit}' "$REPO/Cargo.toml")
[ -n "$version" ] || version="0.0.0"

# --- skeleton ---
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

# --- icon: one master PNG -> the 10 sizes iconutil expects ---
# sips resamples per size rather than letting the Dock scale one bitmap, which is
# what keeps the 16pt and 32pt variants legible.
iconset="$OUT_DIR/copycat.iconset"
rm -rf "$iconset"; mkdir -p "$iconset"
for sz in 16 32 128 256 512; do
  sips -z "$sz" "$sz" "$ICON_SRC" --out "$iconset/icon_${sz}x${sz}.png" >/dev/null
  sips -z $((sz * 2)) $((sz * 2)) "$ICON_SRC" --out "$iconset/icon_${sz}x${sz}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$APP/Contents/Resources/copycat.icns"
rm -rf "$iconset"

# --- Info.plist ---
# LSUIElement makes it a menu-bar-only accessory: no Dock tile, no Cmd-Tab entry.
# Off by default, because then the Dock tile is what shows the cat icon and the
# window minimises like any normal app.
# The two plist lines are emitted by awk rather than passed in: `awk -v` cannot
# carry a value containing a literal newline ("newline in string"), so the
# earlier multi-line variable made `--menubar-only` fail *before* writing the
# plist — the flag silently produced a Dock-tile build every time.
awk -v ver="$version" -v lsui="$menubar_only" '
  { gsub(/__VERSION__/, ver)
    if ($0 == "__LSUIELEMENT__") {
      if (lsui == 1) { print "    <key>LSUIElement</key>"; print "    <true/>" }
      next
    }
    print }
' "$PLIST_SRC" > "$APP/Contents/Info.plist"
plutil -lint "$APP/Contents/Info.plist" >/dev/null || die "generated Info.plist is malformed"

cp "$bin" "$APP/Contents/MacOS/copycat-ui"
chmod 0755 "$APP/Contents/MacOS/copycat-ui"

# Ad-hoc signature. Not notarisation — it does not satisfy Gatekeeper for a
# downloaded bundle — but an unsigned bundle is refused outright on Apple
# silicon, so this is the floor for the app launching at all.
codesign --force --sign - --timestamp=none "$APP" >/dev/null 2>&1 \
  || say "warning: ad-hoc codesign failed; the app may refuse to launch"

say ""
say "Built $APP  (v$version)"
[ "$menubar_only" -eq 1 ] && say "  LSUIElement: menu-bar only, no Dock tile"

if [ "$install_it" -eq 1 ]; then
  dest="/Applications/Copycat.app"
  # A running copy holds its bundle open; replacing it underneath is what
  # produces a half-updated app that then fails to launch.
  pkill -f "Copycat.app/Contents/MacOS/copycat-ui" 2>/dev/null || true
  rm -rf "$dest" 2>/dev/null || die "could not remove $dest (try: sudo rm -rf $dest)"
  cp -R "$APP" "$dest" || die "could not copy into /Applications"
  # Nudge Launch Services so Spotlight/Launchpad see it without a logout.
  /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
    -f "$dest" >/dev/null 2>&1 || true
  say "Installed $dest"
  say "Open it from Spotlight (Cmd-Space, \"Copycat\") or: open -a Copycat"
else
  say "Install with:  sh dist/macos/build-app.sh --install"
fi
