#!/usr/bin/env bash
# Robustness smoke on real history: rebuild 3-commit windows as dirty trees, run plan-only.
# Usage: smoke.sh <repository>...   (local paths or URLs; each is cloned, never modified)
set -uo pipefail
[ $# -gt 0 ] || { echo "usage: $0 <repository>..." >&2; exit 2; }
BIN=${GIT_CARVE_BIN:-$(cd "$(dirname "$0")/../.." && pwd)/target/debug/git-carve}
W=/tmp/carve-smoke; rm -rf $W; mkdir -p $W
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
printf '%-22s %-10s %5s %6s %5s %5s %5s %6s %s\n' repo window exit atoms paths groups leave ms note
for src in "$@"; do
  name=$(basename "$src" | tr ' ' '_'); dst=$W/$name
  git clone -q "$src" "$dst" 2>/dev/null || { echo "clone failed: $src"; continue; }
  cd "$dst"
  mapfile -t commits < <(git rev-list --no-merges --first-parent --reverse HEAD)
  n=${#commits[@]}; step=$(( n > 40 ? n / 8 : 5 ))
  for ((i=1; i+2<n; i+=step)); do
    base=$(git rev-parse "${commits[$i]}^" 2>/dev/null) || continue
    last=${commits[$((i+2))]}
    git checkout -q -f --detach "$base" && git clean -qfdx
    git read-tree -u --reset "$last" && git reset -q "$base"
    paths=$(git status --porcelain --untracked-files=all | wc -l)
    t0=$(date +%s%N); out=$("$BIN" --plan-only --no-check --json 2>$W/err); code=$?; t1=$(date +%s%N)
    ms=$(( (t1 - t0) / 1000000 ))
    if [ $code -eq 0 ]; then
      read atoms groups leave < <(echo "$out" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(len(d["atoms"]), len(d["groups"]), len(d["leave"]))')
      note=$(echo "$out" | python3 "$(dirname "$0")/note.py")
    else
      atoms=-; groups=-; leave=-; note=$(head -c 110 $W/err | tr '\n' ' ')
    fi
    printf '%-22s %-10s %5s %6s %5s %5s %5s %6s %s\n' "${name:0:22}" "${base:0:7}" $code $atoms $paths $groups $leave $ms "$note"
  done
  cd /
done
