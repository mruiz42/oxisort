#!/usr/bin/env bash
# Builds a synthetic benchmark library (plan: "Benchmark and parity").
#
# Usage: scripts/gen-bench.sh DIR [N=1200] [SEED=42]
#   OXI_BENCH_SMALL=1  use tiny templates (smoke tests of the scripts only)
#
# Templates are made once with ffmpeg/sips, then cloned into N files across
# ~40 nested folders: 45% JPEG, 25% "ARW" (TIFF data), 10% HEIC, 5% PNG,
# 10% video (MP4/MOV), 5% filename-dated or undated JPEG. 30% of the ARW
# files get a name.ARW.xmp or name.xmp sidecar, ~5% share a capture second
# with the previous file, and 3% are exact duplicates in other folders.
# Every per-file tag is written by ONE exiftool process (-@ argfile with
# -execute).
#
# Refuses any DIR under $HOME/Pictures (checked before anything is created):
# real libraries are never touched.
set -euo pipefail

dir="${1:?usage: gen-bench.sh DIR [N] [SEED]}"
n="${2:-1200}"
seed="${3:-42}"
for tool in exiftool ffmpeg sips python3; do
    command -v "$tool" >/dev/null || { echo "gen-bench: missing $tool" >&2; exit 1; }
done

# Resolve DIR through its nearest existing ancestor, without creating it.
resolve() {
    python3 -c 'import os, sys; print(os.path.realpath(sys.argv[1]))' "$1"
}
abs="$(resolve "$dir")"
pics="$(resolve "$HOME/Pictures")"
case "$abs/" in
    "$pics"/*) echo "gen-bench: refusing to write under $pics" >&2; exit 2 ;;
esac
if [ -e "$abs" ] && [ -n "$(ls -A "$abs")" ]; then
    echo "gen-bench: $abs is not empty" >&2; exit 2
fi
mkdir -p "$abs"

tpl="$abs/.templates"
mkdir -p "$tpl"
if [ "${OXI_BENCH_SMALL:-0}" = 1 ]; then
    img="s=64x48"; vid="testsrc2=s=64x48:d=0.2"; tiff_w=64
else
    img="s=3000x2000"; vid="testsrc2=s=1280x720:d=4"; tiff_w=1600
fi
q() { ffmpeg -loglevel error -y "$@"; }
# Noise compresses badly, which gives realistic multi-MB JPEGs.
q -f lavfi -i "nullsrc=$img,geq=random(1)*255:128:128" -frames:v 1 -q:v 2 "$tpl/t.jpg"
sips -s format heic "$tpl/t.jpg" --out "$tpl/t.heic" >/dev/null
sips -Z 1000 -s format png "$tpl/t.jpg" --out "$tpl/t.png" >/dev/null
sips -Z "$tiff_w" -s format tiff "$tpl/t.jpg" --out "$tpl/t.tif" >/dev/null
for ext in mp4 mov; do
    q -f lavfi -i "$vid" -vf noise=alls=40:allf=t -c:v libx264 -b:v 8M -pix_fmt yuv420p "$tpl/t.$ext"
done

# Python lays the library out deterministically from the seed and writes
# the exiftool argfile plus the duplicate copies to make afterwards.
python3 - "$abs" "$tpl" "$n" "$seed" <<'PY'
import os, random, shutil, sys
root, tpl, n, seed = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
r = random.Random(seed)
folders = [f"{y}/card{c:02d}" + ("/DCIM" if c % 3 == 0 else "")
           for y in (2019, 2020, 2021, 2022) for c in range(10)]
args, dups = [], []
def stamp():
    return (2019 + r.randrange(4), 1 + r.randrange(12), 1 + r.randrange(28),
            r.randrange(24), r.randrange(60), r.randrange(60))
last = stamp()
for i in range(n):
    folder = os.path.join(root, r.choice(folders))
    os.makedirs(folder, exist_ok=True)
    roll = r.random()
    ts = last if r.random() < 0.05 else stamp()
    last = ts
    dt = "%04d:%02d:%02d %02d:%02d:%02d" % ts
    if roll < 0.45:   kind, tmpl = "jpg", "t.jpg"
    elif roll < 0.70: kind, tmpl = "arw", "t.tif"
    elif roll < 0.80: kind, tmpl = "heic", "t.heic"
    elif roll < 0.85: kind, tmpl = "png", "t.png"
    elif roll < 0.95: kind, tmpl = "vid", r.choice(["t.mp4", "t.mov"])
    else:             kind, tmpl = "plain", "t.jpg"
    ext = {"jpg": "JPG", "arw": "ARW", "heic": "HEIC", "png": "png", "plain": "jpg"}.get(
        kind, tmpl.split(".")[1].upper())
    name = f"DSC{i:05d}.{ext}"
    if kind == "plain" and r.random() < 0.5:
        name = "IMG_%04d%02d%02d_%02d%02d%02d.jpg" % ts
    path = os.path.join(folder, name)
    shutil.copyfile(os.path.join(tpl, tmpl), path)
    # A unique description per file keeps same-second files distinct.
    if kind == "vid":
        tags = [f"-QuickTime:CreateDate={dt}", f"-QuickTime:Comment=bench {i}"]
    else:
        tags = [f"-ImageDescription=oxisort bench {i}"]
        if kind != "plain":
            tags += [f"-DateTimeOriginal={dt}", f"-CreateDate={dt}"]
            if r.random() < 0.3:
                tags.append(f"-SubSecTimeOriginal={r.randrange(1000):03d}")
    args += ["-overwrite_original"] + tags + [path, "-execute"]
    if kind == "arw" and r.random() < 0.3:
        side = path + ".xmp" if r.random() < 0.5 else os.path.splitext(path)[0] + ".xmp"
        with open(side, "w") as f:
            f.write(f"<x:xmpmeta><!-- sidecar {i} --></x:xmpmeta>\n")
    if r.random() < 0.03:
        dups.append((path, os.path.join(root, r.choice(folders), f"COPY_{i:05d}.{ext}")))
with open(os.path.join(tpl, "args.txt"), "w") as f:
    f.write("\n".join(args) + "\n")
with open(os.path.join(tpl, "dups.txt"), "w") as f:
    f.writelines(f"{a}\t{b}\n" for a, b in dups)
PY
exiftool -q -q -@ "$tpl/args.txt" >/dev/null
while IFS=$'\t' read -r a b; do
    mkdir -p "$(dirname "$b")"
    cp -p "$a" "$b"
done < "$tpl/dups.txt"
rm -rf "$tpl"
count="$(find "$abs" -type f ! -name '*.xmp' | wc -l | tr -d ' ')"
echo "gen-bench: $count media files ($(du -sh "$abs" | cut -f1)) in $abs"
