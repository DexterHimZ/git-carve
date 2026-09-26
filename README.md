<!-- DRAFT (M7). Not published. Before publishing: record the demo tape, re-verify §3.4 of
docs/SPEC.md again, and check private repository names under eval/. -->

# git carve

**git carve** carves a dirty working tree into commits that each pass your check.
It will not write your commit messages.

`git history split` splits a commit you already made. carve-cli stacks PRs. AI commit tools
group your changes and write the messages; at most they *ask* a model to keep each commit
buildable. **git carve splits your uncommitted work, checks every commit it makes, and never
writes the message.**

<!-- demo recording goes here (VHS tape, M7) -->

```console
$ git status --short
 M src/format.ts
 M src/main.ts
 M src/net.ts
 M src/parse.ts
 ...
$ git carve
```

Your editor opens once:

```text
# git carve: 3 commits, 2 hunks left out. Every commit below passed: tsc --noEmit -p .
#
# Write a message under each "commit" line (first line = subject).
# ...

commit


  @ a1   src/format.ts:1                  +1 -1  formatBytes  [new]
  @ a2   src/main.ts:2                    +1 -1
  @ a4   src/main.ts:10                   +1 -1  main
  @ a9   src/progress.ts:1                +1 -1
  ...

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

Type a message under each `commit` line, save, and close. That's all. Everything under `leave`
stays in your working tree, uncommitted.

## What it promises

- **Every commit passes your check.** carve runs the check on each commit's exact tree in a
  scratch worktree before it commits anything. With `--no-check`, the summary says the commits
  are unverified.
- **It never writes a commit message.** There is no `-m`, no suggestion, no summary. The
  buffer shows paths, line ranges, and symbol names, and you write every word.
- **It never touches your files.** carve writes git objects, the branch ref (compare-and-swap),
  the index once at the end, and its own files under `.git/carve`.
- **`git carve --undo` restores the exact state before the run.**
- **Same tree, same plan,** byte for byte.
- **It refuses states it cannot handle safely:** staged changes, a merge or rebase in progress,
  submodules.

**Your tree doesn't compile? carve commits what does.** If the working tree as a whole fails
the check, carve asks the compiler which changed lines it rejects, leaves those changes (and
anything that needs them) uncommitted, and commits the rest. The buffer tells you what stayed
out and quotes the compiler:

```text
# Your working tree does not pass the check, so a5 a7 a17 a18 stay uncommitted (under leave) and everything else passes:
#   src/net.ts(22,38): error TS2552: Cannot find name 'failur'. Did you mean 'failure'?
```

**Your project already has type errors at HEAD?** Many do. carve refuses by default, because
it cannot promise that commits pass a check that HEAD itself fails, and tells you how many errors
HEAD has. `git carve --no-new-errors` carves anyway, promising instead that no commit adds an
error HEAD didn't already have. The buffer and the summary say so in those words.

With the default check, "passes" means *type-checks*, not *works*. To make every commit
bisectable for a specific bug, use the test that reproduces it as the check:

```sh
git carve --check 'npx vitest run src/parse.test.ts'
```

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
- **Left out:** debug lines carve can tell are leftovers (`console.log`, `debugger`) when they
  stand alone. Logging in a `catch` block or right before a `throw` is error reporting, so it is
  committed.
- **In order:** a definition lands no later than the code that uses it.

Then the check decides. If two planned commits do not pass separately, carve merges them and
says so in the buffer. Regroup anything by moving `@` lines; if you change the grouping, carve
checks again before committing.

## Install

Not published yet. From source (Rust 1.85 or newer):

```sh
cargo install --path .
```

Planned: `cargo install git-carve`, and `npm install -g git-carve` with prebuilt binaries.
Because the binary is called `git-carve`, git finds it as `git carve`.

## Usage

| Command | What it does |
|---|---|
| `git carve` | Plan, check, open your editor once, commit |
| `git carve --plan-only` | Print the checked plan; commit nothing |
| `git carve --plan-file plan.txt` | Use a filled-in plan instead of the editor |
| `git carve --check '<cmd>'` | Use your own check (default: `tsc --noEmit` on the projects that own the changed files) |
| `git carve --no-check` | Skip checking; the summary says the commits are unverified |
| `git carve --no-new-errors` | When HEAD already fails the check: no commit may add a new error |
| `git carve --undo` | Restore the state before the last run |
| `git carve --json` | The plan and its evidence as JSON |
| `git carve --clean` | Remove the scratch worktree and caches |

`git config carve.check '<cmd>'` sets a per-repository default check.

## Scope (v1)

- **TypeScript** (`.ts`, `.tsx`, `.mts`, `.cts`) is understood at the symbol level. Other
  files are grouped by name and place.
- Commit hooks do not run on carve's commits.
- A debug line inside a larger changed region cannot be separated from it.

## Design

`docs/SPEC.md` is the specification. `docs/DECISIONS.md` records every design decision and the
debate behind it. `docs/EVAL.md` is the pre-registered evaluation protocol.
