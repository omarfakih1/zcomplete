#!/bin/zsh
# Times the two calls that run on every command you type. Needs zsh for
# EPOCHREALTIME; the binary itself does not.
#
#   ./tests/bench.sh [entries]

zmodload zsh/datetime

entries=${1:-2000}
root=${0:a:h:h}
bin=$root/target/release/zcomplete
[[ -x $bin ]] || { print -u2 "build first: cargo build --release"; exit 1 }

work=$(mktemp -d)
trap 'rm -rf $work' EXIT
export ZCOMPLETE_DATA_DIR=$work/data ZCOMPLETE_CONFIG=$work/config.toml

# A database of plausible size, timestamped across the last few months so the
# decay buckets are all in play.
python3 - "$entries" "$work/seed.tsv" <<'PY'
import random, sys
random.seed(7)
count, out = int(sys.argv[1]), sys.argv[2]
rows = []
for _ in range(count):
    name = "".join(random.choice("abcdefghijklmnopqrstuvwxyz-") for _ in range(random.randint(3, 12)))
    rows.append(f"{name}\t{random.uniform(1, 80):.3f}\t{1700000000 + random.randint(0, 9000000)}")
open(out, "w").write("\n".join(rows))
PY
$bin import --restore "$work/seed.tsv" >/dev/null

time_it() {
    local label=$1 runs=$2 start end i
    shift 2
    start=$EPOCHREALTIME
    for (( i = 0; i < runs; i++ )); do "$@" >/dev/null 2>&1 || true; done
    end=$EPOCHREALTIME
    printf '%-34s %6.2f ms\n' "$label" $(( (end - start) * 1000.0 / runs ))
}

print "database: $entries commands, PATH: $(ls ${(s.:.)PATH} 2>/dev/null | wc -l | tr -d ' ') entries\n"
time_it 'process spawn (the floor)' 300 /usr/bin/true
time_it 'record  (every command)' 300 $bin record --shell zsh --kind auto -- mkdir
time_it 'resolve (learned hit)' 300 $bin query mkd
time_it 'resolve (no match)' 300 $bin query qwzzxv
ZCOMPLETE_DATA_DIR=$work/empty time_it 'resolve (cold, scans PATH)' 200 $bin query mkd
time_it 'zcomplete --version' 300 $bin --version
