#!/usr/bin/env bash
# Benchmarks oxisort against phockup on a synthetic set (gen-bench.sh).
#
# Usage: scripts/bench.sh SET SCRATCH [EXTERNAL_DIR]
#   SET           input library (never modified: every run copies or dry-runs)
#   SCRATCH       folder for outputs on the same volume as SET
#   EXTERNAL_DIR  optional folder on another volume (external SSD) for the
#                 cross-device copy and the --io-jobs 1 vs 4 comparison
#
# Uses hyperfine when installed (--warmup 1; >= 3 runs for phockup, >= 10 for
# oxisort). Without it, falls back to a simple timing loop (mean and stddev
# of the same run counts) so the script still works end to end.
#
# Fairness note: a same-volume oxisort copy uses APFS clones, which phockup's
# shutil.copy2 never does. The dry-run and external-copy rows compare real
# work on both sides; report them alongside the same-volume number.
set -euo pipefail

set_dir="${1:?usage: bench.sh SET SCRATCH [EXTERNAL_DIR]}"
scratch="${2:?usage: bench.sh SET SCRATCH [EXTERNAL_DIR]}"
external="${3:-}"
root="$(cd "$(dirname "$0")/.." && pwd)"
oxi="$root/target/release/oxisort"
(cd "$root" && cargo build --release --quiet)
command -v phockup >/dev/null || { echo "bench: phockup not found" >&2; exit 1; }
mkdir -p "$scratch"

files="$(find "$set_dir" -type f ! -name '*.xmp' | wc -l | tr -d ' ')"
echo "bench: $files media files in $set_dir"

# bench NAME RUNS PREPARE COMMAND
bench() {
    local name="$1" runs="$2" prepare="$3" cmd="$4"
    echo "--- $name"
    if command -v hyperfine >/dev/null; then
        hyperfine --warmup 1 --runs "$runs" --prepare "$prepare" -N --style basic "$cmd" 2>&1 \
            | grep -E "Time|Range" || true
        return
    fi
    python3 - "$runs" "$prepare" "$cmd" "$files" <<'PY'
import statistics, subprocess, sys, time
runs, prepare, cmd, files = int(sys.argv[1]), sys.argv[2], sys.argv[3], int(sys.argv[4])
def once():
    subprocess.run(prepare, shell=True, check=True)
    t = time.perf_counter()
    subprocess.run(cmd, shell=True, check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return time.perf_counter() - t
once()  # warmup
ts = [once() for _ in range(runs)]
m = statistics.mean(ts)
sd = statistics.stdev(ts) if len(ts) > 1 else 0.0
print(f"  mean {m:.3f} s ± {sd:.3f} s over {runs} runs  ({files / m:.0f} files/s)")
PY
}

out="$scratch/bench_out"
bench "phockup -c 8 (copy, same volume)" 3 "rm -rf '$out'" "phockup '$set_dir' '$out' -c 8"
bench "oxisort (copy, same volume)" 10 "rm -rf '$out'" "'$oxi' --quiet '$set_dir' '$out'"
bench "phockup -y -c 8 (dry run)" 3 "rm -rf '$out'" "phockup '$set_dir' '$out' -y -c 8"
bench "oxisort -n (dry run)" 10 "rm -rf '$out'" "'$oxi' --quiet -n '$set_dir' '$out'"

if [ -n "$external" ]; then
    ext="$external/oxisort_bench_out"
    bench "phockup -c 8 (copy to external)" 3 "rm -rf '$ext'" "phockup '$set_dir' '$ext' -c 8"
    bench "oxisort --io-jobs 1 (copy to external)" 10 "rm -rf '$ext'" "'$oxi' --quiet --io-jobs 1 '$set_dir' '$ext'"
    bench "oxisort default --io-jobs 4 (copy to external)" 10 "rm -rf '$ext'" "'$oxi' --quiet '$set_dir' '$ext'"
    rm -rf "$ext"
fi
rm -rf "$out"
