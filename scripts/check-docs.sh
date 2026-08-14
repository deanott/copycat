#!/bin/sh
# The house style rules a human reviewer reliably misses: no em/en dashes, no
# emoji, length caps, and no dead relative links.
#
#   sh scripts/check-docs.sh          # every tracked .md
#
# Judgement calls (voice, selling, over-explaining) stay with the reviewer.
# This only checks what a grep can prove, and prints every violation rather
# than stopping at the first, so one run tells you the whole job.
set -eu

cd "$(git rev-parse --show-toplevel)"
fail=0
note() {
	printf '%s\n' "$1" >&2
	fail=1
}

docs=$(git ls-files '*.md')

# --- Rule 1: no em dash, no en dash ------------------------------------------
for f in $docs; do
	hits=$(grep -nP '[\x{2014}\x{2013}]' "$f" || true)
	[ -z "$hits" ] || note "$f: em/en dash (use a comma, colon or full stop)
$hits"
done

# --- Rule 4: no emoji --------------------------------------------------------
# Pictographs, dingbats, symbols and the variation selector. Deliberately not
# every symbol block: `ᗢ`, `•` and box drawing are product output we quote.
emoji='[\x{1F300}-\x{1FAFF}\x{2600}-\x{27BF}\x{2B00}-\x{2BFF}\x{FE0F}\x{2705}\x{274C}\x{2728}]'
for f in $docs; do
	hits=$(grep -nP "$emoji" "$f" || true)
	[ -z "$hits" ] || note "$f: emoji
$hits"
done

# --- Length caps -------------------------------------------------------------
cap_for() {
	case "$1" in
	README.md) echo 250 ;;
	*/README.md) echo 100 ;;
	docs/*.md) echo 300 ;;
	*) echo 0 ;;
	esac
}
for f in $docs; do
	cap=$(cap_for "$f")
	[ "$cap" -eq 0 ] && continue
	lines=$(wc -l <"$f")
	[ "$lines" -le "$cap" ] || note "$f: $lines lines, cap is $cap"
done

# --- Dead relative links -----------------------------------------------------
# Only local paths: a 404 on a URL needs the network, which a commit hook and a
# no-egress CI step should not depend on. Anchors are checked when the link
# names a file; a bare #anchor is resolved against the file itself.
for f in $docs; do
	dir=$(dirname "$f")
	targets=$(grep -oP '\]\(\K[^)]+' "$f" | grep -v '^[a-z][a-z0-9+.-]*://' | grep -v '^mailto:' || true)
	for t in $targets; do
		path=${t%%#*}
		anchor=${t#*#}
		[ "$anchor" = "$t" ] && anchor=''
		if [ -n "$path" ]; then
			resolved="$dir/$path"
			[ "$dir" = "." ] && resolved="$path"
			if [ ! -e "$resolved" ]; then
				note "$f: link to missing path '$path'"
				continue
			fi
		else
			resolved="$f"
		fi
		# Only markdown files have headings to anchor into.
		case "$resolved" in *.md) ;; *) continue ;; esac
		[ -n "$anchor" ] || continue
		# GitHub's slug: lowercase, drop anything but word chars/spaces/hyphens,
		# then spaces to hyphens.
		found=$(grep '^#\{1,6\} ' "$resolved" |
			sed 's/^#\{1,6\} //' |
			tr '[:upper:]' '[:lower:]' |
			sed 's/[^a-z0-9 _-]//g; s/ /-/g' |
			grep -Fx "$anchor" || true)
		[ -n "$found" ] || note "$f: link to '$t' but $resolved has no such heading"
	done
done

if [ "$fail" -ne 0 ]; then
	echo "" >&2
	echo "docs check failed. Cut the explanation, not the facts." >&2
	exit 1
fi
echo "docs check passed: $(echo "$docs" | wc -w) files."
