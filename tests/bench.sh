#!/bin/zsh
# Times the two calls that run on every command you type.
#
#   ./tests/bench.sh [entries]

zmodload zsh/datetime

entries=${1:-2000}
root=${0:a:h:h}
bin=$root/target/release/zcomplete
[[ -x $bin ]] || { print -u2 "build first: cargo build --release"; exit 1 }

work=$(mktemp -d)
trap 'rm -rf $work' EXIT
export ZCOMPLETE_DATA_DIR=$work/data

# A database of plausible size. The names have to be on PATH to be learned, so
# they are made there; the timestamps spread across a few months so the decay
# buckets are all in play.
mkdir -p $work/bin
python3 - "$entries" "$work/bin" "$work/hist" <<'PY'
import os, random, sys
random.seed(7)
count, bindir, hist = int(sys.argv[1]), sys.argv[2], sys.argv[3]
lines = []
for _ in range(count):
    name = "".join(random.choice("abcdefghijklmnopqrstuvwxyz-") for _ in range(random.randint(3, 12)))
    open(os.path.join(bindir, name), "w").close()
    os.chmod(os.path.join(bindir, name), 0o755)
    for _ in range(random.randint(1, 3)):
        lines.append(f": {1700000000 + random.randint(0, 9000000)}:0;{name}")
open(hist, "w").write("\n".join(lines) + "\n")
PY
export PATH=$work/bin:$PATH
HISTFILE=$work/hist $bin import zsh >/dev/null

# Per-directory and per-command ranks share one table, and it is the biggest
# thing written on every command. Fill it the way months of use would.
for (( d = 0; d < 40; d++ )); do
    mkdir -p $work/dirs/$d
    ( cd $work/dirs/$d && for c in git make ls cargo grep sed awk find rsync ssh; do
        $bin record --shell zsh --kind auto --status 0 -- $c >/dev/null
    done )
done
for verb in status commit push pull rebase log diff add stash fetch; do
    $bin record --shell zsh --kind auto --status 0 -- "git $verb" >/dev/null
done

time_it() {
    local label=$1 runs=$2 start end i
    shift 2
    start=$EPOCHREALTIME
    for (( i = 0; i < runs; i++ )); do "$@" >/dev/null 2>&1 || true; done
    end=$EPOCHREALTIME
    printf '%-34s %6.2f ms\n' "$label" $(( (end - start) * 1000.0 / runs ))
}

print "database: $($bin stats -n 100000 | grep -c . ) rows, PATH: $(ls ${(s.:.)PATH} 2>/dev/null | wc -l | tr -d ' ') entries\n"
time_it 'process spawn (the floor)' 300 /usr/bin/true
time_it 'record  (every command)' 300 $bin record --shell zsh --kind auto --status 0 -- mkdir
time_it 'record  (with a subcommand)' 300 $bin record --shell zsh --kind auto --status 0 -- 'git status'
time_it 'resolve (learned hit)' 300 $bin query mkd
time_it 'resolve (no match)' 300 $bin query qwzzxv
time_it 'resolve (subcommand)' 300 $bin query git sttaus
ZCOMPLETE_DATA_DIR=$work/empty time_it 'resolve (cold, scans PATH)' 200 $bin query mkd
time_it 'zcomplete --version' 300 $bin --version
