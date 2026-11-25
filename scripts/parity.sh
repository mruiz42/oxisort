#!/usr/bin/env bash
# Side-by-side comparison of phockup and oxisort on the test fixtures.
#
# Usage: scripts/parity.sh [WORKDIR]
#   WORKDIR defaults to a new temp folder; it receives copies of the
#   fixtures (in_p, in_o) and both outputs (out_p, out_o). Nothing else is
#   read or written.
#
# Prints the sorted relative output trees and their diff for the default
# date fields and for `-f DateTimeOriginal`. Every difference must be on the
# expected-difference list in README.md ("Differences from phockup");
# anything else is a bug.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
work="${1:-$(mktemp -d)}"
mkdir -p "$work"
command -v phockup >/dev/null || { echo "parity: phockup not found" >&2; exit 1; }

oxisort="$root/target/release/oxisort"
[ -x "$oxisort" ] || (cd "$root" && cargo build --release --quiet)

# phockup names videos by UTC; oxisort converts mvhd UTC to the system
# zone. Pass the system offset explicitly so the run is reproducible.
offset="$(date +%z)"
tz="${offset:0:3}:${offset:3:2}"

listing() { (cd "$1" && find . -type f -o -type l | sed 's|^\./||' | LC_ALL=C sort); }

compare() { # label, extra args...
    local label="$1"; shift
    rm -rf "$work/in_p" "$work/in_o" "$work/out_p" "$work/out_o"
    mkdir -p "$work/in_p" "$work/in_o"
    cp -p "$root"/tests/fixtures/* "$work/in_p/"
    cp -p "$root"/tests/fixtures/* "$work/in_o/"
    rm -f "$work/in_p/README.md" "$work/in_o/README.md"
    # Sidecars, to show the suffix handling.
    for d in in_p in_o; do
        printf '<x/>' > "$work/$d/dto.jpg.xmp"
        printf '<x/>' > "$work/$d/dto.xmp"
    done
    phockup "$work/in_p" "$work/out_p" "$@" >/dev/null 2>&1 || true
    "$oxisort" --quiet --tz "$tz" "$work/in_o" "$work/out_o" "$@" || true
    echo "=== $label: phockup (left) vs oxisort (right) ==="
    diff <(listing "$work/out_p") <(listing "$work/out_o") && echo "(identical)" || true
    echo
}

compare "default date fields"
compare "-f DateTimeOriginal" -f DateTimeOriginal
echo "work folder: $work"
