# git-carve: proposal and technical specification (v1)

Status: ready for implementation. Audience: implementers, including Claude Code.
Section numbers are referenced from `docs/EVAL.md` and `docs/DECISIONS.md`. Keep them stable.

---

## 1. Summary

`git carve` takes a dirty working tree that contains several logical changes, such as a
feature, a drive-by bug fix, a rename, and forgotten debug prints, and turns it into a short
sequence of commits. It guarantees one thing mechanically: **every commit it creates passes
the user's check command.** It deliberately does not write commit messages. The user writes
all of them at once in a single editor buffer that shows the proposed commits.

The architecture follows one rule: **code builds and proves the structure; a model only
labels what code cannot decide.** A symbol graph from tree-sitter and a few git-derived
rules group the changes. The user's compiler or test command proves each commit. An
optional judge (Laya, a self-hosted classifier that returns only probabilities; it replaced Jev on 2026-09-24, DECISIONS.md D20) may veto weak merges,
propose merges the graph missed, and spot leftover debug output. The tool is fully useful
without the judge.

## 2. Problem

Developers routinely end up with a dirty tree that is obviously two or three commits.
Splitting it by hand with `git add -p` is slow, error-prone, and rarely verified: the
intermediate commits often do not compile, which breaks `git bisect` and makes reverts
unsafe.

Existing AI tools approach this as an agent task: read the diff, group the files, write
messages. Some of them *instruct* the model to keep every commit buildable. None of the
ones we found *prove* it, and all of them write the messages, which many developers now
distrust.

## 3. Positioning

### 3.1 One-liner (the first screen of the README)

> **git carve** carves a dirty working tree into commits that each pass your check.
> It will not write your commit messages.

### 3.2 Contrast sentence (also on the README's first screen)

> `git history split` carves a commit you already made. carve-it writes your messages.
> carve-cli stacks PRs. Agent skills *ask* a model to keep commits buildable.
> **git carve splits your uncommitted work and proves every commit passes your check,
> in seconds, and never writes the message.**

Before publishing, re-verify every competitor claim in this section. They came from
planning research and were not all independently confirmed (see §3.4).

**Re-verified 2026-09-24** (web; the table in §3.4 carries each result):
- `git history split` is confirmed (Git 2.54, experimental; interactive hunk selection on a
  commit already made).
- carve-cli is confirmed (crates.io `carve-cli` 0.2.2, created 2026-06-09: "split a
  monolithic branch into a reviewable stacked-PR set").
- **carve-it could not be found**, so it is dropped from the contrast sentence.
- AI grouping tools that write messages are confirmed (aic, Chronicle), and so are agent
  skills that instruct buildable atomic commits.
- `git-carve` is still unclaimed on crates.io and npm (both 404).

Contrast sentence as it stands after the re-verification:

> `git history split` splits a commit you already made. carve-cli stacks PRs. AI commit tools
> group your changes and write the messages; at most they *ask* a model to keep each commit
> buildable. **git carve splits your uncommitted work, checks every commit it makes, and never
> writes the message.**

### 3.3 Wording rules

- Say "each commit passes your check." Do **not** say "bisectable" unless the check is the
  test that reproduces the bug. Document `--check 'cargo test the_failing_test'` as the
  bisect mode.
- If the default check is compile-only, say "compiles," not "works."
- Never describe the judge as "AI that understands your code." Describe what it does: it
  vetoes or suggests groupings and flags leftover debug lines, and never writes text.
- Lead with the demo recording, not with the judge. The design thread comes a day later, and only
  if the evaluation (EVAL.md) earned it.

### 3.4 Landscape (re-verify before launch)

| Tool | What it does | Why git carve is different |
|---|---|---|
| `git history split` (Git 2.54, experimental; **confirmed** 2026-09-24) | Interactively splits an existing commit | Works on commits already made; no automatic grouping; no check |
| carve-it (Claude skill, reported) | *Not found on 2026-09-24; dropped from launch copy* | – |
| carve-cli (crates.io 0.2.2, created 2026-06-09; **confirmed**) | One branch into stacked PRs | Different unit (PRs, not commits from a dirty tree) |
| aic, Chronicle (**confirmed** 2026-09-24) | Group uncommitted hunks by concern and commit each group with a generated message | Writes messages; no check-based proof |
| conventional-commits and commit-prompt style agent skills | Group working-tree changes into commits and write messages; some instruct the agent to keep each commit buildable | Instruction, not proof; minute-long agent sessions; write messages |
| smart-commit skill, Chronicle, smart-git-commit, HagiCode "AI Compose Commit" | LLM grouping of uncommitted changes plus generated messages | Same as above |
| git-absorb | Assigns fixup hunks to earlier commits | Different job (fixups into history) |
| GitButler virtual branches, `jj split` | Manual hunk assignment | Manual; no proof |
| SmartCommit and "untangling" research | Graph-based partitioning of tangled changes | Research prototypes; no check-based proof |

Names: publish as the crate/package `git-carve` with the binary `git-carve`, so that
`git carve` works via git's external-command lookup. Do **not** take the crate name `carve`
(already used by a log TUI; `carve-cli`, `carve-lang`, and borkdude/carve also exist).

## 4. Scope

### 4.1 In v1

- One target language for symbol extraction: **TypeScript** (`.ts`, `.tsx`, `.mts`, `.cts`),
  decided 2026-09-23 (§15). Files in other languages, including `.js`, still become atoms;
  they just get no symbol edges.
- Tracked modifications, deletions, additions of untracked non-ignored files, mode changes,
  binary files (whole-file only), and symlinks (whole-entry only).
- Code-only grouping (§6), check verification with merge-forward (§8), one plan buffer (§9),
  commit creation with a compare-and-swap ref update, and one-level exact undo (§10).
- Optional judge (§7) behind the `Judge` trait, with `NullJudge`, `FixtureJudge`, and
  `LayaJudge`.
- Evaluation harness (EVAL.md).
- macOS and Linux.

### 4.2 Out of v1 (explicitly)

Ticket linking, changelogs, multiple languages, jj support, stacked PRs, staged/partially
staged input, submodules, sparse checkouts, LFS partials, Windows, running commit hooks,
splitting inside a single atom, and any message generation (never, not just in v1).

### 4.3 Non-goals (permanent)

- Generating or suggesting commit message text.
- Editing the user's source files (no auto-removing debug lines from the working tree; the
  "leave" set simply stays uncommitted).
- Rewriting existing history.

## 5. Core model

### 5.1 Preconditions (fail closed)

Refuse with a clear message and exit code 2 if any of these hold:

- Not inside a work tree, or the repository is bare.
- HEAD is unborn (no commits yet).
- An operation is in progress: `MERGE_HEAD`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`,
  `rebase-merge/`, `rebase-apply/`, or `BISECT_LOG` exists under `git rev-parse --git-dir`.
- The index differs from HEAD (`git diff --cached --quiet` fails). This includes
  intent-to-add entries. Hint: `git reset` (mixed) unstages everything and keeps all
  changes in the working tree.
- Unmerged entries exist.
- A submodule pointer changed.
- `core.sparseCheckout` is enabled.
- There is nothing to carve.
- Size limits are exceeded: more than `carve.maxAtoms` atoms (default 400) or more than
  200 changed paths.

### 5.2 Snapshot

1. Record `HEAD` (oid, and the symbolic ref or detached state).
2. Enumerate changes with `git status --porcelain=v2 -z --untracked-files=all`.
3. For each changed or added path, get the **clean** blob of the working-tree content with
   `git hash-object -w -- <path>`. This applies clean filters and end-of-line conversion,
   so all diffing happens in repository form. Batch the hashing with `--stdin-paths`.
4. For each path, record the base entry (mode, oid) from HEAD via `git ls-tree -z HEAD -- <paths>`.
5. Record a fingerprint of the working tree (path, size, mtime, clean-blob oid). This is
   re-checked just before the ref update (§10.3).

Paths with a `filter` attribute (`git check-attr filter -- <path>`), such as LFS, become
whole-file atoms.

### 5.3 Atoms

Atoms get stable ids `a1..aN`, ordered by (path, base line, kind).

- **Text modification.** Line diff of base blob against the clean new blob, using the
  histogram algorithm (`imara-diff`), with zero context. Each maximal changed region is one
  atom: base range `[bs, be)`, new range `[ns, ne)`, and new lines. Regions separated by at
  least one unchanged line are separate atoms. Lines keep their terminators, so a missing
  final newline is just a different last line (test this explicitly).
- **Whole-file atoms**: added file (untracked), deleted file, binary (NUL in the first
  8000 bytes, or a `-diff`/`binary` attribute), filtered file, symlink change, non-UTF-8
  file.
- **Mode atom**: an executable-bit change, separate from content atoms of the same file.
- **Untracked defaults**: untracked files larger than 1 MiB, or binary, default to `leave`
  (the user can move them).
- **Rename pairs**: run `git diff -M --name-status` (HEAD against the worktree, with
  untracked files included via a temporary index) to pair a deleted path with an added path.
  Pairs get a hard bind (§6.3, B3). v1 does not diff across a rename.

### 5.4 Reconstruction

For a set `S` of selected atoms and a file `f`:

```
reconstruct(f, S) = walk base lines of f; for each atom in S ∩ atoms(f), in base order,
                    replace base[bs..be) with the atom's new lines; copy everything else.
```

Whole-file atoms either apply (add, delete, or replace the entry) or do not. A mode atom
decides the mode. Otherwise the base mode is kept (or the worktree mode for added files).

Reconstructed content is written with `git hash-object -w --no-filters --stdin`, because it
is already in repository form.

**This replaces `git apply --cached --unidiff-zero`.** Applying patches is fragile when the
subsets are arbitrary (offset drift, adjacent hunks, context interplay). Reconstructing
content from base plus disjoint regions is exact by construction.

### 5.5 Invariants (property-tested with `proptest`, at least 1000 cases each)

- **R1** `reconstruct(f, all_atoms(f))` equals the clean worktree blob, byte for byte.
- **R2** `reconstruct(f, ∅)` equals the base blob, byte for byte.
- **R3** For any partition of the atoms into ordered groups, the tree after the last group
  equals the tree of HEAD plus all non-leave atoms, independent of the partition.
- **R4** Every atom appears in exactly one group or in leave. No atom is lost or duplicated.
- **R5** The working tree's bytes are identical before and after a run (safety suite).

Generators: random files (including CRLF lines, a missing final newline, empty files, and
unicode), random edit scripts, and random subsets and partitions.

## 6. Code graph

### 6.1 Symbol extraction (tree-sitter)

A `LanguageAdapter` trait for the single v1 language:

```rust
trait LanguageAdapter {
    fn matches(&self, path: &Path) -> bool;
    fn parse(&self, src: &[u8]) -> Option<Tree>;                 // None => no symbol facts
    fn defs(&self, tree: &Tree, src: &[u8]) -> Vec<Def>;          // name, kind, line range, scope
    fn refs(&self, tree: &Tree, src: &[u8]) -> Vec<Ref>;          // name, line range
    fn imports(&self, tree: &Tree, src: &[u8]) -> Vec<Import>;    // names brought into file scope, line range
    fn enclosing_def(&self, tree: &Tree, line: u32) -> Option<DefId>;
    fn whitespace_significant(&self) -> bool;                     // true for Python, YAML, Makefile
    fn debug_patterns(&self) -> &[Regex];                         // e.g. dbg!, println!, console.log
    fn manifest_lock_pairs(&self) -> &[(&str, &str)];             // e.g. Cargo.toml <-> Cargo.lock
    fn default_check(&self) -> &str;
}
```

Use the grammar's `tags.scm` queries (via `tree-sitter-tags`) where they are available.
Otherwise use queries in `queries/<lang>/`. Parse the **base** and **final** version of each
file in the target language. Never parse intermediate reconstructions.

Name resolution is by name only (no type resolution):

- Top-level or exported definitions match across files.
- Imports and locals match only within their own file.
- Ignore names shorter than 3 characters and names in a stoplist (`new`, `get`, `set`,
  `init`, `main`, `test`, `self`, `this`, `value`, `data`, `result`, `err`, `ctx`, plus a
  per-language list).
- A name defined in more than one place in HEAD produces only soft edges, never hard ones.

**TypeScript refinements (DECISIONS.md D7, D10).** The adapter is one tree-sitter walk per
file (`analyze` returns definitions, imports, references, and debug statements together),
not separate query calls. A cross-file reference is **strong** only through an import whose
source resolves (a relative path, the `.js`-suffix ESM style, or a tsconfig `paths` alias)
to the changed file that defines the name. Unresolvable imports and unimported names are
**weak**: F2 evidence only, never an order constraint. This replaces the "defined in more
than one place in HEAD" rule above, which would need a repository-wide parse. The stoplist
applies to weak matches and file-local names, not to strong import matches.

### 6.2 Facts per atom

Computed by intersecting atom line ranges with definitions and references (base lines for
removals, final lines for additions):

- `adds_def(S)`: S is defined in final within the atom's new lines, and S is not defined in
  base in this file.
- `modifies_def(S)`: S is defined in both, and the atom touches S's line range.
- `removes_def(S)`: S is defined in base within the atom's removed lines, and not in final
  in this file.
- `adds_ref(S)` and `removes_ref(S)`: likewise for references.
- Imports count as file-scoped definitions of the names they bring in.
- `enclosing(A)`: the innermost existing definition (present in both base and final) that
  contains the atom. One-line variables and fields do not count as enclosing; multi-line
  ones (arrow-function components, config objects) do.
- Clarifications (M2): `adds_ref`/`removes_ref` are **net** per resolved definition (added
  lines minus removed lines), so a name that appears on both sides of an edited line is
  context, not a change. `modifies_def` keeps only the innermost touched definitions.

### 6.3 Edges

**Hard binds** (same group, never sent to the judge):

- **B1** Atoms overlapping the same newly added definition.
- **B2** Manifest and lockfile, including all their atoms (Cargo.toml/Cargo.lock,
  package.json with its lock files, go.mod/go.sum, pyproject.toml with its lock files).
- **B3** Rename pairs (§5.3).
- **B4** A mode atom and the content atoms of the same file.

**Order constraints** (group of A comes no later than group of B):

- **O1** `adds_def(S)` in A and `adds_ref(S)` in B.
- **O2** `removes_ref(S)` in B and `removes_def(S)` in A (B no later than A).

A rename therefore produces a cycle (the new definition must come first, and the old
references must be removed first), which collapses into a single group via SCC merging.
That is exactly the right result.

**Soft affinities** (merged in code-only mode; the judge may veto them):

- **F0** `adds_def(S)` in A and a strong `adds_ref(S)` in B (DECISIONS.md D10). No rarity cap:
  a symbol that does not exist in HEAD can only be used by changes that came with it.
- **F1** Same `enclosing` definition.
- **F2** A shared rare identifier: both atoms add or remove refs or defs of X, X passes the
  stoplist, `len(X) ≥ 4`, and X appears in at most 3 atoms.
- **F3** `modifies_def(S)` in A and `adds_ref(S)` or `removes_ref(S)` in B. This covers a
  signature change together with its callers. v1 cannot tell a signature change from a body
  change, so this edge is soft: if the judge vetoes it and it was a signature change, the
  check fails and merge-forward (§8.4) repairs it. The compiler is the tiebreaker.

- **F6** Systematic edits (DECISIONS.md D26): atoms whose removed→added text is identical (at
  least 12 non-space characters), or whose small token substitution is identical (1–6 words
  each way, one non-numeric word of 3+ characters). Applies to symbol-less atoms too.

- **F7** Same file (DECISIONS.md D41): atoms in one file join, except a *drive-by shape*
  (a modification of at most 3 lines on each side that defines nothing and is the only change
  in its function), which never joins by file alone. Atoms whose group membership rests on F7
  alone are tagged `[same file]` in the buffer. The judge's features ignore F7.

F2 merges in code-only mode. D11 had stopped that; D11's own re-validation rule reversed it
at M5 (D26). F2 pairs are the judge's first veto candidates (§7.3).

**Excluded from affinity**: style atoms (§6.5), debug candidates (§6.5), and import-only
atoms (every changed line is inside an import statement; they would chain unrelated features
through a shared import line, DECISIONS.md D5). They join
groups only through hard binds or the post-passes.

### 6.4 Grouping algorithm

1. Union-find over the hard binds (B1–B4).
2. Code-only mode: union every soft edge (F1–F3).
3. Judge mode: skip step 2 and merge the hard-bound components by summed evidence instead
   (§7.4, DECISIONS.md D4).
4. Components become groups.
5. Lift O1/O2 to the group graph. Merge strongly connected components (Tarjan).
6. Topologically sort. Tie-break deterministically by the smallest (path, base line) among
   each group's atoms. Style group last (§6.5).
7. Import post-pass: an atom that only adds imports moves into the **earliest** group that
   contains an `adds_ref` of any imported name in the same file. An atom that only removes
   imports moves into the **latest** group containing a matching `removes_ref`. This keeps
   unused-import errors (Go) and lints from appearing between commits.
8. Leave pass (§6.5).
9. Verify (§8).

**Placement rules (M2).** Components made only of symbol-less atoms (files without an
adapter, binaries, mode changes) join the group with the most atoms from files with the same
stem, such as `Button.module.css` with `Button.tsx`; otherwise they stand alone and count in
the `orphan_groups` diagnostic (DECISIONS.md D9). Kept debug candidates join the nearest
group in their file. Import-only atoms whose new names are used by two or more groups are
reported as `bridging_import_atoms` (D5). All three are post-passes, so they cannot chain
groups together.

### 6.5 Style atoms and debug leftovers

- **Style atom**: the region differs from base only in whitespace, and the adapter is not
  whitespace-significant. Style atoms that are not hard-bound go to a trailing group marked
  `# style-only (whitespace)`. The user still writes its message.
- **Debug candidate**: an atom that only adds lines (no removals), where every added
  non-blank line matches the adapter's `debug_patterns`, and which is not hard-bound.
  - Code-only mode (DECISIONS.md D8): leave the candidate only if it is `debugger;`, or a
    `console.log/debug/trace/dir/table` call whose method this file in HEAD never calls.
    Otherwise it stays in the nearest group in its file, tagged `looks like debug output`.
    `console.error`, `warn`, and `info` are never debug candidates. Matching is structural
    (parser), not regex.
  - Judge mode: ask `debug_leftover` (§7.3). Leave the atom only if the probability is at
    least `T_debug`. Otherwise it stays in its component's group.
- Debug lines inside a larger changed region cannot be separated in v1 (atom granularity).
  Document this limitation.
- If the "final" tree (all non-leave atoms) fails the check but the worktree-equivalent
  tree passes, the leftovers were load-bearing. Put them all back into their groups and
  report it (§8.3).

## 7. Judge (Laya)

### 7.1 Role

The judge only answers questions that code cannot: whether two nearby edits belong to the
same intent, and whether a print statement is forgotten debugging. It never produces text,
never sees the plan buffer, and never decides anything the check can prove.

**What `--judge` runs today (DECISIONS.md D28, D31, D33; `eval/RESULTS.md`).** D21's ladder,
rung 1: a logistic model over carve's own 20 pair features, with its weights compiled into the
binary. It is exactly the judge frozen in `eval/LOCK`, and a unit test ties the two together.
- **Mechanics:** hard binds first; then pooled-linkage agglomeration of hard-bound
  components while the Platt-scaled P(same commit) ≥ 0.70; then the same post-passes as code-only
  mode.
- **Properties:** local, deterministic, and it cannot time out, so the fail-open path is never
  needed. It never decides debug lines (code-only D8 does).
- **Explanations:** `--json` lists every merge with its probability and top features
  (`diagnostics.judge_merges`).
- **Status:** EVAL verdict *optional flag* (provisional until E7), so code-only is the default.
- **Laya:** the `LayaJudge` below remains the interface for a later rung (rung 2, a fine-tuned
  Laya), which needs its own protocol (D34).

### 7.2 Interface

```rust
pub enum Question {
    Noul   { id: String, kind: NoulKind, state: serde_json::Value },
    Choice { id: String, kind: ChoiceKind, options: Vec<String>, state: serde_json::Value },
}
pub enum Answer { Noul { id: String, p: f32 }, Choice { id: String, dist: Vec<f32> } }

pub trait Judge {
    fn name(&self) -> &str;              // included in cache keys, e.g. "laya:english@55cf4c4"
    fn ask(&self, batch: &[Question], deadline: Duration) -> Result<Vec<Answer>, JudgeError>;
}
```

- `NullJudge`: returns an error immediately, so callers take the code-only path.
- `FixtureJudge`: answers from a JSONL file keyed by question id or state hash. Used in
  tests and for eval replay.
- `LayaJudge` (DECISIONS.md D20): HTTP to a running `laya-serve` at `LAYA_URL` (default
  `http://127.0.0.1:8000`), `POST /v1/systemone` with `{state, questions, model?}`, answers in
  `answers.<id>.noul` / `.choice`. Bearer token from `LAYA_API_KEY` when set. One request per
  state (the protocol takes one state), up to 4 concurrent requests, all within the run's
  deadline. `name()` is `laya:<checkpoint>@<hf-revision>`, so a model update invalidates the
  cache. Only fields that `laya/serve.py` at the pinned version accepts are sent.

v1 uses only Nouls. `Choice` exists in the trait for v2 (for example, "which group does
this orphan atom belong to?").

### 7.3 Questions (v1: exactly two kinds)

Keep each question to a single concept. Do not merge several concerns into one question.

**`same_change`** (Noul): *"Would a careful engineer put these two edits in the same commit?"*

- **Soft-edge use**: asked for every soft edge (F1–F3). The answer is evidence for or
  against keeping the edge (§7.4).
- **Proposal use**: asked for F2 pairs (DECISIONS.md D11), and for pairs of components after hard binds that share a file, sit
  within 60 lines of each other, and have no edge between them. Use the closest atom pair
  as the representative. The answer is evidence for a new edge (§7.4).
- State: `{path_a, path_b, enclosing_a, enclosing_b, line_distance|null, diff_a, diff_b,
  shared_identifiers}`, where each diff has at most 40 lines prefixed with `-`/`+`, plus 3
  lines of surrounding context marked as context.

**`debug_leftover`** (Noul): *"Is this added line temporary debugging output the author
forgot to remove?"*

- Asked for every debug candidate (§6.5).
- State: `{path, enclosing, added_lines, pattern_occurrences_in_head, file_has_other_logging}`,
  where `pattern_occurrences_in_head` counts how often the same call pattern appears in HEAD
  across the repo (from `git grep -c`). Heavy existing use suggests intentional logging.

All questions for a run are sent in **one batch**, including speculative ones. Answers that
turn out not to be needed are ignored.

### 7.4 Evidence merge, thresholds, and caps

Judge mode groups by summed evidence, not per-edge thresholds (DECISIONS.md D4):

1. Start from the hard-bound components.
2. Each asked pair (a soft edge or a proposal) between components X and Y contributes
   `w = prior(kind) + logit(p)`, with `p` clipped to [0.01, 0.99]. `prior(kind)` is the logit
   of that edge kind's precision, counted on the tune split and clipped to [-2, 2].
   Proposals have prior 0. Soft edges beyond the cap contribute `prior(kind)` alone.
3. `score(X, Y)` is the sum of `w` over all pairs between X and Y.
4. Repeatedly merge the highest-scoring pair while its score is greater than `θ`, adding
   scores when components merge. Break ties by the smallest (path, base line) of each pair.

Parameters: `θ = 0` and `T_debug = 0.70` initially. Both are tuned **only** on the tune
split (EVAL.md), then frozen. If the judge's expected calibration error on the tune split exceeds
0.1, Platt scaling fitted on the tune split is applied to every `p` before step 2.

Each pair is sent with the lower atom id first, so questions are deterministic and
cacheable. EVAL.md measures order sensitivity once, on the tune split.

Caps per run: 150 `same_change` questions and 40 `debug_leftover` questions. Prioritize by
line proximity. Proposals beyond the cap are not asked.

### 7.5 Cache, determinism, and failure

- Cache key: `sha256(judge.name() || kind || canonical_json(state))`. Store it in
  `<git-path carve>/judge-cache.jsonl`. A cache hit never calls the network, so re-runs on
  the same tree give identical plans.
- Deadline: `carve.judgeTimeout` (default 3s) for the whole batch. On timeout, error, or a
  malformed response, fall back to code-only for the **entire run** (never a partial mix),
  and print one line: `judge unavailable (<reason>); using code-only grouping`.

### 7.6 Privacy

The built-in judge (§7.1) sends nothing anywhere. `carve.judge` defaults to `none`. With a local `laya-serve` (a loopback `LAYA_URL`), nothing
leaves the machine. When `LAYA_URL` points elsewhere, the first use in a repo prints exactly
what is sent (hunk text of candidate pairs, file paths, symbol names) and records the
acknowledgment in `<git-path carve>/judge-ack`.

## 8. Verification

### 8.1 Check command

Resolution order: `--check '<cmd>'`, then `git config carve.check`, then the adapter's default:

| Language | Default check |
|---|---|
| Rust | `cargo check --all-targets --quiet` |
| TypeScript | `npx --no-install tsc --noEmit -p .` |
| Go | `go build ./... && go test -run '^$' ./...` (compiles tests without running them) |
| Python | `python -m compileall -q .` (syntax only; print a note that it is weak and suggest pyright/mypy) |

`--no-check` skips verification entirely. The summary must then say the commits are
**unverified**.

Run the check with `sh -c` in the scratch worktree, stdin closed, a per-run timeout
(`carve.checkTimeout`, default 600s), and output captured. On failure, show the last 40
lines.

### 8.1.1 TypeScript check hazards (resolve in M3)

The spec's default TypeScript check (`tsc --noEmit -p .`) is unsafe as written. These cases
came from real repositories and current project templates (checked 2026-09-23):

- **A vacuous pass.** The current `create-vite` `react-ts` template's root `tsconfig.json` is
  `"files": []` plus `references` to `tsconfig.app.json` and `tsconfig.node.json`. Running
  `tsc --noEmit -p .` there type-checks nothing, so every commit "passes." That is a false
  proof and violates non-negotiable 3. The adapter must detect a root config with no inputs
  and `references`, and either check each referenced project or refuse with a hint.
- **No root `tsconfig.json`.** Frontends often live in a subdirectory (`app/frontend/`). The
  default then fails the base precheck with a confusing error. Detect this, list the tracked
  `tsconfig*.json` paths, and suggest `git config carve.check`.
- **Gitignored inputs.** `create-next-app` gitignores `next-env.d.ts`, so it is missing from
  the scratch worktree. Add it to the default `carve.link` set.
- **Writes through links.** The scratch worktree's `node_modules` is a symlink into the
  user's worktree. Build info (Vite's template puts `tsBuildInfoFile` under
  `node_modules/.tmp/`) must not be written through it. Never use `tsc -b` against linked
  directories, and redirect any build info under `<git-path carve>`.
- **Dependencies follow the worktree.** If the dirty tree changes `package.json` or a
  lockfile, the linked `node_modules` matches the final tree, not HEAD, so the base check
  can fail or pass for the wrong reason. Say so in the base-failure message and in the
  summary.

### 8.2 Scratch worktree

- Location: `<git-path carve>/scratch`, created with `git worktree add --detach` and **kept
  between runs** so incremental builds stay warm. `git carve --clean` removes it with
  `git worktree remove --force`.
- Materialize a candidate tree with `git -C <scratch> read-tree -u --reset <tree>`. This
  rewrites only the files that changed, so unchanged files keep their mtimes and incremental
  compilers stay fast.
- Dependency links: for each name in `carve.link` (multi-valued; default `node_modules`,
  `.venv`, `venv`) that exists untracked in the main worktree, symlink it into the scratch
  worktree.
- Rust: set `CARGO_TARGET_DIR=<git-path carve>/target` so builds persist and never disturb
  the user's `./target`. The first run is a full build. Say so.
- If the check needs untracked files that exist only in the main worktree (such as `.env`),
  the base precheck fails. The error message must suggest adding them to `carve.link`.

### 8.3 Order of runs

0. **Base first** (DECISIONS.md D43): HEAD's tree. If it fails and `--no-new-errors` is on
   and the output's compiler errors are recognizable (`path(l,c): error TS…`), those errors
   become the baseline: from then on a tree passes when it adds no error beyond them (matched
   as a multiset of file, code, and message). Otherwise exit 3, counting HEAD's errors and
   offering `--no-new-errors`.
1. **Final**: HEAD plus all non-leave atoms. If it fails and the worktree-equivalent tree
   (all atoms) passes, un-leave the leftovers and continue (§6.5). If both fail, D40: leave the groups the check's error locations implicate
   (a few rounds), else search for one group to leave, and commit the rest; exit 3 only if
   nothing passes.
2. **Base**: already checked in step 0.
3. **Probe** (DECISIONS.md D1): the final tree with one changed target-language file
   corrupted by the adapter (`LanguageAdapter::corrupt`). Probe one file per distinct
   nearest-`tsconfig.json` directory, at most 3, and the check must **fail** on each. Cache
   by (check command, link set, blob ids of tracked `tsconfig*.json` and `package.json`).
   If a probe passes: with the default check, exit 3 and name the file the check does not
   read; with an explicit `--check` or `carve.check`, continue, and the summary lists those
   files as unverified.
4. **Prefixes**: the tree after groups 1..k for k = 1..N−1, in increasing k, so an
   incremental checker sees one group added per run (the tree after group N is the final
   tree, already checked).

Cache results by `(tree oid, check command, link set)` in `<git-path carve>/checks.json`.
Identical trees are never checked twice, including across plan edits and re-runs.

### 8.4 Merge-forward

If prefix k fails, merge group k into group k+1. The tree after groups 1..j depends only on
the union of those groups, so a merge removes boundary k and leaves every other prefix tree
byte-identical: merge-forward never creates a tree that needs checking. The loop is
therefore "check each prefix once, then drop every failing boundary" (DECISIONS.md D2).
Check runs are exactly N+1 (base, final, and N−1 prefixes), plus one if the leftovers had
to be un-left (§8.3), plus uncached probe runs. The worst case is a single commit.

Report merges in the summary (`1 merge: groups 2+3 did not pass separately`). The merge
count is also an evaluation metric.

v2 idea (not v1): try reordering before merging.

### 8.5 Building trees

Use a temporary index (`GIT_INDEX_FILE=<git-path carve>/index.tmp`):

1. Run `git read-tree HEAD`.
2. For each group k in order, feed `git update-index --index-info` lines of the form
   `<mode> <oid>\t<path>` for the files touched by group k. The blob is
   `reconstruct(f, atoms of groups ≤ k in f)`. Deletions use mode 0 with an all-zeros oid of
   the repo's hash length.
3. Run `git write-tree` to get `tree_k`.

The temporary index is kept between k steps, so this is incremental.

## 9. Plan buffer

### 9.1 Editor

Resolve the editor with `git var GIT_EDITOR`, which respects git's own precedence. The
buffer lives at `<git-path carve>/PLAN`. It opens **once** for all messages. There is never
one editor per commit.

With `--plan-file <path>`, carve reads an already-filled buffer from that file instead of
opening the editor, and a validation error exits 1 instead of reopening (DECISIONS.md D6).

### 9.2 Format

```
# git carve: 3 commits, 2 hunks left out. Every commit below passed: cargo check
#
# Write a message under each "commit" line (first line = subject).
# Move "@" lines between blocks to regroup. Reorder blocks to reorder commits.
# Delete a "commit" line to merge its block into the block above.
# "@" lines under "leave" stay in your working tree, uncommitted.
# Lines starting with '#' are ignored. Any commit with an empty message aborts everything.
# If you change grouping or order, the check re-runs before anything is committed.

commit


  @ a2  src/net/fetch.rs:41-78       +38 -0   fn retry_with_backoff  [new]
  @ a6  tests/fetch.rs:88-120        +33 -0   fn retries_on_503
  @ a4  src/net/fetch.rs:120         +1 -1    fn fetch_all

commit


  @ a1  src/parse.rs:212             +1 -1    fn parse_header

commit


  @ a3  src/util.rs:10-14            +5 -5    fn format_bytes  [rename]
  @ a5  src/cli.rs:33                +1 -1    fn print_summary
  # … 5 more hunks in 4 files

leave
  @ a9  src/main.rs:30               +1 -0    dbg!(&cfg)            # debug-only line
  @ a7  src/net/fetch.rs:64          +1 -0    println!("here")      # debug-only line
```

Every atom is listed. The header states verdicts only (the check that passed, merged
commits and why, probe results, unverified files), never counts or timings, so the buffer
is byte-deterministic for a given state (DECISIONS.md D19); counts and timings go to the
summary and `--json`. Below the blocks, a scissors line
(`# ------------------------ >8 ------------------------`) starts a reference diff of each
atom, headed `@@ <id>  <path:lines>` (DECISIONS.md D16; at most 40 lines per atom and
`carve.planDiff` lines in total, default 400). Everything below the scissors is ignored. The `# … N more` line is illustrative only; the real buffer lists all
atoms. The label after the atom id (path, line range, counts, enclosing symbol, tags) is
generated by code and **ignored when parsing**.

### 9.3 Grammar

- A block starts with a line that is exactly `commit` or `leave` (after trimming).
- **Atom line**: matches `^\s*@\s+(a\d+)\b`. Only the id is read.
- **Message line**: any other non-comment line inside a `commit` block. Comment lines
  are lines starting with `#` at column 0, whatever `core.commentChar` says (the buffer
  always uses `#`). carve drops them itself, then normalizes whitespace like `git stripspace`
  without `--strip-comments`, so a message line starting with the user's own comment
  character is kept.
- Lines before the first block header must be comments or blank. Anything else is an error.
- Deleting a `commit` header merges its atoms (and any message lines) into the previous
  block.

### 9.4 Validation (on save)

Errors reopen the editor with `# ERROR: …` lines at the top. After 3 attempts, abort.

- Unknown or duplicate atom id: error.
- A missing atom: moved to `leave`, with a warning printed after the run (no data loss:
  leave atoms stay in the worktree).
- Hard binds split across blocks: error ("a3 and a5 must be in the same commit: rename").
- A `commit` block with no atoms: error.
- A `commit` block with an empty message: abort the whole run (exit 1) with no changes,
  like `git commit` with an empty message.
- An empty buffer, or one containing only comments: abort (exit 1).
- Any abort after the editor opened (exit 1, 3, or 4) keeps the typed messages in
  `<git-path carve>/PLAN.saved`. The next run pre-fills a message only for a group whose
  atoms have exactly the same content, and shows the others as `#` comments at the top
  (DECISIONS.md D17).

### 9.5 Re-verification after edits

Compute a structure hash (the ordered list of atom-id sets). If only messages changed, skip
re-checking. Otherwise re-run §8.3 (prefixes only; results are mostly cached).

On failure, do **not** auto-merge the user's explicit choices. Reopen the editor with
`# CHECK FAILED at commit k:` followed by the last 20 lines of check output, and offer the
hint: "delete the next 'commit' line to merge them."

## 10. Committing and undo

### 10.1 Creating commits

For each group in order: `git commit-tree <tree_k> -p <parent> -F <msgfile>`. Add `-S` if
`git config --bool commit.gpgsign` is true. Author and committer come from normal git
config and environment.

Commit hooks are **not** run in v1. The README must say so. The check is the verification
mechanism.

### 10.2 Ref update (compare-and-swap)

- On a branch: `git update-ref -m "carve: N commits" refs/heads/<b> <new> <old>`.
- Detached HEAD: `git update-ref --no-deref -m … HEAD <new> <old>`.
- If the CAS fails because the ref moved, abort. The objects are harmless and dangling
  (gc removes them). Exit 4.

### 10.3 Race guard

Immediately before the ref update, re-fingerprint the worktree (§5.2). If anything changed
since the snapshot, abort with "working tree changed during carve; re-run" and exit 4.

### 10.4 Finalize

1. Run `git read-tree <new HEAD>` so the real index matches the new HEAD. Do not use
   `reset --hard`, and never touch the worktree.
2. Run `git update-index -q --refresh`.
3. Leave atoms now appear as unstaged changes, exactly as before.

Then write `<git-path carve>/undo.json`:

```json
{ "version": 1, "ref": "refs/heads/main", "detached": false,
  "old": "<oid>", "new": "<oid>", "commits": ["<oid>", "..."],
  "left": ["src/main.rs:30", "src/net/fetch.rs:64"], "created_at": "<rfc3339>" }
```

Summary output:

```
Created 3 commits on main, each passed `cargo check` (5 check runs, 0 merges, 38s):
  4f2a1c9 Add retry with backoff to fetch
  9b03e17 Fix off-by-one in parse_header
  c71d2aa Rename fmt_bytes to format_bytes
Left in working tree: 2 hunks (src/main.rs:30, src/net/fetch.rs:64)
Undo: git carve --undo
```

### 10.5 Undo

`git carve --undo`:

1. Read undo.json.
2. Require the ref tip to equal `new`. If the user has committed since, refuse and print the
   manual command `git reset <old>`.
3. CAS the ref back to `old`, then run `git read-tree <old>`.

The working tree is never touched. Because the precondition was an index equal to HEAD, this
restores the exact original state, and it also preserves any edits made after the carve.
The reflog keeps everything regardless.

## 11. CLI

```
git carve [--check <cmd> | --no-check] [--no-new-errors] [--no-judge | --judge] [--plan-only | --plan-file <path>] [--json] [-v]
git carve --undo
git carve --clean
git carve eval build [--sources eval/sources.json] [--out <manifest>] [--train]   (hidden dev subcommands,
git carve eval run [--corpus <manifest>] [--arm code|judge] [--split tune|test]    DECISIONS.md D22–D25)
                   [--check] [--out <results.json>]
```

| Flag | Meaning |
|---|---|
| `--check <cmd>` | Check command (overrides config and default) |
| `--no-check` | Skip verification; summary says "unverified" |
| `--no-new-errors` | When HEAD already fails the check, commit only what adds no new error to HEAD's (DECISIONS.md D43); the buffer and summary say so |
| `--no-judge` / `--judge` | Force code-only or judge mode (default: `carve.judge`, which defaults to `none`) |
| `--plan-only` | Compute and verify, print the plan to stdout, commit nothing |
| `--plan-file <path>` | Read a filled plan buffer from a file instead of opening the editor (§9.1) |
| `--json` | Machine-readable plan and metrics (used by eval) |
| `--undo` | Undo the last carve (§10.5) |
| `--clean` | Remove the scratch worktree, caches, and the build target dir |
| `-v` | Print check commands, timings, and judge decisions with probabilities |

Config keys: `carve.check`, `carve.link` (multi-valued), `carve.judge` (`none|laya`),
`carve.judgeTimeout`, `carve.checkTimeout`, `carve.maxAtoms`.

| Exit code | Meaning |
|---|---|
| 0 | Success |
| 1 | User abort (empty message, empty plan, editor error) |
| 2 | Precondition failed |
| 3 | Check fails on HEAD or on the final tree |
| 4 | Concurrent change (CAS failure or worktree changed) |
| 5 | Internal error (includes a bug-report hint) |
| 129 | Command-line usage error, as with git's own commands (kept apart from 2 so a typo never reads as a refused repository state) |

With `-v`, the judge's probabilities are shown next to each decision. By default no
probabilities appear except the `# debug-only line` tag.

## 12. Milestones and acceptance criteria

Estimates assume focused work. M1–M4 ("Day 1") realistically take 2–3 days.

**M0 Scaffold** (1–2h). Rust workspace, clap CLI, `src/git.rs` wrapper, a temp-repo test
helper, and CI (fmt, clippy, test).
Accept: `git carve --version` works with the binary on PATH.

**M1 Core** (about 1 day). Preconditions, snapshot, atoms, reconstruction, tree building,
commit-tree, CAS ref update, finalize, undo. Test with a trivial grouping (one group per file).
Accept: R1–R5 property tests pass, the safety suite passes, and a sha256 test repo
(`git init --object-format=sha256`) passes.

**M2 Graph** (about 0.5 day). Language adapter, facts, edges, grouping, ordering, import
post-pass, style and debug classification, and the bridging-import diagnostic in `--json`
(DECISIONS.md D5).
Accept, one fixture repo per rule:
- A rename across files becomes one group.
- A new function plus its call site plus its test becomes one group, ordered correctly.
- A drive-by fix in an unrelated function becomes a separate group, even in a file whose
  other edits join by F7 (DECISIONS.md D41).
- A debug print becomes leave.
- A whitespace-only change goes to the trailing style group.

**M3 Verify** (about 0.5 day). Check resolution, scratch worktree, links, result cache,
final/base prechecks, the check probe (§8.3 step 3), merge-forward. A repo whose default
check is vacuous (a Vite-style `"files": []` root config) must be refused by the probe.
Accept: a fixture where the graph mis-splits gets repaired by merge-forward. An automated
test runs `git rebase -x '<check>' <base>` over carve's output, with the check writing a
marker file per run, and asserts that the check ran once per commit and passed.

**M4 Plan and UX** (about 0.5 day). Plan render and parse, editor, `--plan-file`, validation
loop, re-verification, summary, `--plan-only`, `--json`, `scripts/make-demo-repo.sh` (§14.1).
Accept: on the demo repo, carve produces 3 commits with 2 hunks left; `rebase -x` passes;
`--undo` makes `git status --porcelain=v2` and `git diff` byte-identical to before.

**M5 Eval harness and baseline** (about 0.5 day; took a full session). Per EVAL.md and
DECISIONS.md D22–D26: `git carve eval build` clones the sources into
`~/.cache/git-carve/eval` (sources are only read), and writes a manifest of non-overlapping
windows with per-line provenance and debug injections. `git carve eval run` scores an arm:
tier A without the check, tier B (`--check`) with dependencies installed into the cache.
Accept: `eval/results/baseline.json` is written, and the thresholds and decision rules in
EVAL.md are locked and committed by the user.

**M6 Judge** (0.5–1 day once the API docs exist). Tune thresholds on the tune split,
implement `LayaJudge` (DECISIONS.md D20), caching, fail-open, and the privacy notice.
Accept: with an unreachable endpoint, the output is byte-identical to `--no-judge` plus the
notice. The test-split run is recorded once in `eval/results/judge.json`, and the launch
decision follows EVAL.md.

**M7 Launch prep.** README first screen (§3.1–3.2), VHS tape (§14.2), `cargo install
git-carve`, and optional prebuilt binaries.

## 13. Testing strategy

- **Unit tests**: diffing and atoms, reconstruction, each edge rule, plan parsing, and
  stripspace integration.
- **Property tests**: R1–R4 (§5.5).
- **Integration tests**: real temporary repos built with the git CLI, with fixtures named
  after the rule they test.
- **Safety suite** (`safety_*` tests):
  - Worktree bytes are unchanged (hash every file before and after).
  - Undo is exact.
  - A CAS race: a test hook moves the ref mid-run, and carve must abort with no ref change.
  - The worktree-change race guard triggers correctly.
  - Refusal on each §5.1 precondition.
- **Golden plans**: `insta` snapshots of plan buffers for the fixtures.
- **End-to-end**: the demo repo, plus `rebase -x` with a marker-file assertion.
- **Edge-case fixtures**: CRLF files with `core.autocrlf`, a missing final newline, an empty
  file, a binary file, an executable-bit change, a symlink, an untracked large file, a
  rename plus modification, deleting a whole file, a Go unused-import case, and a
  signature change with its callers.

## 14. Demo and launch

### 14.1 Demo repo (`scripts/make-demo-repo.sh <dir>`)

A small project in the target language. The base commit contains `parse_header`,
`fetch_all`, and a helper `fmt_bytes` used in 6 files. The script then applies these dirty
changes:

1. **Feature**: a new `retry_with_backoff`, a call site in `fetch_all`, and a test.
2. **Drive-by fix**: an off-by-one in `parse_header` (an unrelated function).
3. **Rename**: `fmt_bytes` to `format_bytes` across 6 files.
4. **Two debug lines**: in `main` and in `fetch_all`, each separated from other changes by
   at least one unchanged line so they form their own atoms.

With TypeScript as the target, use its naming: `parseHeader`, `fetchAll`, `fmtBytes` →
`formatBytes`, `retryWithBackoff`, and `console.log` for the debug lines.

Expected result: 3 commits and 2 left, all passing the default check. Keep the three
changes independent (the feature must not call the renamed helper) so the demo shows clean
splits.

### 14.2 Tape (VHS, at most about 25 seconds)

1. `git status` shows the mess.
2. `git carve` opens the plan in the editor.
3. Type three one-line messages, then save.
4. The summary appears.
5. `git log --oneline -3`.
6. `git rebase -x 'cargo check' HEAD~3` shows each exec passing.

End card: *"Splits your mess into commits that each pass your check. Refuses to write your
commit messages."*

### 14.3 Launch sequence

- Day 0: Show HN and an X post with the tape, the one-liner, and the contrast sentence.
- Day +1: a design thread (code proves the structure, the judge only labels what's left,
  and the model is not allowed to write), posted **only if** the eval decision allows the judge to
  be mentioned.
- Reply-ready answers: "Why not just ask Claude/an agent?" (proof versus instruction;
  seconds versus minutes; deterministic). "Why no messages?" (your history, your words; the
  tool does the mechanical part).

## 15. Risks and open decisions

| Risk | Mitigation |
|---|---|
| Data loss through reconstruction or ref bugs | The worktree is never written; property tests; CAS; exact undo; reflog |
| Checks too slow to use twice | Compile-only default; a persistent scratch worktree and target dir; a tree-oid result cache |
| Graph too fragmented (many tiny commits) | Plan editing (delete a header to merge); eval metric for group count; judge proposals |
| Graph too coarse (fix merged into feature) | Judge vetoes on soft edges; eval pairwise F1 |
| Name-only resolution collisions | Stoplist; multiply defined names get soft edges only; the check is the backstop |
| Judge weak or unavailable | Code-only is the product; the judge is a measured add-on (EVAL.md) |
| Commit hooks not run | Documented; revisit in v2 |
| Competitor overlap | Positioning on proof and on not writing messages (§3) |
| Code-only mode leaves intentional logging (§6.5 sends *every* debug candidate to leave, but EVAL E5 requires 0 real code wrongly left, and E5 gated only the judge arm) | Resolved in M2 by DECISIONS.md D8 (precision-first leave rule); E5 is reported for both arms (D3) |

**Open decisions for the user:**

1. ~~The target language for v1.~~ Resolved: TypeScript (2026-09-23).
2. ~~Rust as the implementation language.~~ Resolved: Rust (2026-09-23).
3. ~~The Jev API details.~~ Resolved: the judge is Laya over its Jev-compatible `/v1/systemone` protocol (2026-09-24, DECISIONS.md D20).
4. The locked EVAL thresholds and decision rules (before M6).
5. Whether v2 should accept already-staged input.

## 16. Glossary

- **atom**: one zero-context diff region, or a whole-file or mode change.
- **group**: the atoms that become one commit.
- **leave**: atoms left uncommitted.
- **check**: the user's verification command.
- **hard bind**: atoms that must share a commit.
- **order constraint**: one group must not come after another.
- **soft affinity**: a default merge the judge may veto.
- **merge-forward**: fusing a failing group into the next one.
- **judge**: the component that answers questions (Null, Fixture, or Laya).
- **Noul**: a calibrated yes/no probability.
- **Choice**: a distribution over a closed set of options.
