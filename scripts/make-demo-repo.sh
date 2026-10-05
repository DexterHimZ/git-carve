#!/usr/bin/env bash
# Builds the SPEC §14.1 demo repository in TypeScript: a base commit, then a dirty tree
# with a feature, a drive-by fix, a rename across 6 files, and 2 debug lines.
# Expected carve result: 3 commits and 2 lines left out.
# Usage: scripts/make-demo-repo.sh <dir> [--typescript]
#   --typescript  also provide node_modules/typescript for the default check: links a
#                 global install if there is one, otherwise runs npm install.
set -euo pipefail
DIR=${1:?usage: make-demo-repo.sh <dir> [--typescript]}
WITH_TS=${2:-}
[ -e "$DIR" ] && [ -n "$(ls -A "$DIR" 2>/dev/null)" ] && { echo "$DIR is not empty" >&2; exit 1; }
mkdir -p "$DIR" && cd "$DIR"
git init -q -b main .
git config user.name "carve demo"
git config user.email "demo@example.com"
mkdir -p src tests

cat > package.json <<'EOF'
{
  "name": "carve-demo",
  "private": true,
  "type": "module",
  "devDependencies": { "typescript": "^5.6.0" }
}
EOF
cat > tsconfig.json <<'EOF'
{
  "compilerOptions": {
    "strict": true,
    "noEmit": true,
    "target": "ES2022",
    "module": "ESNext",
    "moduleResolution": "Bundler",
    "skipLibCheck": true
  },
  "include": ["src", "tests"]
}
EOF
printf 'node_modules/\n' > .gitignore

cat > src/format.ts <<'EOF'
export function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  return `${(n / 1024).toFixed(1)} KiB`;
}
EOF
cat > src/parse.ts <<'EOF'
export interface Header {
  name: string;
  value: string;
}

export function parseHeader(line: string): Header {
  const colon = line.indexOf(":");
  const name = line.slice(0, colon - 1);
  const value = line.slice(colon + 1).trim();
  return { name, value };
}
EOF
cat > src/net.ts <<'EOF'
export interface Reply {
  status: number;
  body: string;
}

export type Fetcher = (url: string) => Promise<Reply>;

export async function fetchAll(urls: string[], fetcher: Fetcher): Promise<Reply[]> {
  const results: Reply[] = [];
  for (const url of urls) {
    results.push(await fetcher(url));
  }
  return results;
}
EOF
cat > src/main.ts <<'EOF'
import { fetchAll } from "./net";
import { fmtBytes } from "./format";

export async function main(urls: string[]): Promise<string> {
  const cfg = { urls, verbose: false };
  const replies = await fetchAll(cfg.urls, async (u) => ({ status: 200, body: u }));

  const total = replies.reduce((sum, r) => sum + r.body.length, 0);
  return fmtBytes(total);
}
EOF
for f in report table progress summary; do
  case $f in
    report)   fn='describeUpload(label: string, size: number)'; body='`${label}: ${fmtBytes(size)}`';;
    table)    fn='sizeCell(size: number)';                      body='fmtBytes(size).padStart(10)';;
    progress) fn='progressText(done: number, all: number)';     body='`${fmtBytes(done)} of ${fmtBytes(all)}`';;
    summary)  fn='summaryLine(count: number, size: number)';    body='`${count} files, ${fmtBytes(size)}`';;
  esac
  cat > src/$f.ts <<EOF
import { fmtBytes } from "./format";

export function $fn: string {
  return $body;
}
EOF
done
cat > tests/net.test.ts <<'EOF'
import { fetchAll } from "../src/net";

export async function testFetchAll(): Promise<boolean> {
  const out = await fetchAll(["a"], async (u) => ({ status: 200, body: u }));
  return out.length === 1;
}
EOF
git add -A && git commit -qm "Base: fetch, parse, and format helpers"

# 1. Feature: retryWithBackoff, a call site in fetchAll, and a test.
# 4a. A debug line in fetchAll, separated from the call site by an unchanged line.
cat > src/net.ts <<'EOF'
export interface Reply {
  status: number;
  body: string;
}

export type Fetcher = (url: string) => Promise<Reply>;

export async function fetchAll(urls: string[], fetcher: Fetcher): Promise<Reply[]> {
  const results: Reply[] = [];
  for (const url of urls) {
    results.push(await retryWithBackoff(() => fetcher(url)));
  }
  console.log("fetched", results.length);
  return results;
}

export async function retryWithBackoff<T>(fn: () => Promise<T>, attempts = 3): Promise<T> {
  let delay = 100;
  for (let attempt = 1; ; attempt++) {
    try {
      return await fn();
    } catch (failure) {
      if (attempt >= attempts) throw failure;
      await new Promise((wake) => setTimeout(wake, delay));
      delay *= 2;
    }
  }
}
EOF
cat > tests/net.test.ts <<'EOF'
import { fetchAll, retryWithBackoff } from "../src/net";

export async function testFetchAll(): Promise<boolean> {
  const out = await fetchAll(["a"], async (u) => ({ status: 200, body: u }));
  return out.length === 1;
}

export async function testRetryWithBackoff(): Promise<boolean> {
  let calls = 0;
  const outcome = await retryWithBackoff(async () => {
    calls += 1;
    if (calls < 2) throw new Error("flaky");
    return "ok";
  });
  return outcome === "ok" && calls === 2;
}
EOF
# 2. Drive-by fix: an off-by-one in parseHeader.
sed -i.bak 's/line.slice(0, colon - 1)/line.slice(0, colon)/' src/parse.ts && rm src/parse.ts.bak
# 3. Rename fmtBytes -> formatBytes across 6 files.
for f in src/format.ts src/main.ts src/report.ts src/table.ts src/progress.ts src/summary.ts; do
  sed -i.bak 's/fmtBytes/formatBytes/g' "$f" && rm "$f.bak"
done
# 4b. A debug line in main, separated from the rename by unchanged lines.
sed -i.bak 's/^  const cfg = { urls, verbose: false };$/  const cfg = { urls, verbose: false };\n  console.log(cfg);/' src/main.ts && rm src/main.ts.bak
if [ "$WITH_TS" = "--typescript" ]; then
  GLOBAL="$(npm root -g 2>/dev/null)/typescript"
  if [ -x "$GLOBAL/bin/tsc" ]; then
    mkdir -p node_modules/.bin
    ln -s "$GLOBAL" node_modules/typescript
    ln -s ../typescript/bin/tsc node_modules/.bin/tsc
  else
    npm install --silent --no-audit --no-fund >/dev/null
  fi
fi
echo "Demo repo ready in $DIR"
