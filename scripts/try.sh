#!/usr/bin/env bash
# Try the current milestone end to end. Only touches throwaway repos under /tmp.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
say() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
fingerprint() { find . -path ./.git -prune -o -path ./node_modules -prune -o -print0 | sort -z | xargs -0 stat -c '%n %a %s %Y'; find . -path ./.git -prune -o -path ./node_modules -prune -o -type f -print0 | sort -z | xargs -0 sha256sum; }

say "Building and running the test suite (a minute or two)"
cd "$ROOT"
cargo build --quiet --bins
cargo test --quiet --no-fail-fast 2>&1 | grep -E '^test result' \
  | awk '{p+=$4; f+=$6} END {printf "    %d passed, %d failed\n", p, f; if (f) exit 1}'
export PATH="$ROOT/target/debug:$PATH"

DEMO=$(mktemp -d /tmp/carve-try.XXXXXX)
"$ROOT/scripts/make-demo-repo.sh" "$DEMO" --typescript >/dev/null
cd "$DEMO"

say "The mess: a feature, a drive-by fix, a rename across 6 files, 2 debug lines"
git status --short

say "git carve --plan-only   (the buffer your editor gets; every commit already checked with tsc)"
git carve --plan-only 2>/dev/null
N=$(git carve --plan-only 2>/dev/null | grep -c '^commit$')

say "Optional: git carve --judge   (built-in learned judge; EVAL verdict: optional flag, provisional)"
J=$(git carve --plan-only --judge 2>/dev/null | grep -c '^commit$')
echo "code-only: $N commits; --judge: $J commits. Why the judge merged what it merged:"
git carve --plan-only --judge --json 2>/dev/null \
  | sed -n '/"judge_merges"/,/\]/p' | grep '^ *"a' | sed -e 's/^ *"/  /' -e 's/",\{0,1\}$//'
echo "In its buffer, carve flags the merges it is least sure of (D30):"
git carve --plan-only --judge 2>/dev/null | grep '^# unsure' | sed 's/^/  /'
echo "(Known, DECISIONS.md D37/D38: the judge helps on small trees, not measurably on large ones; code-only is the default.)"

BEFORE_STATUS=$(git status --porcelain=v2; git diff)
BEFORE_FILES=$(fingerprint)

say "git carve   (your editor opens once; here a script types one subject per commit, as you would)"
TYPIST=$(mktemp /tmp/carve-typist.XXXXXX)
cat > "$TYPIST" <<'AWK'
#!/bin/sh
awk '/^commit$/ { print; n++; printf "Subject typed by you, commit %d\n", n; next } { print }' "$1" > "$1.tmp" && mv "$1.tmp" "$1"
AWK
chmod +x "$TYPIST"
GIT_EDITOR="$TYPIST" git carve
rm -f "$TYPIST"

say "Proof 1: carve did not touch a single working-tree file"
[ "$(fingerprint)" = "$BEFORE_FILES" ] && echo "    identical bytes, modes, sizes, and mtimes" || { echo "    FAILED"; exit 1; }
echo "    still uncommitted (the debug lines):"; git diff -U0 | grep '^+[^+]' | sed 's/^/      /'

say "Proof 2: git rebase -x 'tsc --noEmit' passes on every commit (in a throwaway clone)"
CLONE=$(mktemp -d /tmp/carve-rebase.XXXXXX)
git clone -q "$DEMO" "$CLONE" && ln -s "$DEMO/node_modules" "$CLONE/node_modules"
(cd "$CLONE" && git rebase -q -x 'echo "    tsc passed on: $(git log -1 --format=%s)" && node_modules/.bin/tsc --noEmit -p . >/dev/null' "HEAD~$N" 2>/dev/null || true)
(cd "$CLONE" && git status | grep -q 'rebase in progress') && { echo "    FAILED: an exec failed"; exit 1; } || echo "    all $N commits type-check"
rm -rf "$CLONE"

say "git carve --undo"
git carve --undo

say "Proof 3: undo restored the exact state"
[ "$(git status --porcelain=v2; git diff)" = "$BEFORE_STATUS" ] && echo "    git status and git diff are byte-identical to before" || { echo "    FAILED"; exit 1; }
[ "$(fingerprint)" = "$BEFORE_FILES" ] && echo "    working tree still untouched" || { echo "    FAILED"; exit 1; }

say "When things don't compile (throwaway copies of the demo)"
BROKEN=$(mktemp -d /tmp/carve-try-broken.XXXXXX)
"$ROOT/scripts/make-demo-repo.sh" "$BROKEN" --typescript >/dev/null
(
  cd "$BROKEN"
  sed -i 's/throw failure/throw failur/' src/net.ts
  echo "  1. A typo in the half-written feature: carve commits the rest and leaves the feature (D40):"
  git carve --plan-only 2>/dev/null | sed -n '1,3p' | sed 's/^/     /'
)
LEGACY=$(mktemp -d /tmp/carve-try-legacy.XXXXXX)
"$ROOT/scripts/make-demo-repo.sh" "$LEGACY" --typescript >/dev/null
(
  cd "$LEGACY"
  git stash -q -u
  printf 'export const legacy: number = "not a number";\n' > src/legacy.ts
  git add src/legacy.ts && git -c user.name=demo -c user.email=demo@example.com commit -qm "legacy type error"
  git stash pop -q
  echo "  2. HEAD already has a type error: carve refuses and says why (D43)..."
  git carve --plan-only 2>&1 >/dev/null | grep -E 'fails on HEAD|hint' | sed 's/^/     /' || true
  echo "     ...and with --no-new-errors, no commit may add a new one:"
  git carve --plan-only --no-new-errors 2>/dev/null | sed -n '1p' | sed 's/^/     /'
)
rm -rf "$BROKEN" "$LEGACY"

say "Done. Try it yourself with your own editor:"
echo "    cd $DEMO && git carve        # the mess is back after the undo"
echo "    (then git carve --undo to try again; rm -rf $DEMO when finished)"
