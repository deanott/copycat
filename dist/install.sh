#!/bin/sh
# copycat installer — downloads prebuilt binaries from a GitHub Release and drops
# them on your PATH. No Rust/cargo required.
#
#   curl -fsSL https://raw.githubusercontent.com/deanott/copycat/main/dist/install.sh | sh
#
# PRIVATE REPO: release assets aren't downloadable with anonymous curl. If the
# GitHub CLI (`gh`) is installed and logged in, this script uses it automatically
# (works for private repos). Otherwise it falls back to anonymous curl (public
# repos only). Auth other than gh: set GITHUB_TOKEN and this script will pass it.
#
# Env overrides:
#   COPYCAT_VERSION=v1.0.0        install a specific tag instead of latest
#   COPYCAT_INSTALL_DIR=~/bin     install location (default: ~/.local/bin)
#   GITHUB_TOKEN=ghp_...          token for authenticated curl (private repos)
set -eu

REPO="deanott/copycat"
INSTALL_DIR="${COPYCAT_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

command -v uname >/dev/null 2>&1 || die "need 'uname'"
command -v tar   >/dev/null 2>&1 || die "need 'tar'"

# --- detect platform -> release target triple ---
os=$(uname -s); arch=$(uname -m)
case "$os" in
  Linux)  case "$arch" in
            x86_64|amd64) target="x86_64-unknown-linux-gnu" ;;
            *) die "unsupported Linux arch '$arch' (build from source: see README)" ;;
          esac ;;
  Darwin) case "$arch" in
            arm64|aarch64) target="aarch64-apple-darwin" ;;
            x86_64)        target="x86_64-apple-darwin" ;;
            *) die "unsupported macOS arch '$arch'" ;;
          esac ;;
  # MINGW*/MSYS*/CYGWIN* mean someone ran this from Git Bash on Windows. The
  # binaries exist for Windows, but this script installs the unix layout; point
  # them at the PowerShell installer rather than failing opaquely.
  MINGW*|MSYS*|CYGWIN*)
    die "this looks like Windows (uname reports '$os'). Use the PowerShell installer:
  irm https://raw.githubusercontent.com/deanott/copycat/main/dist/install.ps1 | iex" ;;
  *) die "unsupported OS '$os' — copycat supports Linux, macOS and Windows (Windows: use dist/install.ps1)" ;;
esac

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# --- fetch the release archive: prefer gh (auth, private-repo capable) ---
have_gh() { command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; }

if have_gh; then
  say "Using GitHub CLI (authenticated — works for private repos)."
  tag="${COPYCAT_VERSION:-}"
  [ -n "$tag" ] || tag=$(gh release view -R "$REPO" --json tagName -q .tagName 2>/dev/null) \
    || die "no release found on $REPO (has one been published?)"
  asset="copycat-$tag-$target.tar.gz"
  gh release download "$tag" -R "$REPO" -p "$asset" -D "$tmp" 2>/dev/null \
    || die "asset '$asset' not in release $tag — did the release build finish & upload it? (check the Actions run)"
  # Fetched separately: a missing checksum must produce our own explanatory
  # error below, not gh's "no assets match the file pattern".
  gh release download "$tag" -R "$REPO" -p "$asset.sha256" -D "$tmp" 2>/dev/null || true
else
  say "gh not available/authenticated — trying anonymous download (public repos only)."
  if command -v curl >/dev/null 2>&1; then
    auth=""; [ -n "${GITHUB_TOKEN:-}" ] && auth="-H \"Authorization: Bearer $GITHUB_TOKEN\""
    fetch() { eval curl -fsSL $auth "\"$1\""; }
    fetcho() { eval curl -fsSL $auth -o "\"$1\"" "\"$2\""; }
  elif command -v wget >/dev/null 2>&1; then
    fetch()  { wget -qO- "$1"; }
    fetcho() { wget -qO "$1" "$2"; }
  else
    die "need 'gh' (logged in) or 'curl'/'wget'"
  fi
  tag="${COPYCAT_VERSION:-}"
  if [ -z "$tag" ]; then
    tag=$(fetch "https://api.github.com/repos/$REPO/releases/latest" \
          | sed -n 's/.*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)
  fi
  [ -n "$tag" ] || die "no release found (private repo? install gh + 'gh auth login', or build from source)"
  asset="copycat-$tag-$target.tar.gz"
  say "Downloading $asset ..."
  fetcho "$tmp/$asset" "https://github.com/$REPO/releases/download/$tag/$asset" \
    || die "download failed — if $REPO is private, install gh and 'gh auth login', or build from source"
  fetcho "$tmp/$asset.sha256" "https://github.com/$REPO/releases/download/$tag/$asset.sha256" || true
fi

# --- verify the checksum before unpacking anything ---
# Parity with dist/install.ps1, which has always refused unverified binaries.
# release.yml publishes `<asset>.sha256` beside every archive; if it is absent
# the release is incomplete, and trusting the bytes anyway is exactly the hole
# the Windows installer was written to avoid.
sumfile="$tmp/$asset.sha256"
[ -f "$sumfile" ] || die "no '$asset.sha256' published alongside $asset — refusing to install unverified binaries.
  The release build should upload it; re-run the release workflow for $tag."

# The file is '<hex>  <filename>' as written by sha256sum/shasum -a 256.
expected=$(awk 'NR==1 {print $1}' "$sumfile" | tr 'A-F' 'a-f')
[ -n "$expected" ] || die "could not parse a checksum out of '$asset.sha256'"

if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$tmp/$asset" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then   # macOS ships shasum, not sha256sum
  actual=$(shasum -a 256 "$tmp/$asset" | awk '{print $1}')
else
  die "need 'sha256sum' or 'shasum' to verify the download"
fi
actual=$(printf '%s' "$actual" | tr 'A-F' 'a-f')

[ "$expected" = "$actual" ] || die "checksum mismatch for $asset
  expected $expected
  got      $actual"
say "Checksum OK ($actual)"

# --- extract & install ---
tar -xzf "$tmp/$asset" -C "$tmp"
srcdir="$tmp/copycat-$tag-$target"
mkdir -p "$INSTALL_DIR"
for b in copycatd copycat copycat-mcp; do
  [ -f "$srcdir/$b" ] || die "binary '$b' missing from archive"
  cp "$srcdir/$b" "$INSTALL_DIR/$b"; chmod 0755 "$INSTALL_DIR/$b"
done

say ""
say "Installed copycatd, copycat, copycat-mcp ($tag) -> $INSTALL_DIR"
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) say "NOTE: add $INSTALL_DIR to your PATH:  export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac
say "Next: run 'copycatd' then 'copycat status'  (walkthrough: docs/TESTING.md)"
