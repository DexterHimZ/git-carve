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

say "Already committed? git carve <base> re-carves the commits too (D47)"
WIP=$(mktemp -d /tmp/carve-try-wip.XXXXXX)
"$ROOT/scripts/make-demo-repo.sh" "$WIP" --typescript >/dev/null
(
  cd "$WIP"
  commit() { git -c user.name=demo -c user.email=demo@example.com commit -qm "$1"; }
  git add src/format.ts src/main.ts src/progress.ts && commit "wip"
  git add src/net.ts tests/net.test.ts src/parse.ts && commit "asdf"
  git add -A && commit "fix stuff"
  echo "  An agent committed the same mess as:"; git log --oneline -3 | sed 's/^/     /'
  echo "  Each new commit names the old commits its lines came from:"
  git carve HEAD~3 --plan-only 2>/dev/null | grep '^# from' | sed 's/^/     /'
  BEFORE=$(fingerprint)
  TYPIST=$(mktemp /tmp/carve-typist.XXXXXX)
  printf '%s\n' '#!/bin/sh' "awk '/^commit\$/ { print; n++; printf \"Subject typed by you, commit %d\\n\", n; next } { print }' \"\$1\" > \"\$1.tmp\" && mv \"\$1.tmp\" \"\$1\"" > "$TYPIST"
  chmod +x "$TYPIST"
  GIT_EDITOR="$TYPIST" git carve HEAD~3 | sed 's/^/     /'
  rm -f "$TYPIST"
  [ "$(fingerprint)" = "$BEFORE" ] && echo "     working tree untouched; the committed debug lines are uncommitted again" || { echo "     FAILED"; exit 1; }
  git carve --undo >/dev/null && echo "     git carve --undo: back on \"$(git log -1 --format=%s)\""
)
rm -rf "$WIP"

say "From a branch to a stack: git carve main --stack (D48)"
STK=$(mktemp -d /tmp/carve-try-stack.XXXXXX)
"$ROOT/scripts/make-demo-repo.sh" "$STK" --typescript >/dev/null
(
  cd "$STK"
  commit() { git -c user.name=demo -c user.email=demo@example.com commit -qm "$1"; }
  git branch -m feat && git branch main
  git add -A && commit "everything at once"
  TYPIST=$(mktemp /tmp/carve-typist.XXXXXX)
  printf '%s\n' '#!/bin/sh' "awk '/^commit\$/ { print; n++; printf \"Layer %d by you\\n\", n; next } { print }' \"\$1\" > \"\$1.tmp\" && mv \"\$1.tmp\" \"\$1\"" > "$TYPIST"
  chmod +x "$TYPIST"
  GIT_EDITOR="$TYPIST" git carve main --stack 2>/dev/null | sed -n '/^Stack on/,$p' | sed 's/^/     /'
  rm -f "$TYPIST"
  echo "     branches now: $(git branch --format='%(refname:short)' | tr '\n' ' ')"
  git carve --undo >/dev/null && echo "     git carve --undo removed the layer branches: $(git branch --format='%(refname:short)' | tr '\n' ' ')"
)
rm -rf "$STK"

say "Gate every push: git carve hooks install (D50), with fail-fast and parallel checks (D51)"
GATE=$(mktemp -d /tmp/carve-try-gate.XXXXXX)
"$ROOT/scripts/make-demo-repo.sh" "$GATE/repo" --typescript >/dev/null
(
  cd "$GATE/repo"
  commit() { git -c user.name=demo -c user.email=demo@example.com commit -qm "$1"; }
  git init -q --bare "$GATE/remote.git"
  git remote add origin "$GATE/remote.git" && git push -q -u origin main
  git carve hooks install | sed 's/^/     /'
  # An agent's history: the rename lands before its callers are updated.
  git add src/format.ts && commit "rename fmtBytes"
  git add -A && commit "everything else"
  echo "     \$ git push    (the hook checks each pushed commit and stops at the first failure)"
  git push -q origin main 2>&1 | sed 's/^/     /' || true
  echo "     what an agent's hook tells it (git carve hooks run claude):"
  printf '{"tool_input":{"command":"git push"},"cwd":"%s"}' "$PWD" \
    | git carve hooks run claude 2>&1 | sed -n '1,4p' | sed 's/^/       /' || true
  TYPIST=$(mktemp /tmp/carve-typist.XXXXXX)
  printf '%s\n' '#!/bin/sh' "awk '/^commit\$/ { print; n++; printf \"Part %d\\n\", n; next } { print }' \"\$1\" > \"\$1.tmp\" && mv \"\$1.tmp\" \"\$1\"" > "$TYPIST"
  chmod +x "$TYPIST"
  echo "     \$ git carve @{u}    (re-split the unpushed commits; you write the messages)"
  GIT_EDITOR="$TYPIST" git carve @{u} 2>/dev/null | sed -n '1,2p' | sed 's/^/     /'
  rm -f "$TYPIST"
  git push -q origin main 2>&1 | sed 's/^/     /' && echo "     pushed: $(git log --oneline origin/main | head -n 4 | tr '\n' ';')"
)
rm -rf "$GATE"

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
