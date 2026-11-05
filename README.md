<!-- DRAFT (M7). Not published. Before publishing: record the demo (docs/demo.tape), re-verify
§3.4 of docs/SPEC.md, and check private repository names under eval/. -->

# git carve

**Splits your mess into commits that each pass your check. Refuses to write your commit messages.**

After two hours of work (yours or your agent's), `git status` lists nine files. In them: a new
feature, a rename across six files, a one-line bug fix, and two `console.log`s you forgot to
delete. You could spend ten minutes in `git add -p` and hope each commit still compiles. Or:

```console
$ git carve
```

carve reads the code, splits the changes into commits, **runs your check on every one of them
before it exists** (by default, your compiler or type checker), and opens your editor once. You
type the messages. It never writes a word of them.

> **The code decides the grouping. Your check decides what passes. You decide what to say.**

<!-- demo recording goes here (docs/demo.tape) -->

## See it

```console
$ git status --short
 M src/format.ts
 M src/main.ts
 M src/net.ts
 ...                (9 files, +39 -14)
$ git carve
```

Your editor opens once, with a plan that has already passed the check:

```text
# git carve: 3 commits, 2 hunks left out. Every commit below passed: tsc --noEmit -p .
# ...

commit


  @ a1   src/format.ts:1                  +1 -1  formatBytes  [new]
  @ a2   src/main.ts:2                    +1 -1
  @ a4   src/main.ts:10                   +1 -1  main
  ...                                              (the rename, across 6 files)

commit


  @ a5   src/net.ts:11                    +1 -1  fetchAll
  @ a7   src/net.ts:16-28                 +13 -0  retryWithBackoff  [new]
  @ a17  tests/net.test.ts:1              +1 -1
  @ a18  tests/net.test.ts:7-16           +10 -0  testRetryWithBackoff  [new]

commit


  @ a8   src/parse.ts:8                   +1 -1  parseHeader

leave
  @ a3   src/main.ts:6                    +1 -0  main  [debug-only line]
  @ a6   src/net.ts:13                    +1 -0  fetchAll  [debug-only line]
```

The rename travels with every use of it, the feature with its test, and the fix stands alone.
The `console.log`s stay out of history. Type a subject under each `commit`, then save and close:

```console
Created 3 commits on main, each passed `tsc --noEmit -p .` (6 check runs, 0 merges, 5.0s):
  f489d2c Rename fmtBytes to formatBytes
  4702a85 Retry failed fetches with backoff
  d7479ea Fix off-by-one in parseHeader
Left in working tree: 2 hunks (src/main.ts:6, src/net.ts:13)
Undo: git carve --undo
```

Below the plan, the buffer shows each hunk's diff, as `git commit -v` does. Regroup by moving
`@` lines, reorder the blocks, or delete a `commit` line to merge it into the block above. If you
change the grouping, carve checks again before it commits anything.

## Why carve

- **Big diffs are the new normal.** At the median, pull requests written with AI help are more
  than three times the size of unassisted ones: 89–96 lines against 26
  ([LinearB, 8.1 million PRs](https://linearb.io/library/ai-in-software-development)).
  Small commits are how reviewers keep up.
- **Splitting by hand is slow, and nothing checks it.** `git add -p` can't tell you whether the
  commit you just staged compiles.
- **AI split tools write your messages and skip your check.** They group changes with a
  language model and generate the message. At most, they *ask* the model to keep each commit
  buildable; none we found runs your check on each commit. carve does it the other way round.

## What it promises

- **Every commit passes your check.** carve runs the check on each commit's exact tree, in a
  scratch worktree, before it commits anything. With `--no-check`, the summary says the commits
  are unverified.
- **It checks the check.** Before trusting a check, carve breaks a changed source file on
  purpose and makes sure the check fails. When the check can't see a file, carve says that
  file's changes are unverified instead of calling them passed.
- **It never writes a commit message.** There is no `-m`, no suggestion, no summary. The buffer
  shows paths, line ranges, and function names, and every word of the message is yours.
- **It never touches your files.** carve writes git objects, the branch ref (compare-and-swap),
  the index once at the end, and its own files under `.git/carve`.
- **`git carve --undo` restores the exact state before the run.**
- **Same tree, same plan,** byte for byte.
- **It refuses what it can't handle safely:** staged changes, a merge or rebase in progress,
  submodules.

## Already committed? Re-carve it

Agents commit as they go, and so do people: `wip`, `asdf`, `fix stuff`. carve doesn't need a
dirty tree. Give it a base, and it re-carves everything since then (the commits plus any
uncommitted changes) into commits that each pass your check:

```console
$ git log --oneline -3
fc7358d fix stuff
91b4d04 asdf
d0ad624 wip
$ git carve HEAD~3
```

The buffer lists the old commits for reference, and every new commit says where its lines
came from:

```text
# Re-carving 3 commits since 7edd6a6, plus any uncommitted changes.
# Their old messages, for reference only (nothing is copied into a commit):
#   d0ad624 wip
#   91b4d04 asdf
#   fc7358d fix stuff
...
commit
  @ a1   src/format.ts:1                  +1 -1  formatBytes  [new]
  ...
# from: d0ad624 wip (5 lines), fc7358d fix stuff (6 lines)
```

```text
Re-carved 3 commits since 7edd6a6 into 3.
Created 3 commits on main, each passed `tsc --noEmit -p .` (6 check runs, 0 merges, 5.6s):
  248d82c Rename fmtBytes to formatBytes
  4418701 Retry failed fetches with backoff
  294ead8 Fix off-by-one in parseHeader
Left in working tree: 2 hunks (src/main.ts:6, src/net.ts:13)
Undo: git carve --undo
```

The two `console.log`s the agent committed came back out of history and sit in your working
tree, uncommitted.

**What rewriting history means here:**
- **Same final code.** The new tip plus what was left out is byte-identical to before, and
  carve never writes a file in your working tree.
- **Exact undo.** `git carve --undo` puts the branch back on the old commits.
- **Fail closed:**
  - carve refuses ranges with merge commits;
  - it refuses commits already on a remote unless you pass `--rewrite-pushed`, because
    you'd need to force-push. `git carve @{u}` re-carves only what's unpushed.
- **`git carve main` keeps working after main moves on.** carve uses the merge-base and
  tells you so.

## From a branch to a stack

GitHub's stacked pull requests went GA in October 2026, but they assume you planned the stack
before writing code. carve splits what you already have into one, and every layer has
already passed your check:

```console
$ git carve main --stack
...
Stack on main (3 layers, each passed `tsc --noEmit -p .`):
  feat-1-rename-fmtbytes-to-formatbytes    14d9833  Rename fmtBytes to formatBytes
  feat-2-retry-failed-fetches-with-backof  c45e573  Retry failed fetches with backoff
  feat                                     e7b10b9  Fix off-by-one in parseHeader   (top)
To open the PRs without gh stack (nothing was pushed):
  git push -u <remote> feat-1-rename-fmtbytes-to-formatbytes feat-2-retry-failed-fetches-with-backof feat
  gh pr create --base main --head feat-1-rename-fmtbytes-to-formatbytes --fill
  ...
```

**How it works:**
- **One commit per layer.** Each PR's title is the subject you typed, and carve writes no PR
  text.
- **Your branch stays the top layer.** An existing PR keeps its identity.
- **`gh stack` gets one line.** With the [gh-stack](https://github.com/github/gh-stack)
  extension installed, carve prints `gh stack init --base main … feat && gh stack submit
  --auto` instead. `git carve main --stack --submit` runs it for you: it pushes the layers and
  opens draft PRs.
- **carve never pushes by itself.**
- **Re-split after review without losing the PRs.** Run `git carve main --stack` again, with
  your review fixes uncommitted. carve moves the existing layer branches instead of making
  new ones, so each PR keeps its number and comments. It prints the
  `git push --force-with-lease` command that updates them, and `git carve --undo` moves them
  back.
- **Graphite:** `gt track <layer> --parent <below>` for each layer, then
  `gt submit --stack`.
- **Squash-merge teams are fine.** Each layer squashes on its own, with your subject as the
  title.
- **Dependency safety.** A layer that starts using a package travels with the
  `package.json`, `Cargo.toml` or `go.mod` line that adds it. A layer can't pass locally only
  because a later layer installed its dependency.
- **`git carve --undo`** removes the layer branches too, unless you moved them.

## When your tree doesn't compile

Real working trees are half-finished. If yours fails the check as a whole, carve asks the
compiler which changed lines it rejects. It then leaves those lines (and anything that needs
them) uncommitted, commits the rest, and quotes the error:

```text
# git carve: 2 commits, 6 hunks left out. Every commit below passed: tsc --noEmit -p .
# Your working tree does not pass the check, so a5 a7 a17 a18 stay uncommitted (under leave) and everything else passes:
#   src/net.ts(22,38): error TS2552: Cannot find name 'failur'. Did you mean 'failure'?
```

**Your project already has type errors at HEAD?** Many do. By default carve refuses, because it
cannot promise that commits pass a check HEAD itself fails, and it tells you how many errors
HEAD has. `git carve --no-new-errors` carves anyway, promising instead that no commit adds an
error HEAD didn't already have. The buffer and the summary say so in those words.

## Gate every push

`git carve verify` checks every commit on your branch, oldest first, in a scratch worktree.
Your files, index, and refs are never touched. When a commit fails, it quotes the compiler and
names the commit that introduced each error:

```console
$ git carve verify main..HEAD
git carve verify: 3 commits, check: tsc --noEmit -p .
  ok    83cc25329 Rename fmtBytes to formatBytes  (0.9s)
  FAIL  2b5ccc0c9 Retry failed fetches with backoff  (0.9s)
        src/net.ts(22,38): error TS2552: Cannot find name 'failur'. Did you mean 'failure'?
  FAIL  870962d51 Fix off-by-one in parseHeader  (0.8s)
        same errors as 2b5ccc0c9, which introduced them
2 of 3 commits do not pass the check.
```

Results are cached by tree, so a second push re-checks nothing. Running the branch above again
takes a quarter of a second. Commits are checked in parallel (two at a time for TypeScript;
`git config carve.checkJobs 3` for more, or for your own check).

Install it as a pre-push hook. As a hook, it stops at the first failing commit, because a
push needs a verdict, not a report:

```sh
git carve hooks install      # writes .git/hooks/pre-push; never replaces a hook you already have
```

With husky, put `git-carve verify --pre-push` in `.husky/pre-push` (`git carve hooks print git`
shows the hook).

**Coding agents.** The pre-push hook already stops an agent's bad push. An agent hook also
explains the refusal to the agent: the failing commit, its errors, and how to fix it. It
refuses `--no-verify` too. carve prints the setup and never edits your agent's config:

```sh
git carve hooks print claude   # or cursor, codex: merge the printed JSON into its settings
```

Agents never write your commit messages. An agent can prepare a re-split from `git carve @{u}
--plan-only --json` (its `buffer` field), with its suggestions as `# proposed:` comments. You
open that with `git carve @{u} --plan-file <file> --edit` and write the messages yourself.
A plan file with messages already in it is refused, unless you opt in for the repository:
`git config carve.agentMessages true`.
`agent/skills/git-carve/SKILL.md` teaches an agent this recipe.

In CI (GitHub Actions), with the npm package:

```yaml
- uses: actions/checkout@v4
  with: { fetch-depth: 0 }
- run: npm ci
- run: npx git-carve verify origin/${{ github.base_ref }}..HEAD
```

Add `--no-new-errors` if the branch's base already fails the check. Each commit then fails only
for errors the base did not have. `git config carve.noNewErrors true` does the same for the hook.

## Not just TypeScript

| Language | How carve reads it | Default check |
|---|---|---|
| TypeScript (`.ts`, `.tsx`, `.mts`, `.cts`) | full parser: names resolved through imports | `tsc --noEmit` on the projects that own your changes |
| Rust | definitions by keyword, function extents, names matched across changed files | `cargo check --all-targets` |
| Go | as above | `go build ./... && go vet ./...` |
| Python | as above | `python3 -m compileall` (syntax only) |
| Java, Kotlin, C, C++, C#, Ruby, PHP, Swift, Scala, Elixir, Lua, Dart | as above | none: pass `--check` or set `git config carve.check` |
| everything else | grouped by file and name | none |

The same mess in Python (a new function, its caller, and its test; a drive-by fix; a stray
`breakpoint()`) comes out the same way:

```text
# git carve: 2 commits, 1 hunk left out. Every commit below passed: python3 -m compileall (syntax only)
# ...

commit


  @ a1   inventory/cli.py:3               +1 -1
  @ a2   inventory/cli.py:9-10            +2 -0  main
  @ a5   inventory/store.py:16-19         +4 -0  low_stock  [new]
  @ a6   tests/test_store.py:1            +1 -1
  @ a7   tests/test_store.py:6-10         +5 -0  test_low_stock  [new]

commit


  @ a3   inventory/store.py:5             +1 -1  load

leave
  @ a4   inventory/store.py:14            +1 -0  total_stock  [debug-only line]
```

On 584 windows of real history from five open-source Python, Go, and Rust projects, this
reading matched the developers' own commits more closely than grouping by file did, in every
project. Any check works with any language: `git carve --check "pytest -q"`.

## How it groups

Code first, and only what code can prove:

- **Always together:** a renamed symbol and every use of it (the new definition must come
  first and the old uses must go first, so they form one commit), a manifest and its lockfile,
  the two halves of a renamed file, and a file's mode change and its content.
- **Together by evidence:** a new function and the code that uses it, a signature change and
  its callers, edits inside the same function, a rare identifier shared by a few edits, one
  mechanical edit repeated across files, and a test with the new code it calls.
- **Together by place:** other edits in the same file, except a small drive-by fix in an
  otherwise untouched function, which stays its own commit. Edits joined only by sharing a file
  are marked `[same file]`, so you can see where to split.
- **Left out:** debug lines carve can tell are leftovers (`console.log`, `debugger`,
  `breakpoint()`, `dbg!`) when they stand alone. Logging in a `catch` block or right before a
  `throw` is error reporting, so it is committed.
- **In order:** a definition lands no later than the code that uses it.

Then the check decides. If two planned commits do not pass separately, carve merges them and
says so in the buffer.

## How it compares

| | Splits uncommitted work | Runs your check on each commit before it exists | Writes your messages |
|---|---|---|---|
| `git add -p` | by hand | no | no |
| `git history split` | no: splits a commit you already made | no | no |
| git-absorb | no: folds changes into commits you already made | no | no |
| `jj run`, git-branchless `git test` | no | no: checks commits after they exist | no |
| AI split tools (Cursor, omp commit) | yes, with a language model | no | yes |
| **git carve** | **yes, from the code** | **yes** | **never** |

## Install

Not published yet. From source (Rust 1.85 or newer):

```sh
cargo install --path .
```

Planned: `npm install -g git-carve` (prebuilt binaries for Linux and macOS, no install scripts)
and `cargo install git-carve`. Because the binary is called `git-carve`, git finds it as
`git carve`. On Windows, use WSL. The TypeScript check uses your project's own `typescript`.

## Usage

| Command | What it does |
|---|---|
| `git carve` | Plan, check, open your editor once, commit |
| `git carve <base>` | Re-carve everything since `<base>` (commits and uncommitted changes), for example `git carve @{u}` |
| `git carve <base> --rewrite-pushed` | Also re-carve commits already on a remote (you will force-push) |
| `git carve <base> --stack` | Also make one branch per commit: a stack of PRs, each layer already checked; prints the commands to submit it |
| `git carve <base> --stack --submit` | Submit the stack with `gh stack` (pushes the layers, opens draft PRs) |
| `git carve --plan-only` | Print the checked plan; commit nothing |
| `git carve --plan-file plan.txt --edit` | Start the editor from a prepared plan (an agent's `# proposed:` lines, say) and write the messages |
| `git carve --check '<cmd>'` | Use your own check (default: the language's, above) |
| `git carve --no-check` | Skip checking; the summary says the commits are unverified |
| `git carve --no-new-errors` | When HEAD already fails the check: no commit may add a new error |
| `git carve --undo` | Restore the state before the last run |
| `git carve --json` | The plan and its evidence as JSON |
| `git carve --clean` | Remove the scratch worktree and caches |
| `git carve verify [RANGE]` | Check every commit in a range (default: what the upstream lacks); `--fail-fast` stops at the first failure |
| `git carve hooks install` | Check every push first (git's pre-push hook) |
| `git carve hooks print claude` | Print the agent hook that explains refused pushes (also `cursor`, `codex`) |

`git config carve.check '<cmd>'` sets a per-repository default check.

## FAQ

**Why won't it write my commit messages?** A message records why a change was made, and only
its author knows that. A generated message describes the diff, which the diff already does.
carve does the mechanical part (grouping, ordering, checking) and leaves the words to you.

**What does "passes" mean?** By default, it type-checks (TypeScript, Rust, Go) or parses
(Python). It does not mean "works". To make every commit bisectable for a specific bug, use the
test that reproduces it as the check: `git carve --check 'npx vitest run src/parse.test.ts'`.

**Is it fast?** Checks keep their incremental build state between runs (tsc's build info,
cargo's target directory), and results are cached by tree, so re-runs and regroupings reuse
earlier work. The demo above runs the check 6 times in 5 seconds, most of it `tsc`.

**Can I use it on my coding agent's output?** That is one of the main uses. Let the agent
finish, run `git carve`, review the plan, and write the messages yourself.

## Limits

- Commit hooks do not run on carve's commits. Put your lint in `--check` instead, and it runs
  on every commit.
- A debug line inside a larger changed region cannot be separated from it.
- A re-carve makes new commits: they are yours, with today's date. The old commits' authors and
  dates are not kept (their trailers are listed in the buffer for you to copy).
- When a re-carved range adds a dependency, every commit is checked against your current
  `node_modules`, so a commit that uses the package before the one that adds it can still pass.
- Outside TypeScript, carve reads code by keywords and indentation, not a full parser. Two files
  are linked by a name only when exactly one other changed file defines it.

## Learn more

- [ROADMAP.md](ROADMAP.md): what is done and what comes next
- [docs/SPEC.md](docs/SPEC.md): the specification
- [docs/DECISIONS.md](docs/DECISIONS.md): every design decision and the debate behind it
- [docs/EVAL.md](docs/EVAL.md): the pre-registered evaluation protocol
