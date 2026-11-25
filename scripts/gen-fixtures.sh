#!/usr/bin/env bash
# Regenerates the small test fixtures in tests/fixtures.
#
# Dev-only: needs exiftool, ffmpeg and sips (macOS). The test suite itself
# never runs these tools; it only reads the committed files. Every fixture is
# a few hundred bytes to a few KB, and tests/fixtures/README.md lists the exact
# tags each one carries. Re-running this script produces equivalent files (the
# encoders may change a few bytes between versions, but not the tags).
set -euo pipefail

for tool in exiftool ffmpeg sips python3; do
    command -v "$tool" >/dev/null || { echo "gen-fixtures: missing $tool" >&2; exit 1; }
done

root="$(cd "$(dirname "$0")/.." && pwd)"
out="$root/tests/fixtures"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$out"

ex() { exiftool -q -overwrite_original "$@"; }

# A 16x16 red JPEG without any metadata: the template for every JPEG/image.
ffmpeg -loglevel error -y -f lavfi -i color=c=red:s=16x16 -frames:v 1 "$work/base.jpg"
ex -all= "$work/base.jpg"

jpeg() { cp "$work/base.jpg" "$out/$1"; shift_name="$1"; shift; [ $# -gt 0 ] && ex "$@" "$out/$shift_name"; true; }

jpeg base.jpg
jpeg dto.jpg "-DateTimeOriginal=2017:01:02 03:04:05" "-SubSecTimeOriginal=123" \
    "-OffsetTimeOriginal=+02:00" "-CreateDate=2017:01:02 03:04:06"
jpeg cd_only.jpg "-CreateDate=2018:02:03 04:05:06"
jpeg modify_only.jpg "-ModifyDate=2018:03:04 05:06:07"
jpeg zero_date.jpg "-DateTimeOriginal#=0000:00:00 00:00:00"
jpeg IMG_20160915_123456.jpg "-Comment=prefixed name"
jpeg 20160915_123456.jpg "-Comment=bare name"

# Other still-image containers, all with the same DateTimeOriginal.
dto19="-DateTimeOriginal=2019:03:04 05:06:07"
sips -s format heic "$work/base.jpg" --out "$out/a.heic" >/dev/null
sips -s format png "$work/base.jpg" --out "$out/a.png" >/dev/null
sips -s format tiff "$work/base.jpg" --out "$out/a.tiff" >/dev/null
ex "$dto19" "$out/a.heic" "$out/a.png" "$out/a.tiff"
# TIFF data under a RAW extension: exercises the extension map + TIFF parser.
cp "$out/a.tiff" "$out/fake.arw"

# WebP: neither ffmpeg nor sips writes WebP here, so build a 1x1 lossless
# (VP8L) file by hand and let exiftool add the EXIF chunk.
python3 - "$work/a.webp" <<'PY'
import struct, sys
vp8l = bytes.fromhex("2f000000100710111188 88fe07".replace(" ", ""))
chunk = b"VP8L" + struct.pack("<I", len(vp8l)) + vp8l + b"\0" * (len(vp8l) % 2)
data = b"WEBP" + chunk
open(sys.argv[1], "wb").write(b"RIFF" + struct.pack("<I", len(data)) + data)
PY
cp "$work/a.webp" "$out/a.webp"
ex "-DateTimeOriginal=2023:01:02 03:04:05" "$out/a.webp"

# ORF: TIFF-like header with the Olympus "RO" magic that neither metadata
# crate parses (decision Q7). Content is filler; only the extension matters.
python3 - "$out/a.orf" <<'PY'
import sys
open(sys.argv[1], "wb").write(b"IIRO\x08\x00\x00\x00" + bytes(248))
PY

# Videos. ffmpeg writes creation_time into mvhd (UTC).
vid() { # name creation_time(or empty)
    local meta=()
    [ -n "$2" ] && meta=(-metadata "creation_time=$2")
    ffmpeg -loglevel error -y -f lavfi -i color=c=blue:s=16x16:d=0.2 -c:v libx264 \
        -pix_fmt yuv420p "${meta[@]}" "$out/$1"
}
vid apple.mov 2021-06-07T06:09:10Z
ex "-Keys:CreationDate=2021:06:07 08:09:10+02:00" "$out/apple.mov"
vid mvhd_only.mp4 2022-07-08T02:10:11Z
vid a.3gp 2020-05-06T07:08:09Z
vid a.mkv 2020-05-06T07:08:09Z
vid nodate.mp4 ""

printf 'not media\n' > "$out/notes.txt"

# Normalise mtimes so mtime-based tests are reproducible.
touch -t 201501020304.05 "$out"/*.jpg "$out"/*.heic "$out"/*.png "$out"/*.tiff \
    "$out"/*.arw "$out"/*.webp "$out"/*.orf "$out"/*.mov "$out"/*.mp4 "$out"/*.3gp \
    "$out"/*.mkv "$out"/*.txt
ls -l "$out"
