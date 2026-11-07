# git-carve: design decisions

How this file works: every idea is argued for at least two rounds (for, against, synthesis)
before it is adopted. The decision records *why*; `SPEC.md` and `EVAL.md` record *what*.
If they disagree with this file, they win, and this file gets a correction entry.

Earlier decisions: TypeScript as the target language and Rust as the implementation language (SPEC §15).

---

## D1. Prove the check can fail before trusting it (M3)

**Idea.** Before carve trusts a check, it builds the final tree with one changed TypeScript
file deliberately corrupted, runs the check, and requires it to fail.

**Round 1 (for).** The product makes exactly one claim: every commit passes your check. A
check that cannot fail makes that claim false while everything looks green. Examples: a Vite
root config with `"files": []`, a check run in the wrong directory, `--check true`, or a
check that never reads the file you changed. SPEC §8.1.1 lists special cases; this one
mechanism catches all of them, including the ones nobody has thought of yet.

**Round 2 (against).** It adds a check run, which is the slowest thing carve does. It proves
coverage of one file, not all of them. Mutating code at all, even in a scratch tree, feels
like it cuts against the "never touch the user's files" rule.

**Round 3 (synthesis).** Cost: cache the result by (check command, link set, blob ids of
the tracked `tsconfig*.json` and `package.json` files), so it runs once per config change,
not once per carve. Coverage: probe one changed file per distinct nearest-`tsconfig`
directory, at most 3. Multi-project repos are exactly where coverage gaps hide, and files
that share a config share coverage. Safety: the corrupted file is a blob in a temporary
tree, materialized only in the scratch worktree. The user's files are never touched.

**Decision.** Adopt in M3 (SPEC §8.3 step 3).
- With the default check, a probe that passes means exit 3, naming the file the check
  doesn't read.
- With an explicit `--check` or `carve.check`, carve continues, and the summary lists the
  files it could not verify.

## D2. Merge-forward never creates a new tree (M3)

**Idea.** The tree after groups 1..j depends only on the union of those groups. Merging
group k into k+1 removes boundary k and leaves every other boundary's tree byte-identical.
So verification is: check every boundary once, then drop the failing ones.

**Round 1 (for).** Exactly N+1 check runs, not "at most 2N+2" as SPEC §8.4 said; the
"re-check" there is always a cache hit. The runs are also independent, so they could run
in parallel.

**Round 2 (against).** Parallel isn't free. Each concurrent check needs its own scratch
worktree, and `tsc` on a mid-size project takes 0.5–1.5 GB of memory. Incremental checkers
(`tsc --incremental`, `cargo`) are fastest when they walk adjacent trees in one directory,
and parallel runs throw that away.

**Round 3 (synthesis).** Run the checks one at a time, in this order: final tree first (fail
fast if the working tree itself is broken), then base, then prefixes 1..N−1 in increasing
order, so each step adds exactly one group. Redirect build info under `<git-path carve>`.
Add `carve.jobs` only if measured check time (EVAL E6) says we need it. Side benefit: v2's
"reorder before merging" becomes a search over boundary sets, which this framing makes
cheap to reason about.

**Decision.** Rewrite SPEC §8.4 as "drop failing boundaries," with the sequential order
above and no parallelism in v1.

## D3. Fix the evaluation before it is locked (EVAL.md)

**Idea.** Tighten the pre-registered protocol now, while it can still be edited.

**Round 1 (for).**
- **Leakage between tune and test.** The split key hashes `repo_name`. This machine holds
  two pairs of clones of the same repository under different names, so identical windows
  would land in both splits. Key the split on the root commit id and dedupe repos by root
  commit.
- **Noise.** With about 60 test windows, a 0.10 F1 gap on a point estimate can be noise.
  Require a paired bootstrap confidence interval (CI).

**Round 2 (against).** Pairwise F1 grows quadratically with atom count: one 60-atom window
outweighs thirty 5-atom windows, and a commit with a single atom has no positive pairs, so
F1 is undefined there. And rules that are "more rigorous" but unreachable with 60 windows
would kill Jev by construction, which is not a fair test either.

**Round 3 (synthesis).**
- **Primary metric:** BCubed F1 per window, macro-averaged. It is defined for singleton
  commits, weights each window once, and is standard for clustering evaluation. Pairwise F1
  stays as a secondary metric.
- **Headline gate:** Δ ≥ Δ_headline **and** the 95% paired-bootstrap CI lower bound > 0.
- **Optional-flag gate:** CI lower bound > 0 (a point estimate alone is not enough).
- **Corpus:** skip windows that change `package.json` or a lockfile, since base and final
  would need different `node_modules`. Cache `node_modules` by lockfile id, so building the
  corpus takes minutes, not hours.
- **E5 applies to both arms.** Code-only mode wrongly leaving real code out is a launch
  blocker too (the SPEC §15 risk).

**Decision.** Amended EVAL.md as proposed defaults. The user still locks it.

## D4. How judge probabilities become groups (M6)

**Idea (spec).** Veto each soft edge if `p < T_veto`, accept proposals if `p ≥ T_merge`,
then union-find.

**Round 1 (against the spec).** Union-find chains: one false "same change" edge fuses two
whole components. With up to 150 questions, at least one false positive becomes likely.
Tuning three thresholds (`T_veto`, `T_merge`, `T_debug`) on about 26 tune windows also
invites overfitting.

**Round 2 (alternative).** Treat each asked pair as evidence. The score for a component
pair is the sum of `prior(edge kind) + logit(p)` over the pairs asked between them. Greedily
merge the highest-scoring pair while its score exceeds θ. That leaves one grouping
parameter. One confident "no" can outvote several weak "yes"es, and several moderate "yes"es
can outvote one lukewarm "no", which is the behavior we want.

**Round 3 (stress test and synthesis).** Summing logits assumes calibration and independence.
Jev claims calibrated Nouls, so verify it on the tune split:
- Measure expected calibration error (ECE). If it exceeds 0.1, fit Platt scaling (two
  parameters, low overfitting risk).
- Send pairs in canonical order (lower atom id first) for determinism, and probe order
  sensitivity once on tune.
- Priors are *counted* (each code-edge kind's precision on tune), not searched.

Code-only mode keeps the spec's plain union-find, so the baseline stays simple and unchanged.

**Decision.** SPEC §6.4 and §7.4 updated. Two tuned parameters (θ, `T_debug`) instead of
three. The calibration and order probes are pre-registered in EVAL.md.

## D5. TypeScript import lines are the likely #1 cause of false merges (M2)

**Idea.** Two unrelated changes often edit the same import line
(`import { a, retry, clamp } from './util'`). That line is one atom, so it welds the two
groups together. If `clamp` is a new export from the later change, the earlier commit fails
the check, and merge-forward welds them anyway.

**Round 1 (for splitting).** Split import declarations below atom granularity. An
intermediate import line would be the user's final line with some specifiers *deleted*.
carve would only remove specifiers and separators, never add characters. Selecting all
specifiers reproduces the user's exact bytes, so R1 still holds.

**Round 2 (against).** That puts code the user never saw into intermediate commits. With
`prettier --check` in the check, a shortened multi-line import may need re-wrapping and
fail. Comments, `type` modifiers, and default-plus-named imports add many edge cases. This
is scope creep before we know it matters.

**Round 3 (synthesis).** Failure only degrades to today's behavior (the check fails, and
merge-forward merges), so it is safe but not free. Measure first. M2 emits a "bridging
import atoms" diagnostic in `--json`: atoms inside import declarations whose names are
referenced from two or more groups.

**Decision.** Instrument in M2. If bridging import atoms are involved in ≥10% of merges or
false merges on the baseline, implement subsequence splitting before M6.

## D6. Non-interactive plan input, human-first launch (M4)

**Idea.** `--plan-file <path>` reads an already-filled plan buffer (same grammar, §9.3)
instead of opening the editor.

**Round 1 (for).** M4 tests become deterministic without a fake editor. Scripts, CI, and
coding agents can then use carve as a proof engine. Competing agent skills only *promise*
buildable commits, and carve can be the thing that proves it.

**Round 2 (against).** Agents would then write the messages, which blurs "refuses to write
your commit messages" and the spirit of non-negotiable 1. Marketing an agent integration at
launch would muddy the one-liner.

**Round 3 (synthesis).** carve itself still never writes message text. It accepts text
someone else wrote, exactly like `git commit -F`. The flag is cheap and makes testing
easier. The launch copy stays human-first, and any agent integration is a post-launch
experiment, only if the launch lands.

**Decision.** Add `--plan-file` in M4. No agent marketing at launch.

## D7. Parser: tree-sitter, walked directly (M2)

**Idea.** Use `oxc`, a Rust TypeScript parser with semantic analysis, instead of
tree-sitter.

**Round 1 (for oxc).** Exact TypeScript grammar; scope-aware binding resolution for free
(shadowing, locals); pure Rust, no C compiler needed.

**Round 2 (against).** oxc covers JS/TS only, so v2's other languages would need
tree-sitter anyway, meaning two parser stacks. Its 0.x releases break often, and it pulls
in many crates, against "keep dependencies few." Checked: `tree-sitter-typescript` 0.23
has every node kind carve needs (declarations, import clauses, `debugger_statement`,
member calls).

**Round 3 (synthesis).** oxc's extra precision is mostly about local names, which only
produce within-file soft edges. Cross-file precision matters more, and D10 gets it from
import statements, which tree-sitter parses fine.

**Decision.** tree-sitter 0.26 with `tree-sitter-typescript` 0.23. Instead of `.scm`
queries, one Rust walk per file collects definitions (with their scope stack), imports,
references, and debug statements. Revisit oxc only if the eval shows local-name mistakes
causing wrong groups.

## D8. When code-only mode may leave a debug line out (M2)

**Idea (spec §6.5).** In code-only mode, leave *every* debug candidate: an atom that only
adds lines matching debug patterns.

**Round 1 (for the spec).** Simple, and "2 debug lines left unstaged" is the most visible
win in the demo.

**Round 2 (against).** Intentional logging, such as a new `console.log("listening on",
port)`, would silently be left out of history. E5 = 0 is now a gate on both arms (D3), so
the rule has to put precision first. The leave list is shown in the plan, but people skim.

**Round 3 (synthesis).** Code takes the unambiguous part and the judge takes the ambiguous
part. Code-only mode leaves an atom only if all of these hold:
- (a) The atom only adds lines.
- (b) Every non-blank added line is inside a `debugger;` statement or a
  `console.log/debug/trace/dir/table(...)` statement, matched structurally by the parser,
  not by regex.
- (c) Either the atom is `debugger;`, or this file in HEAD never calls that console method.
  A file that already uses `console.log` probably means it.

`console.error`, `warn`, and `info` never count as debug output. A candidate that fails
(c) stays in the nearest group in the same file, tagged `looks like debug output`, so the
user can move it. Jev's `debug_leftover` (M6) can raise recall later.

**Decision.** Implement in M2 and update SPEC §6.5. E5 is measured on the baseline.

## D9. Files with no symbols attach by file stem (M2)

**Idea.** Files without a language adapter (CSS, JSON, test snapshots, Markdown) have no
symbol facts, so in code-only mode each becomes its own commit. Attach them by stem:
`Button.module.css` and `__snapshots__/Button.test.tsx.snap` belong with `Button.tsx`.

**Round 1 (for, as an edge).** It is precise and free: one-component-per-file naming is
near-universal in TypeScript frontends.

**Round 2 (against, as an edge).** If `Button.tsx` holds two unrelated changes, a stem edge
from the CSS atom to both would chain the feature and the fix together. Co-change history
(files that historically change together) would also cover more cases, such as i18n JSON
or global CSS.

**Round 3 (synthesis).** Make it a post-pass, not an edge. A component made only of
symbol-less atoms joins exactly one group: the one holding the most atoms from its stem
siblings, where the same file also counts as a sibling (ties go to the earliest group).
This cannot chain groups. Defer co-change mining: report an `orphan_groups` count in
`--json` and look at the baseline first. If co-change is ever added, mine only history
from before the window base, so the eval cannot leak.

**Decision.** Stem post-pass and orphan diagnostic in M2. Co-change deferred.

## D10. TypeScript cross-file references go through imports (M2)

**Idea (spec §6.1).** Name-only matching: top-level definitions match across files, and
names defined in more than one place in HEAD produce only soft edges.

**Round 1 (against the spec as written).** Knowing whether a name is defined "in more than
one place in HEAD" means parsing every TypeScript file in the repo (thousands in real
projects) or guessing. Pure name matching also links a `format` in `a.ts` with an unrelated
`format` in `b.ts`.

**Round 2 (alternative).** In TypeScript modules, one file can use another file's export
only through an import. So a cross-file reference counts as **strong** only when the
referencing file has an import binding for the name, and the import's source resolves to
the changed file that defines it. Resolution covers relative paths, including the
`.js`-suffix ESM style, and tsconfig `paths` aliases such as `@/*`. Imports of packages or
unchanged files produce nothing. Names that can't be resolved fall back to **weak**
name-only matching: soft evidence only, never an order constraint, with the stoplist and
the rarity cap applied.

**Round 3 (stress test).**
- (a) Ambient or global declarations and script files don't use imports. They are rare in
  modern TS and fall back to weak matching.
- (b) Barrel files (`export * from`) break the path chain. They fall back to weak matching,
  and the eval measures how often.
- (c) The spec's F2 rarity cap (at most 3 atoms) would split a new helper from its fifth
  call site. A symbol that doesn't exist in HEAD can only be used by changes that came with
  it, so **F0**: A adds definition S, B adds a strong reference to S. F0 is a soft
  affinity with no rarity cap.
- (d) An atom that only edits import statements would chain unrelated features through a
  shared import line (D5). It is excluded from unions and placed by the import post-pass.

**Decision.** Update SPEC §6.1–§6.4: import-aware resolution, F0, and import-only atoms
placed by the post-pass. No repo-wide scan.

## D11. Code-only mode stops merging on F2; F2 pairs become judge proposals (M2)

**Data (tune-split windows only, never test).** First measurement on 34 tune windows from
my TypeScript repositories: 3 consecutive commits by one author, at most 15 files,
ground truth from `git blame`. Pairwise F1 is 0.67 (precision 0.66, recall 0.81), and
BCubed F1 is 0.76. Trivial references: everything in one commit scores 0.45 pairwise F1,
every atom alone scores 0.18.

Per-rule edge precision (the share of edges joining atoms of the same real commit):

| Rule | Same commit / edges | Precision |
|---|---|---|
| F0 | 179/201 | 0.89 |
| F1 | 3737/3924 | 0.95 |
| F2 | 252/372 | 0.68 |
| F3 | 49/61 | 0.80 |

Edges are mostly precise, yet pairwise precision is 0.66. That is **chaining**: union-find
takes the transitive closure, so one wrong edge between two clusters of 30 atoms creates
900 wrong pairs.

**Idea considered and rejected first.** Group code-only mode by summed evidence (as in D4)
with a negative base-rate prior, so a single weak edge cannot fuse clusters. The counted
base rate killed it: any two atoms in a window share a commit 49% of the time (big commits
dominate), so the prior is about 0 and changes nothing. Recorded so nobody retries it
without new data.

**Ablation.** Pairwise F1 and BCubed F1 with one rule removed at a time:

| Removed | Precision | Recall | Pairwise F1 | BCubed F1 |
|---|---|---|---|---|
| none | 0.66 | 0.81 | 0.67 | 0.76 |
| F2 | 0.80 | 0.68 | 0.68 | 0.77 |
| F1 | 0.66 | 0.63 | 0.57 | 0.69 |
| F3 | 0.66 | 0.80 | 0.66 | 0.75 |
| F0 | 0.66 | 0.76 | 0.63 | 0.74 |

**Round 1 (for dropping F2 as a merge rule).** Its precision is the weakest, and removing it
buys +0.14 precision for −0.13 recall, with F1 unchanged or slightly better.

**Round 2 (against).** A 0.13 recall loss means more small commits for the user to merge,
which could worsen E7 on real trees. And 34 windows from 5 repositories, clustered by repo,
is a small sample: an F1 difference of 0.01 is noise.

**Round 3 (synthesis).** Because F1 is flat, the decision rests on the **cost asymmetry**,
which is not a statistical claim:
- Merging in the plan buffer is one keystroke (delete a `commit` line).
- The check merges automatically when a split breaks the build (merge-forward).
- Splitting a fused group means moving atom lines by hand.

F2's information is not thrown away. F2 pairs are exactly the "maybe related" pairs the
judge is for, so they become `same_change` proposal candidates in M6. Re-validate on the
full tune split at M5 before the baseline is frozen: restore F2 merging if it improves
tune F1 by more than 0.02.

**Decision.** In code-only mode, F2 edges are recorded (in `--json` `edges` and
`soft_edges_by_kind`) but do not merge. SPEC §6.3 and §7.3 updated. The ablation knob
`GIT_CARVE_DEV_DISABLE_EDGES` stays for the M5 harness.

## D12. The default TypeScript check, decided by measurement (M3)

**Measured on the real `create-vite` react-ts template and the demo repo (2026-09-23):**

| Question | Result |
|---|---|
| `tsc --noEmit -p .` on the Vite layout (`"files": []` + references), with a type error in `src/` | **exit 0: a vacuous pass** |
| `tsc --noEmit -p tsconfig.app.json` (each referenced project) | catches the error |
| `tsc -b` (the template's own build script) | writes `node_modules/.tmp/*.tsbuildinfo`, which in the scratch worktree is **through the symlink into the user's files** |
| `tsc -p` with `--incremental --tsBuildInfoFile ../x.tsbuildinfo` | writes only `../x.tsbuildinfo`; correct after an error is introduced and reverted |
| `npx --no-install tsc --version` vs `node_modules/.bin/tsc --version` | 643 ms vs 159 ms per run |
| Plain vs incremental `tsc` on the demo | about 930 ms vs 830 ms (startup-bound; larger projects gain more) |

**Round 1 (the spec's default).** `npx --no-install tsc --noEmit -p .`: one short,
familiar command.

**Round 2 (against, from the table).** It passes vacuously on the most common modern
template, which is a false proof. `npx` adds about 0.5 s to each of the N+2 runs.

**Round 3 (synthesis).** carve builds the command from the repository:
- Check each project: the root if it has inputs, plus every `references` entry.
- Call `node_modules/.bin/tsc` directly when it exists; otherwise fall back to `npx`.
- Use `--incremental` with build info redirected to `<git-path carve>/tsbuildinfo/`.
- Never `-b`.
- If there is no root `tsconfig.json`, refuse, list the tracked `tsconfig*.json` paths, and
  suggest `git config carve.check`.

The summary shows a short form (`tsc --noEmit -p .`), and `-v` shows the full command. D1's
probe backstops any layout this logic misses.

**Decision.** Implement in the TypeScript adapter's `default_check`.

## D13. A cached pass must not outlive the toolchain that produced it (M3)

**Idea (spec §8.3).** Cache check results by (tree id, check command, link set).

**Round 1 (for the spec).** Identical trees are never checked twice, including across plan
edits and re-runs.

**Round 2 (against).** The link set is only *names*. After `npm update` or a TypeScript
upgrade, `node_modules` is different, but the key is the same. carve would then report
"passed" from a result the current toolchain never produced. That is a false proof, the
one failure the product cannot afford.

**Round 3 (synthesis).** Add a toolchain fingerprint to the key. For each linked path:
its resolved target, the directory's modification time, and the modification times of the
package managers' install markers (`.package-lock.json`, `.modules.yaml`,
`.yarn-state.yml`, `.yarn-integrity`) inside it. Also add the carve version, since default
flags change between versions. False cache misses cost only time; false hits cost
correctness, so this asymmetry justifies a coarse, conservative fingerprint.
`git carve --clean` drops the cache.

**Decision.** Implement in the check runner.

## D14. Never make a commit that breaks a file's syntax, even before running the check (M3)

**Idea.** For each prefix tree, re-parse the TypeScript files it partially applies. If a
file that parses cleanly in both HEAD and the working tree does not parse in the prefix,
drop that boundary at once, without running the check.

**Round 1 (for).** It saves a check run for a failure that is certain: `tsc` rejects syntax
errors. And it gives a precise reason ("commit 2 would split a statement in `src/a.ts`")
instead of 40 lines of compiler output.

**Round 2 (against).** It duplicates the check's job. tree-sitter can be wrong (the JSX `&`
case, logged in M2). A file outside the check's scope would now force a merge the check
wouldn't have.

**Round 3 (synthesis).** Only files that parse cleanly in *both* HEAD and the working tree
qualify, so known grammar gaps do not trigger it. The rule can only merge, never pass
anything, so a false positive costs a coarser commit, not a false proof. Forcing a merge
for a file the check doesn't read is a feature: a commit containing a half-statement is
broken whether or not the check looks. This adds a guarantee: no commit carve makes breaks
a file's syntax.

**Decision.** Implement in verification. Report as `dropped: syntax (path)`.

## D15. Scratch worktree: registered worktree as specced; the non-negotiable wording conflicts

**Conflict found.** SPEC §8.2 says `git worktree add --detach`, which writes
`.git/worktrees/<name>/`. Product rule 2 lists what carve may write: objects,
refs, the index, and files under `<git-path carve>`. `.git/worktrees/` is not on that list.

**Round 1 (plain directory).** A plain directory under `<git-path carve>` with its own
temporary index (`git --work-tree=... read-tree -u`) satisfies the list literally. It also
keeps `git worktree list` clean and leaves nothing stale if `.git/carve` is deleted.

**Round 2 (registered worktree).** Checks that call git (a version script running
`git describe`, husky, `vitest --changed`) need a real worktree. A directory inside `.git/`
with no `.git` file confuses git's discovery. And the spec wins on conflicts.

**Round 3 (synthesis).** The rule's title, "never write to the user's working-tree files," is
about the user's files. `.git/worktrees/` holds git metadata, not user files, so the
enumerated list is narrower than the rule's intent. Follow the spec: a registered,
detached worktree created with `--no-checkout`, filled by `read-tree -u --reset`. carve
prunes stale registrations, and `--clean` removes it. Point out the wording conflict and
propose adding "the scratch worktree's registration" to the list.

**Decision.** Registered worktree. Non-negotiable 2 was amended to list the registration.

## D16. Show each atom's diff in the plan buffer, below a scissors line (M4)

**Idea.** The plan buffer lists atoms as `path:lines +a -r symbol`. That is enough to
regroup, but not to write a good message: people need to see the change.

**Round 1 (for: diff under each atom).** The change sits right under its atom line, and
messages get written without switching windows.

**Round 2 (against).**
- Buffers become hundreds of lines on a big carve.
- Moving an atom line means also moving its diff, or the diff misleads.
- The SPEC §9.2 format is a user interface with snapshot tests; interleaving breaks its
  scannability.

**Round 3 (synthesis).** Do what `git commit -v` does, which users already know. Atom lines
stay compact and movable above a scissors line
(`# ------------------------ >8 ------------------------`). Below it comes the diff of each
atom, headed by its id, for reference. Everything below the scissors is ignored when
parsing. Caps: 40 lines per atom, 400 in total (`carve.planDiff`, 0 disables). `--plan-only`
prints the buffer without the diff unless `-v`.

**Decision.** Implement in M4; SPEC §9.2 notes the scissors section.

## D17. Never lose the messages a user typed (M4)

**Idea.** Messages are the only thing the user types into carve, all at once, in one buffer.
If the run then aborts (the working tree changed while the editor was open: exit 4; a
regrouping fails the check: exit 3; three invalid saves: exit 1), that work is lost.

**Round 1 (for).** Save the last buffer on any abort after the editor opened, and restore
it on the next run.

**Round 2 (against).** Atom ids change when the tree changes, so mapping old messages to
new groups is ambiguous. A stale message silently attached to a different commit is worse
than a lost one.

**Round 3 (synthesis).** Match by content, never by id:
- Each group gets a fingerprint: the set of its atoms' content fingerprints (path, kind,
  changed bytes).
- On the next run, a new group with exactly the same fingerprint gets its message
  pre-filled.
- Every other saved message is shown as `#` comments at the top ("from your last attempt,
  not applied"), for copy-paste.

Nothing is ever attached to a group it wasn't written for. A successful commit deletes the
saved buffer.

**Decision.** Implement in M4 (`<git-path carve>/PLAN.saved`).

## D18. Dogfood: carve commits its own development from M4 on

**Idea.** Commit carve's own work with `git carve`: this repository, with
`carve.check = cargo check --all-targets --quiet`.

**Round 1 (for).** Real use finds what fixtures miss, and M2's first real-history run
already found three bugs. "carve's history was carved by carve" is also a good launch
detail.

**Round 2 (against).** carve's own repo is Rust, and there is no Rust adapter, so every
file is symbol-less and grouping degrades to one commit per file, joined only where the
check fails. That doesn't exercise the TypeScript graph. A carve bug could also mangle the
tool's own history.

**Round 3 (synthesis).** The Rust gap makes it a harsh test of the parts that are
language-independent: the plan buffer, `--plan-file`, verification with a real compiler
through merge-forward, commit, CAS, and undo. That is exactly M4's surface. The history
risk is covered by undo and the reflog. Developer messages go through `--plan-file`: carve
writes none, and the developer writes them. If it fails, fall back to plain
git and log the bug as a finding.

**Decision.** From the end of M4, commit through `git carve`.

## D19. The plan buffer is byte-deterministic; metrics go to the summary

**Found.** The M3 preview header included run counts, cache hits, and seconds. A second run
on the same state printed a different header, which breaks non-negotiable 7 (same state +
same caches → the same plan, byte for byte) for the thing users read and edit.

**Round 1.** Keep the metrics in the buffer: users like to see that the checks ran.

**Round 2.** Timings are never deterministic, and cache-dependent counts change from run to
run. The insta snapshot tests the spec requires would be flaky.

**Round 3 (synthesis).** The buffer states the verdicts, which are deterministic for a
given state: which check passed, which commits were merged and why, and probe results.
Counts and timings go to the final summary line and to `--json`.

**Decision.** Fixed in M4.

## D18 correction (2026-09-23): no commits in this repository yet

I decided that nothing is committed in this repository until I say so (at the end of the
project, or on request). A commit made earlier was removed: the branch ref was deleted, the
index cleared, and the working tree left byte-identical.

Dogfooding therefore changes shape. carve's own work stays uncommitted, and dogfood runs
that need commits happen only in throwaway copies under `/tmp`: the real-history windows
from my TypeScript repos (M5 harness), and scratch clones like `scripts/try.sh`. The
goal is the same (real use, not fixtures) without touching my history.

## D20. The judge is Laya, reached over its Jev-compatible HTTP protocol (M6)

**Context (decided 2026-09-24).** The judge changes from TypeSafe Jev (a closed,
hosted API) to **Laya** (github.com/NandhaKishorM/laya, Apache-2.0).

Research findings:
- Laya answers exactly the judge's question types (`noul`, `choice`, plus `score`), with no
  text generation.
- `laya.serve` speaks the same `POST /v1/systemone` wire protocol as Jev, with
  schema-identical answers. The API that non-negotiable 8 said not to invent is now open
  source and self-hostable, which unblocks M6's integration work.
- GitHub holds the code (Python package, HTTP/MCP servers, a TypeScript/ONNX port, docs,
  benchmarks, the fine-tuning notebook). Hugging Face holds the weights: `convaiinnovations/laya`
  (2.4 GB, three safetensors checkpoints) plus single-checkpoint mirrors. The code downloads
  the weights on first use.
- Pinned revisions: laya `55cf4c4`, laya-multilingual `e4e9ddf`, laya-typed-decisions
  `1a793eb`, package 0.3.20. The weights changed on 2026-09-24 itself, so pinning is
  required for non-negotiable 7.

**Round 1 (options).**
- (a) HTTP to a running `laya-serve`.
- (b) Spawn Python per carve run.
- (c) In-process ONNX Runtime (Rust `ort` crate) on the split `encoder.onnx` + `head.onnx`
  export that `laya-ts` already uses.

**Round 2 (against each).**
- (b) is out: a cold checkpoint load takes 7–10 s (measured by Laya), longer than the judge's
  whole 3 s deadline.
- (a) makes the user run a Python server with torch (a multi-GB install), with one HTTP
  request per state.
- (c) avoids Python at run time, but it adds ONNX Runtime and a tokenizer crate to a CLI that
  keeps dependencies few. No ONNX exports are published, and carve would have to
  re-implement Laya's prompt formatting exactly, so behavior could drift.

**Round 3 (synthesis).** The judge is optional and off by default (`carve.judge = none`); the
people who enable it can run a server. (a) uses Laya's own maintained code, so behavior is
exact. It keeps carve a single dependency-light binary, and it fails open by construction
(server unreachable → code-only + one-line notice, non-negotiable 5).

Latency is handled by asking few questions and sending them concurrently:
- D11 already sends only F2 pairs and non-leave debug candidates to the judge.
- `laya-serve` has an inference pool, so carve sends up to 4 requests at once.
- Measured by Laya: 33 ms/request on GPU, 0.2–0.5 s on CPU.

(c) is reserved for after M5/M6: only if the eval earns the judge a place in the launch copy,
publish the ONNX export (Apache-2.0 allows it) and add an optional `ort` feature.

**Decision.** `LayaJudge` implements the `Judge` trait over `/v1/systemone` (`LAYA_URL`,
default `http://127.0.0.1:8000`; optional bearer token). The cache key uses
`judge.name() = "laya:<checkpoint>@<hf-revision>"`. The eval arm formerly called "jev" becomes
"judge (Laya)". Whether Laya is used zero-shot or fine-tuned is D21, decided on data.

## D21. Zero-shot Laya has no signal on carve's question; a trained judge must earn its place

**Measured (2026-09-24, tune split only).** 836 atom pairs from 60 tune windows of the user's
5 TypeScript repos, labeled same/different commit by `git blame`. 246 of them are F2-only
pairs, the ones D11 routes to the judge. The question is SPEC §7.3's `same_change` over the
§7.3 state (both diffs, paths, enclosing symbols, line distance). Raw output:
`eval/dev/results/judge_probe_zero_shot.txt`.

| Checkpoint | Wording | AUROC all | AUROC F2 | P(same) same-commit | P(same) different-commit | s/pair |
|---|---|---|---|---|---|---|
| english | plain | 0.532 | 0.492 | 0.793 | 0.766 | 0.040 |
| english | A/B labels (#156) | 0.546 | 0.479 | 0.575 | 0.545 | 0.039 |
| typed-decisions | plain | 0.514 | 0.470 | 0.570 | 0.567 | 0.071 |
| multilingual | plain | 0.513 | 0.501 | 0.898 | 0.886 | 0.034 |

The code-only verdict on the same pairs has accuracy 0.701 overall and 0.545 on the F2-only
pairs, where the base rate of "same" is 0.42. The F2 pairs are exactly what code cannot
decide, and zero-shot Laya cannot either: every variant is at chance there. This matches
Laya's own README ("base checkpoints near chance zero-shot; treat Laya as a fast base to
specialise"). Speed is not the problem: 19–71 ms per pair on the RTX 3070.

**Round 1 (fine-tune Laya).** carve has something most classifiers lack: unlimited free
labels. Any TypeScript repository's history yields atom pairs labeled same/different commit
by the same `git blame` machinery EVAL uses. The Laya notebook fine-tunes on about 30k
decisions.

**Round 2 (against).**
- The labels are noisy: humans tangle commits (the EVAL threat to validity).
- A model trained on commit habits may not transfer to other developers.
- A full fine-tune of the 421M checkpoint probably does not fit in 8 GB.
- Worst of all, training on my repos would contaminate the pre-registered eval.
- And a tiny model over carve's own features might capture most of the learnable signal,
  with no server at all.

**Round 3 (synthesis): a ladder, cheapest rung first, on data that cannot leak.**
0. **Training data:** mined from open-source TypeScript repositories **disjoint from the EVAL
   corpus** (checked by root commit), never from my repos or any eval window.
1. **Features only:** logistic regression over carve's own pair features (edge kinds, same
   file, line distance, enclosing, path similarity, shared new symbols). If this wins, it
   ships inside carve in Rust: deterministic, no server, "code before model."
2. **Frozen Laya encoder + small head:** embeddings of the two diffs, a trained head
   (fits 8 GB easily; the approach of the community `stuntd` tool).
3. **Full Laya fine-tune** (Kaggle's free 2×T4 notebook, or a low-memory setup on the 3070),
   served by `laya-serve` through the `LayaJudge` of D20.

Climb a rung only if it beats the one below by a clear margin on held-out training repos.
Whatever is frozen when EVAL.md is locked is the judge arm, and the pre-registered rules
decide whether it ships.

**Decision.** Zero-shot Laya is not used. D20's `LayaJudge` integration stands (it serves
rung 3). EVAL.md now requires any trained judge to be trained only on disjoint repositories
and frozen before the test run. Mining reuses the M5 corpus builder, so M5 comes first.

## D22. One data engine for evaluation, training data, and regression (M5)

**Idea.** Build SPEC §11's `git carve eval` as the single place that turns git history into
labeled data. It serves the pre-registered eval corpus, D21's training pairs, and a
repeatable quality benchmark for every heuristic change.

**Round 1 (for, in Rust, in the binary).** It runs carve's own library on each window (no
process per window, no drift between the eval and the product), reuses the repo test
helpers, and is deterministic. Labeling logic (`git blame`, provenance, injection) lives in
one place, so eval labels and training labels cannot disagree.

**Round 2 (against).** Python iterates faster for research, statistics are more code in Rust,
and the product binary grows.

**Round 3 (synthesis).** The math is small (BCubed, pairwise, a seeded bootstrap), and
`eval/dev/` keeps Python for exploration. The decisive argument is **leakage by
construction**. Every source in `eval/sources.json` has a role (`eval-user`, `eval-oss`,
`train`). The builder refuses a `train` source whose root commit matches any eval source,
and it clones every source into a cache, so my repositories are only ever read.

**Decision.** `git carve eval build | run` in Rust (a hidden subcommand), with per-line
provenance computed once at build time and mapped onto atoms at run time.

## D23. Two tiers: grouping metrics on every window, verified metrics where installs work (M5)

**Idea (EVAL as written).** Every window must pass the check on its base, which means
installing each historical lockfile's dependencies.

**Round 1 (for).** Only windows that build are realistic.

**Round 2 (against).** The cost is heavy:
- Next.js `node_modules` are 300–500 MB per lockfile version, and history has many.
- Old lockfiles often no longer install (registry rot), which biases the corpus toward
  recent history.
- It takes hours of network time.

And the headline comparison (judge vs code, E1) is a comparison of *graph plans*: both arms
are measured before verification, so the check does not change which arm wins.

**Round 3 (synthesis).**
- **Tier A** (all sampled windows, `--no-check` plans): E1, E3, E4, E5, E6. This is exactly
  what the judge changes.
- **Tier B** (windows whose base passes the check after a cached `npm ci --ignore-scripts`,
  keyed by lockfile id, at most 3 lockfile versions per repository): E2 and a secondary
  "E1 verified" that includes merge-forward, the product as users get it. Tier B is reported
  with its own sample size.

**Decision.** EVAL.md §1 amended before locking.

## D24. Windows do not overlap (M5)

**Idea.** EVAL says "windows of 2–4 consecutive commits" without saying whether windows may
share commits. The dev previews used every start index.

**Round 1.** Overlapping windows give more data.

**Round 2.** Overlapping windows share commits, so they are correlated. The sample size
looks larger than it is, the bootstrap CI shrinks falsely, and a judge "win" could look
significant when it is not. That is exactly the failure pre-registration exists to prevent.

**Round 3 (synthesis).** Fewer windows is the honest price, and EVAL already treats fewer
than 60 test windows as provisional.
- **Deterministic sampling:** walk first-parent history. At position i, the window size is
  `2 + (sha256(root_commit + commit_i) mod 3)`. An accepted window advances i past itself; a
  rejected one advances i by 1.
- **Windows must be parent chains** (a merge breaks a window) by one author.

**Decision.** EVAL.md §1 amended.

## D25. The code arm is frozen by a source hash at baseline time (M5)

**Idea.** The judge is measured against the code arm, so the code arm must hold still.

**Round 1.** Freeze the code heuristics once the baseline is recorded.

**Round 2.** That blocks legitimate code-only improvements before launch.

**Round 3 (synthesis).** Record a SHA-256 of `src/` and `Cargo.lock` in `baseline.json`. The
harness warns when the running source differs. A heuristic change that alters code-arm
plans requires re-running the code arm on tune *and* test, logged in EVAL.md's change log.
Heuristics are developed on tune windows only (D11 practice), so the test split is never
used to choose them.

**Decision.** Implement in `eval run`.

## D26. F6 systematic edits; D11 reversed by its own rule (M5, tune split only)

**Finding.** On the M5 tune split (66 windows; hono 41, zustand 14, my repos 11),
code-only mode over-splits badly. Pairwise recall is 0.24, and the plan has about 6 more
groups than true commits. The worst tune window, one hono feature commit touching 14 files,
became 52 groups. Its edits change type parameters without adding or removing names, and
half of them are byte-identical copies in a generated mirror (`deno_dist/`).

**Idea (F6).** Atoms making *the same change* link:
- identical removed→added text (at least 12 non-space characters): generated mirrors,
  copy-pasted fixes;
- the same small token substitution (1–6 words removed, 1–6 added, one of them a
  non-numeric word of 3+ characters): renames, codemods, API migrations.

**Round 1 (for).** It is a code-only signal (no model), cheap, and language-independent, so
it also applies to symbol-less files. Systematic edits are known to be commit-coherent.

**Round 2 (against).** The token signature might collide on common substitutions (for
example `true`→`false` in two unrelated fixes), and it might be tuned to hono's mirror
directory.

**Round 3 (measured).** Ablation on the tune split:

| Variant | BCubed F1 (95% CI) | Pairwise P | R | F1 |
|---|---|---|---|---|
| F2 not merging, no F6 | 0.686 [0.63, 0.74] | 0.880 | 0.242 | 0.380 |
| + F6 | 0.745 [0.70, 0.79] | **0.898** | 0.332 | 0.485 |
| + F2 merging | 0.737 [0.68, 0.79] | 0.721 | 0.332 | 0.454 |
| + F6 + F2 merging | **0.769** [0.72, 0.82] | 0.743 | 0.386 | **0.508** |

- **Collisions:** F6 *raises* precision (0.880 → 0.898), so they are rare in practice. The
  substitution key also excludes numbers and requires a 3+ character word.
- **Hono overfit:** F6 helps hono (+0.03) and zustand (+0.06). My repos are unchanged
  (n = 11, too few to move).

**D11 re-validation.** D11 promised to restore F2 merging if it improves tune F1 by more than
0.02. On top of F6 it adds +0.024 BCubed and +0.023 pairwise F1: over the bar, barely, with
overlapping intervals and a precision cost (0.898 → 0.743). The rule was fixed in advance
precisely so that preference would not decide.

**Decision.**
- F6 adopted.
- **D11 reversed:** F2 merges again in code-only mode.
- F2 pairs keep their role as the judge's first veto candidates (D4's original design).
- The code arm is frozen here for the baseline (D25).

## D27. The default check covers the projects that own the changed files (proposed; M5 finding)

**Finding (tier B, test split).** The D1 probe refused 13 windows, 10 of them in my
own `private-repo-A` repo. Its root `tsconfig.json` includes only `src*`, so edits under
`frontend/` (a project with its own tsconfig) and `scripts/` (no project at all) are not
read by `tsc -p .`. D1 then exits 3 for *any* carve touching those directories: carve would
be unusable on my own repository.

**Round 1 (keep D1 strict).** Every commit carve makes must pass a check that actually reads
it. A check that can't see a file proves nothing about it.

**Round 2 (against).** Refusing is the wrong remedy when carve *could* build a check that
reads the file. `frontend/` has its own `tsconfig.json`, so checking that project too is
exactly what my own workflow does (`cd frontend && tsc`). And `scripts/` is type-checked by
nothing in my own setup, so demanding proof for it is stricter than I am.

**Round 3 (synthesis).**
- The default check becomes: the root project (if it has inputs) plus every project that owns
  a changed TypeScript file (its nearest `tsconfig.json`), plus D12's references.
- A changed file owned by no project is **reported as unverified** in the plan header and
  summary, not refused.
- The probe keeps its teeth: if a file *is* owned by a project and the check still passes on
  the broken file, that is a vacuous check, and D1's exit 3 stands.

**Decision.** Adopt after the tier B run completes (source changes mid-run would break D25's
hash). This changes tier B only, not the tier A code-arm baseline. The code arm is re-run on
both splits and the change is logged in EVAL.md, as D25 requires.

**D27 status (2026-09-24): implemented.**
- The default check covers the root project, its references, and every project owning a
  changed file. Sub-project `node_modules` are linked into the scratch worktree.
- Probes are grouped by config directory and first folder below it (at most 4), since
  `include` can leave one folder out.
- Refusal happens only when no probed file is read; otherwise unread files are listed as
  unverified.
- Tier A code-arm metrics were identical after the change (re-run on both splits per D25;
  source hash d2f6a14a4a8c → 56f76184e62a).
- On tier B, my `private-repo-A` windows are no longer refused by the probe. They now stop
  at the check, because tier B does not install sub-project lockfiles; on my machine
  those dependencies exist.

## D28. The judge, rung 1: a learned merge model over carve's own signals (M6)

**Idea.** Replace hand-set merge rules in judge mode with a small model that predicts
P(same commit) for two groups of atoms, trained on history where the answer is known.

**Round 1 (for).** M5 shows the hand rules leave recall on the table: test recall 0.60,
about 2 extra commits per window, and edits that change types without changing names link
nothing. The signals exist but are weighed by hand or ignored:
- edge kinds F0–F6 (counts, not just "any");
- same file and line gap; shared directory depth;
- test↔implementation stems;
- identifiers *mentioned* on changed lines (not only net-added);
- sizes, new files.

A model learns their weights from thousands of labeled pairs, with labels free from
`git blame`. And it is still "code before model": the inputs are code facts, and the output
is a calibrated probability, exactly the `Judge` contract.

**Round 2 (against).**
- A learned model is less explainable than rules.
- It can overfit the training repos' commit habits.
- Its features shift once groups merge (a merged group has more edges than the component pairs
  it was trained on).
- And the learned model might just rediscover union-find.

**Round 3 (synthesis).**
- **Deliberately tiny:** logistic regression, about 20 features, weights in a reviewed JSON
  file. Explainable as a table and deterministic.
- **No leakage:** trained only on the `train` sources (ky, zod, jotai, swr, tanstack-query;
  leak-guarded).
- **Validated leave-one-repository-out,** so the score measures transfer to an unseen
  developer, the overfit risk itself.
- **Scale-free features:** fractions, a log line gap, and indicators instead of raw counts,
  which shrinks the merge-time shift. Merging is D4's evidence agglomeration: the most likely
  pair first, until the probability falls below θ.
- **Kill criterion:** if leave-one-repo-out AUROC does not clearly beat the code baseline's
  edge signal on the same pairs, stop at this rung and write it down.

**Decision.** Build it: `git carve eval pairs` (features and labels), a training script, then
a Rust `LearnedJudge` if the numbers earn it.

## D29. Learn each repository's commit granularity from its own history (candidate)

**Idea.** Developers differ: some commit every function, some every feature. carve could read
the current repo's recent history (local, deterministic for a given HEAD) and shift θ
toward that repo's habits.

**Round 1.** It's free data, it's private (it never leaves the machine), and it targets
exactly the E3 group-count error.

**Round 2.** It costs `git blame` over recent windows on each run (it would need a cache keyed
by HEAD), young repos have little history, and a messy tree may not match a developer's
habits. Behavior that varies per repository is also harder to explain.

**Round 3.** Only worth it if the best θ really differs by repository. That is measurable
once rung 1 exists: tune θ per source on the tune split and compare with the global θ.
**Decision:** measure after D28; build only on a clear per-repo gap.

**Measured (2026-09-24, D31).** On the tune split the best θ per source ranges from 0.64 to
0.70, against a global 0.70. The largest per-source gain would be about 0.03, on a source with
2 windows. There is no clear gap, so D29 is **not built**.

## D30. Mark uncertain boundaries in the plan buffer (candidate)

**Idea.** With calibrated probabilities, carve knows which of its splits and merges are
coin flips. A `#` comment on the one to three least-certain boundaries (for example
"# carve is unsure a4 belongs with this commit") tells the user where to look.

**Round 1.** Regrouping is the user's main edit, and pointing at the likely mistakes should
cut EVAL's E7 `cuts_to_change`.

**Round 2.** It is noise when the model is wrong; the buffer format is a snapshot-tested user
interface; and it drifts toward carve "suggesting" things.

**Round 3.** Code-derived `#` comments are explicitly allowed (non-negotiable 1 forbids
message text, not labels). Cap them at 3, and show them only when the probability is within
0.1 of θ.

**Decision:** after D28, and only if the model's calibration on tune is good (ECE ≤ 0.1).

**Built (2026-09-24).** The condition is met: ECE is 0.088 after Platt scaling.
- **Unsure joins:** judge merges made with p in [θ, θ + 0.1). The note names the smaller
  side's atoms and the top features.
- **Near misses:** the first merge *not* made, if its p is in [θ − 0.1, θ). Both commits get
  a note.
- **Placement:** at most 3 notes, closest to θ first, as `#` lines under the commit's atoms.
  The parser ignores them, and a test proves the buffer parses back to the same plan.
- **Where they appear:** judge mode only, so the code-only buffer and its snapshots are
  unchanged. The blinded E7 renders turn them off, because notes that only the judge arm
  has would reveal it.
- **Cost:** computing them extends the merge sequence to θ − 0.1. The applied prefix is
  identical, so groupings do not change.
- **On the demo** they flag exactly the D37 merge:
  `# unsure: a8 joined this commit at 0.70 (threshold 0.70), on same_dir, same_ext`.

## D31. How the learned judge scores two clusters: pooled linkage (M6, tune split)

**Idea.** Merging needs a cluster-to-cluster score, but the model was trained on pairs of
hard-bound components. Two ways to score a pair of clusters:
- **Pooled:** ask the model about the two merged clusters directly, as one feature row.
- **Average:** ask only about component pairs, the unit it was trained on, and average them
  across the two clusters.

**Round 1 (pooled).** It is simplest and uses all of the evidence at once. A cluster that has
grown a test file now links to the implementation's other atoms.

**Round 2 (against pooled).** Most features are "any" indicators: any F-edge, same file
anywhere, any shared name. A merged cluster accumulates hits, so its probabilities inflate
as it grows, and that causes chaining: D28's round-2 worry, made concrete. Average linkage
stays in the training distribution and resists chaining, at the cost of one k×k matrix.

**Round 3 (measure; ties go to average).** Both were built and swept over θ on the tune split
(66 windows; code arm 0.769 BCubed):

| linkage | best θ (raw) | tune BCubed | θ range scoring ≥ 0.83 |
|---|---|---|---|
| average | 0.45–0.50 | 0.837 | 0.45–0.50 |
| pooled | 0.60 | **0.856** | 0.50–0.70 |

- Pooled wins and is less sensitive to θ. The inflation is real, but a higher θ absorbs it,
  because pooled's *ranking* of pairs is good.
- With the step-3 Platt scaling (ECE 0.137 → 0.088; a = 0.821601, b = 0.480245), pooled's
  decisions map one-to-one onto the raw θ. A finer sweep peaks at θ = 0.70 (tune BCubed 0.856;
  0.843 or better from 0.64 to 0.74).
- Guards on tune: E4 precision 1.00, E5 0.
- Tier B on tune: E2 0.0 merge-forwards per window vs the code arm's 1.8 (5 windows).

**Decision.** Pooled linkage, θ = 0.70 after Platt scaling. Recorded in
`eval/results/thresholds.json`, and frozen in `eval/LOCK` with the weights' SHA-256 before
step 4.

**Also measured (D29).** The best θ per source falls between 0.64 and 0.70. A per-repository θ
would gain at most about 0.03, and only on sources with 2–4 tune windows. D29 is **not built**.

## D32. The verdict is computed, and the freeze is enforced (M6)

**Idea.** EVAL.md §4 step 6 says to apply the decision rules "mechanically". Make that literal:
`git carve eval decide` computes the verdict, and `git carve eval run --arm judge` refuses any
split except tune unless `eval/LOCK` freezes that exact judge.

**Round 1 (for).** A pre-registration kept on the honor system is weak. Once the test numbers
exist, anyone, me included, can reinterpret a rule. So the command:
- reads Δ_headline, Δ_optional, P_debug_min, and the bootstrap seed from `docs/EVAL.md`;
- refuses if that file's SHA-256 differs from `eval/LOCK`;
- checks that each input file is the right arm, split, and tier;
- computes the paired bootstrap;
- prints every guard as holds, FAILS, or pending.

The run guard makes "frozen before step 4" a property of the tool, not of discipline. It
checks the weights file's SHA-256, θ, the linkage, and the Platt parameters.

**Round 2 (against).**
- It is code for a decision made once; a script would do.
- Code bakes in interpretations. §5's optional tier requires that "every other guard holds",
  and E7 (real trees) is one of those guards. So without real trees, even a winning judge can
  only get a provisional verdict.

**Round 3 (synthesis).**
- The command is small (about 200 lines with tests) and is reused by any later protocol (D34).
- Writing the interpretation down *before* the test run is the whole point. Paired means:
  - use the windows scored by both arms;
  - take the per-window difference in BCubed F1;
  - bootstrap the mean difference: 10,000 resamples, locked seed.
- A guard that fails decides the verdict. A guard not yet measurable (E7 before real trees
  exist) marks a winning verdict **provisional**. So does having fewer than 60 paired test
  windows (§1 size target).
- E5(code) > 0 is reported as a launch blocker whatever the verdict.

**Decision.** Built: `src/eval/decide.rs`, `LearnedJudge::check_frozen`.

## D33. If the learned judge wins, it ships inside the binary (proposed; the user may overrule)

**Idea.** The rung-1 judge is 20 weights and 2 Platt parameters. Compile them in
(`include_bytes!` of the frozen weights file) and `--judge` runs locally. It needs no server,
no network, and no model download.

**Round 1 (for).**
- Deterministic (non-negotiable 7).
- Private: no code leaves the machine.
- About 1 ms per plan, and it cannot time out, so the fail-open path is never exercised.
- Non-negotiable 6 still holds: code answers every hard question (hard binds, order, leave),
  and the model only weighs code facts for the soft merges code cannot settle.

**Round 2 (against).**
- I chose Laya as the judge, and this would ship something that is not Laya.
- The weights come from five MIT-licensed OSS repositories. They may not fit a given
  developer's habits (D29).
- "A model trained on other people's history decides my commits" must be explainable.

**Round 3 (synthesis).**
- Laya stays on D21's ladder as rung 2: a fine-tuned cross-encoder on the same labeled pairs,
  which would need the GPU and disk this machine does not have now.
- EVAL allows one judge run on test per lock. Rung 1 is what can be frozen today, so rung 1
  is what gets tested. Rung 2 needs a fresh test set (D34).
- The weights store no code text, only feature weights, so the provenance story is short.
- Explainability is the D30 comment (the probability and the top features behind a merge).

**Decision.**
- Proposed, not built.
- If `eval decide` returns headline or optional, `--judge` uses the built-in learned judge.
- A Laya client lands behind `--judge=laya` only when a Laya rung beats rung 1 under its own
  protocol.
- The user can overrule this; it is flagged in the session report.

## D34. A temporal holdout for every later judge rung (candidate)

**Idea.** The lock allows one test run. Any later rung (a fine-tuned Laya, D29's per-repo θ)
needs data that no decision has touched. Windows from commits made *after* the freeze in the
same sources are unseen by construction, and they also measure drift over time.

**Round 1.** It is free: hono and zustand are active, and so are my repos. Keeping
the same sources keeps the populations comparable.

**Round 2.** Dates can lie (rebases, bad clocks). Recent windows are a different distribution
(new code, not mature code), and a slow repo may add too few windows.

**Round 3.** Define "after" by **ancestry, not dates**:
- record every source's HEAD oid in `eval/LOCK` at freeze time;
- EVAL v2's corpus is windows whose base descends from that oid;
- report per source, and keep the 60-window size target (provisional below it).

**Decision.** The ancestry anchor already exists: the locked manifest records every source's
`head`, and its SHA-256 is in `eval/LOCK` (noted there as `temporal_holdout_anchor`). Write
EVAL v2 only when a rung 2 exists.

## D35. Train on the questions the merge actually asks (candidate for rung 1.5, under D34)

**Idea.** Pooled linkage (D31) asks the model about *clusters*. It was trained only on pairs of
*components*. Close that gap by training on merge trajectories:
- replay the true grouping of each training window as a sequence of merges;
- at each step, emit a row for the current partial clusters: positive if both come from the
  same commit, negative otherwise;
- also emit rows for the merges the current model would make (the DAgger idea: learn from
  the states your own policy reaches).

**Round 1 (for).** It removes the known train/inference mismatch at its root rather than
absorbing it into θ. Platt scaling and the high θ are symptoms of that mismatch, and a model
calibrated on cluster pairs may need neither.

**Round 2 (against).**
- The θ plateau is wide (0.64–0.74), which suggests pooled's ranking is already good. The
  remaining errors may be missing *signal* (type-only edits link nothing), not calibration.
- Every tune-side iteration spends the tune split's credibility: 66 windows, 41 of them hono.
- The lock allows one test run, so this cannot be evaluated against the current test split.

**Round 3 (synthesis).**
- Rung 1 was frozen without it, on purpose. Iterating until the test gain crosses Δ_headline
  is the garden of forking paths that pre-registration exists to prevent.
- D35 is validated **leave-one-repository-out at the window level on the train repositories**,
  which spends no tune or test data. It is adopted only if the mean held-out BCubed gain is
  0.01 or more *and* it holds on at least 4 of the 5 held-out repositories.
- If it passes, it is evaluated on the D34 temporal holdout, next to rung 1.

**Decision.** Candidate. The first experiment after the rung-1 verdict. It needs `eval run` to
accept the train manifest. D32's guard is therefore narrowed, after step 4, to the locked
corpus only (recognized by its manifest SHA-256).

## D36. Capture real trees without writing to the user's repository (M6, EVAL step 5)

**Idea.** EVAL.md §1.B says to save each real messy tree with `git stash create`. That has
three flaws, found while building the E7 tooling:
1. `git stash create` has no `-u`, so it **drops untracked files**, the new files carve
   exists to handle.
2. Its commit is unreachable, so `git gc` may prune it (after two weeks by default) before
   it is scored.
3. It writes objects into the user's repository.

**Round 1 (for a carve command).** `git carve eval capture --note "..."`, run in the user's
repository:
- git runs with `GIT_DIR` set to a private store (`~/.cache/git-carve/eval/real/<root>.git`)
  and `GIT_WORK_TREE` set to the user's repository, so it only *reads* the user's files;
- every object, index, and ref write lands in the store;
- carve's own snapshot code builds the tree, so untracked files, modes, symlinks, and
  deletions are handled exactly as a real carve run would handle them;
- a ref (`refs/real/<id>`) pins the capture;
- the user's HEAD commit arrives by a shallow fetch over `file://`, so the user's side only
  runs `upload-pack`, which does not write.

**Round 2 (against).**
- It deviates from the locked text of EVAL.md, and the lock forbids editing that text.
- A foreign `GIT_DIR` loses the user's `.git/info/exclude`.
- The rendered plans contain the user's private code.

**Round 3 (synthesis).**
- The deviation is in mechanics, not in what is measured (the same trees and the same
  blinded A/B scoring), and it is recorded here and in `eval/RESULTS.md`, not hidden.
- `info/exclude` is copied into the store; `.gitignore` and the global excludes work
  unchanged.
- Rendered buffers go to the eval cache (`real/blind`), **never** into the project directory,
  which may be published. Only numbers (`eval/results/real.json`) come back.
- Both arms render with `--no-check`, because the real trees' `node_modules` are not
  installed in the store. EVAL does not specify the check here, and the two arms are treated
  identically.
- A regression test proves that the user's `.git` stays byte-identical, mtimes included,
  and that the captured tree equals the independent `git add -A` oracle.
- The A/B order comes from a seeded hash of the capture id, so unblinding is mechanical.

**Decision.** Built: `eval capture`, `eval real-render`, `eval real-score`, then
`eval decide --real`.

## D37. The frozen judge merges on base rate alone (finding; candidate fix for rung 1.5)

**Finding.** On the demo repository, `--judge` folds the drive-by fix into the rename commit.
The merge explanation (`--json`, `diagnostics.judge_merges`) shows why:
`a1,a4,a12,a14,+2 + a8: 0.70 (same_dir +0.44, same_ext +0.36)`. Nothing links the fix to the
rename; the probability sits exactly at θ.
- Training pairs come from windows of 2–4 commits, so two components share a commit 73% of
  the time on tune.
- Platt scaling fitted on tune (b = +0.48) imports that prior.
- A cluster that spans `src/` makes `same_dir` nearly always true under pooled linkage.

So "no evidence" scores 0.70. The model is calibrated on the corpus, and the corpus's base
rate is not a messy tree's base rate.

**Round 1 (fix: an evidence floor).** Merge only when at least one *linking* feature fires:
an F-edge, `same_file`, `stem_link`, `name_shared`, `order_rel`, `new_name_used`, or
`test_impl`. Location features (`same_dir`, `dir_overlap`, `same_ext`, `size_balance`) may
strengthen a link but never create one. This is D8's precision-first rule applied to merges.
A user fixes an over-split by merging two lines in the buffer (cheap), but must untangle an
over-merge atom by atom (expensive).

**Round 2 (against).** On synthetic windows, base-rate merges are usually *right* (few
commits per window), so the floor would likely lower E1 on this corpus. EVAL §7 already names
the cause: synthetic windows over-represent small, clean histories. And the lock forbids
changing the tested judge.

**Round 3 (synthesis).**
- The frozen rung-1 judge stays exactly as tested, and the verdict applies only to it.
- E7 is the guard designed for this: if base-rate merges annoy the user on real trees,
  `cuts_to_change` shows it and the verdict drops to code-only.
- The floor is evaluated together with D35 as rung 1.5:
  - leave-one-repository-out at the window level on the train repositories;
  - then the D34 temporal holdout;
  - *and* E7 on real trees, which is the population the floor is for.
- Meanwhile code-only stays the default, and `try.sh` shows the base-rate merge openly.

**Decision.** Candidate for rung 1.5. The finding is public in `try.sh`.

## D38. Train on trees of many sizes, and let the model see the tree's size (rung 1.5)

**Idea.** D37's root cause is the training corpus, not the model. Every training window holds
2–4 commits, so "two components share a commit" has a prior of about 0.73, and the model
learned it as a constant. Train instead on windows of 2–8 commits (a training-corpus rule; the
locked EVAL corpus stays 2–4), and add a `tree_size` feature (ln(1 + hard-bound components)),
so the prior can depend on how big the tree is.

**Round 1 (for).** Real messy trees range from one logical change to many. A model that
conditions on tree size can be confident on a small tree and cautious on a big one. The
evidence floor (D37) attacks the same symptom from the other side, so both are tested
together.

**Round 2 (against).**
- Tree size is a noisy proxy for commit count (one large feature is many components).
- More negatives may make the model timid on small trees, where the synthetic benchmark lives.
- Every such experiment risks tuning to noise.

**Round 3 (the protocol, fixed before any result).**
- Five variants, weights per held-out training repository (`eval/dev/judge_folds.py`):

  | Variant | Training windows | Features | Evidence floor |
  |---|---|---|---|
  | `r1` (baseline: the rung-1 recipe, per fold) | 2–4 | 20 | no |
  | `r1floor` | 2–4, linked rows only | 20 | yes |
  | `wide` | 2–8 | 20 | no |
  | `wsize` | 2–8 | 21 (+ `tree_size`) | no |
  | `wsfloor` | 2–8, linked rows only | 21 | yes |

- Scoring: `git carve eval sweep` on the 2–8 training corpus (train repositories only; no
  tune or test data).
- θ per fold: chosen on the *other* repositories' windows (maximum mean BCubed F1).
- Reporting: the held-out repository's windows, split into 2–4 and 5–8 commits.
- **Adoption rule, relative to `r1`:**
  - on 5–8-commit windows, a mean gain of 0.01 or more, and no worse on at least 4 of the 5
    held-out repositories;
  - on 2–4-commit windows, no loss beyond 0.005.
- Among adopted variants, a floor variant within 0.005 of the best is preferred (D37: precision
  first, for real trees).
- An adopted variant is only a candidate. It still needs EVAL v2 (D34) and E7.

**Tooling (built):**
- window size as a corpus rule, with defaults that reproduce the locked manifest byte for byte;
- a guard against overwriting a locked manifest;
- `judge_sequence` and `plan_soft`: one merge sequence, cut at any θ, proven equal to a direct
  run;
- `eval sweep`, cross-checked against `eval run` on tune (code 0.769, and rung 1 at θ
  0.5/0.6/0.7 gives 0.831/0.856/0.835 on both paths);
- `tree_size`, optional in weights files, so the frozen weights still load and still reproduce
  tune exactly;
- the evidence floor as a weights-file flag.

**Result (2026-09-24; `eval/results/dev/r15-report.txt`).**

Held-out BCubed F1, leave-one-repository-out, with θ chosen on the other repositories:

| Arm | 2–4 commits (269 windows) | 5–8 commits (68 windows) |
|---|---|---|
| code-only | 0.725 | 0.618 |
| `r1` | 0.760 | 0.601 |
| `r1floor` | 0.751 | 0.600 |
| `wide` | 0.766 | 0.607 |
| `wsize` | **0.778** | **0.633** |
| `wsfloor` | 0.764 | 0.632 |

- **Adoption rule: no candidate; rung 1 stands.**
  - `wsize` and `wsfloor` gain about +0.03 on 5–8 windows, but mostly from tanstack-query.
    They are no worse than `r1` on only 1 (`wsize`) and 3 (`wsfloor`) of the repositories.
  - swr has no scored 5–8 window. The verdict is the same whether it counts as a loss or is
    left out.
- **The judge's advantage lives in small trees.** Against code-only, paired bootstrap:

  | Arm | 2–4 commits | 5–8 commits |
  |---|---|---|
  | `r1` | +0.035 [+0.017, +0.054] | −0.017 [−0.053, +0.019] |
  | `wsize` | +0.054 [+0.035, +0.074] | +0.015 [−0.024, +0.055] |

  The frozen rung-1 judge was validated on 2–4-commit windows, where it clearly helps. On
  larger trees no gain is detectable, and the point estimate is negative. This is D37's
  base-rate concern, measured. Real messy trees may be larger, so it strengthens code-only as
  the default and makes E7 the decisive test. It is noted in `eval/RESULTS.md`.
- **Direction for the next attempt.** Wider training windows plus `tree_size` gave the best
  small-tree gain and a non-negative large-tree estimate, but 68 large windows cannot show
  robustness. That needs more large-window training data: more training repositories, whose
  clones could live on E: via `XDG_CACHE_HOME`, keeping C: flat. It would need a new
  pre-registration, not a re-reading of this one.

**Tooling found on the way.** The pooled feature queries were cubic, so one 347-hunk window
did not finish in 10 minutes under the debug build.
- `Features::of` was rewritten over interned sets and a path-pair table.
- A test proves it bit-identical to the pairwise definition, and the frozen judge
  reproduces tune window for window.
- Sweeps now run on the release build (`cargo build --release`, on E:): 8 s for that window,
  with byte-identical output.

## D39. v1 ships for Linux and macOS; the npm package is a shim with platform binaries (M7)

**Idea.** carve's users are TypeScript developers, and they install things with npm. Ship
prebuilt binaries through npm (SPEC §12 M7).

**Round 1 (native Windows too).** Many npm users are on Windows, and Rust cross-compiles
easily.

**Round 2 (against).** carve leans on behavior that differs on Windows, all of it untested:
- the editor runs through `sh -c`;
- dependency directories are symlinked into the scratch worktree (unprivileged Windows
  symlinks need developer mode);
- paths are handled as bytes;
- executable bits are assumed to mean something (`core.fileMode`);
- line endings are assumed not to be converted (`core.autocrlf`).

Shipping an untested platform contradicts "fail closed".

**Round 3 (synthesis).**
- **Platforms:** v1 ships linux-x64, linux-arm64 (static musl), darwin-x64, and darwin-arm64.
  CI already runs Linux and macOS.
- **Windows:** the npm shim refuses win32 with one line pointing to WSL, where carve is
  developed and tested. Native Windows starts with a Windows CI job, and only on demand.
- **Packaging (esbuild's model):** the `git-carve` package is a 40-line Node shim, and
  `@git-carve/<os>-<cpu>` packages carry the binaries as optional dependencies, selected by npm
  through `os` and `cpu`. There are **no install scripts and no downloads**, so it works under
  `--ignore-scripts` and in locked-down CI. That matters for a tool whose pitch is trust.
- **Release:** a version tag builds all four binaries and drafts a GitHub release with
  checksums. Publishing to npm and crates.io runs only by hand with `publish: true`, behind a
  `release` environment.

**Decision.** Built: `npm/git-carve/`, `scripts/npm-pack.mjs` (tested locally with an npm-like
layout: `--version`, exit codes, and a real plan), and `.github/workflows/release.yml`.

**Before publishing:**
- a license (`Cargo.toml` has none; the npm packages say `UNLICENSED`);
- the npm organization `@git-carve`;
- the `NPM_TOKEN` and `CARGO_REGISTRY_TOKEN` secrets;
- the go-ahead.

## D40. A working tree that does not pass: commit what passes, leave the rest (M3/M4 follow-up)

**Idea.** Until now, carve refused outright when the whole working tree failed the check
(exit 3, "nothing to prove"). But the messy moment is often a non-compiling one: a fix and a
refactor are done while a half-written feature is not. carve should commit the groups that
pass and leave the broken one in the working tree, saying so.

**Round 1 (for).** This is the product promise taken seriously: *every commit passes your
check*, even when the tree as a whole does not. Non-negotiables 2 and 3 are untouched:
- nothing new is written (leaving is simply not committing);
- every commit carve makes still passes.

The alternative the user has today is `git add -p` by hand.

**Round 2 (against).**
- It can surprise: work the user meant to commit stays behind.
- It costs check runs.
- The broken group may come *first* in commit order, so cutting the prefix is not enough.
- An error can surface far from its cause (a removed export breaks an unchanged file).
- A group whose code depends on the broken group breaks too once it is left alone.

**Round 3 (synthesis: code before model).**
- **Ask the compiler first.** tsc prints `path(line,col): error`, and most tools print
  `path:line:col`.
  - Each location maps to the atom covering that line *in the checked tree*. That tree omits
    `leave` atoms, so each atom's position is recomputed from the atoms before it (a debug
    line above the typo shifts tsc's line by one; the real-tsc test covers exactly that).
  - Leave those atoms' groups and re-check. A group that breaks only because its dependency
    was left is named by the next round's errors (at most 3 rounds).
- **Otherwise, search.** When the output names no location, leave one group at a time,
  smallest first, for at most 12 groups.
- **Order of checks.** HEAD is checked first; if it fails too, there is nothing to rescue.
  The refusal says "(and neither does HEAD)".
- **Say it where the user looks.**
  - The buffer header lists the left atoms and quotes up to 3 of the check's own error
    lines.
  - The atoms appear under `leave`, tagged `[does not pass the check]`.
  - The summary adds a note.
  - Moving them back into a commit re-verifies and is refused, as any regroup that fails is.
- **Still refuse** when no rescue passes.

**Decision.** Built in `verify.rs`.
- Regression tests: compiler-guided, search fallback, a full CLI run with exact undo, and the
  demo with a typo in the feature under real tsc (the rename and the fix commit; the feature
  stays).
- One test's expectation changed on purpose: a broken sub-project change is now left out with
  its TS2322 quoted, instead of failing the run.
- On the demo: 6 check runs, about 6 s.

## D41. F7: edits in one file join, except drive-by shapes, and the join is labeled

**Finding (error analysis, train repositories as the development set; no eval data spent).**
Code-only mode misses 43% of same-commit atom pairs. The largest block by far is components
it keeps apart **in the same file**: 37% of all misses. Those pairs belong together 93% of the
time. Every other category is near a coin flip: same directory 0.47, shared identifier 0.27.
Precision does not depend on distance, since far-apart edits are as related as nearby ones,
so a distance bound recovers almost nothing (3.4% of misses within 30 lines).

**Round 1 (for).** Join all edits in a file. On the development corpus (389 windows of 2–8
commits) BCubed rises 0.703 → 0.748, and the extra commits per window fall 5.48 → 2.34. On
eval tune, 0.769 → 0.819. Over-splitting is carve's most common error, and every user pays
for it.

**Round 2 (against).** SPEC §12 M2's acceptance criteria say *"a drive-by fix in an
unrelated function becomes a separate group"*, and §1's pitch is a feature *and a drive-by
fix* becoming separate commits. A blanket file rule breaks the promise in exactly the
scenario the product is sold on, and three fixture tests failed for that reason. Part of the
93% is also humans lumping same-file edits together (EVAL §7).

**Round 3 (synthesis; three variants measured on development and tune).**

| Variant | Dev BCubed | Dev extra commits | Tune BCubed | Keeps the drive-by promise? |
|---|---|---|---|---|
| none (before) | 0.703 | 5.48 | 0.769 | yes |
| F7 (all same-file) | 0.748 | 2.34 | 0.819 | no |
| **F7a (drive-by shapes exempt)** | **0.740** | **2.89** | **0.803** | **yes** |
| F7o (only atoms without edges) | 0.714 | 3.90 | 0.771 | yes, little gain |

- **What F7a exempts.** A *drive-by shape* never joins by file alone: a modification of at
  most 3 lines on each side that defines nothing and is the only change in its function
  (or at top level).
- **Where the misses really are.** F7o's small gain shows most missed mass is two *linked*
  clusters sharing a file. Structurally, one feature spread over a file looks the same as
  two features that both touched `util.ts`; that is a judgment call.
- **Labeling.** Atoms whose place in their group rests on F7 alone are tagged `[same file]`,
  so the weak joins are visible where the user would split them.
- **The frozen judge ignores F7.** Its features know only the edge kinds it was trained on
  (`same_file` already carries the evidence). It still reproduces tune window for window.

**Decision.** F7a is the code-only default.
- Gains: development BCubed +0.037 (all five repositories gain), eval tune +0.034 (0.769 →
  0.803). E4 and E5 on tune are unchanged. The test split is untouched.
- The D5 bridging fixture moved its second feature into its own file, to keep testing
  bridging.
- **This is code arm v2.** The judge verdict (EVAL step 4) compared the frozen v1. Judging
  the judge against v2 needs its own protocol (D34). On tune the judge leads v2 by +0.053.

## D42. Logging in an error path is error reporting, not a leftover debug print

**Finding (development corpus E5 = 3, zod).** carve left out 3 committed lines as debug output:
- `console.dir(json.errors, …)` directly before a `throw`;
- `console.log(_)` inside a `catch` block.

These are error reporting, and leaving real code out silently drops it from the user's
commits. It is the worst kind of error (E5 must be 0).

**Round 1.** A console statement inside a `catch` clause (within its function), or directly
followed by a `throw`, is never debug-only. It stays in its commit.

**Round 2.** Injected debug lines could land in such places and be missed (E4 recall).
Broader rules, such as "anywhere in a block that throws", would also keep genuine prints.

**Round 3.** Keep the rule narrow and measure it.
- Development corpus: E5 3 → 1, E4 precision 0.98 → 0.99, recall unchanged at 0.98. Tune
  is unchanged.
- The remaining case is `console.log('NAN')` in zod's parser, a few lines above a `throw`.
  It reads as a debug print its author committed by accident, so leaving it out is
  arguably right. It is accepted as ground-truth noise, not chased.

**Decision.** Built in the TypeScript adapter (`DebugStmt::error_path`), with a regression
test: error-path logging is committed, and a stray print beside it is still left out.

## D43. `--no-new-errors`: carve a project whose HEAD already fails the check (opt-in; the default is decided in D53)

**Finding (tier B, both splits).** Among windows whose dependencies installed, 27 were refused
because the check failed on the final tree, against 15 scored. D40's reordering showed the
cause: **HEAD fails too.**
- These are real projects' committed trees with genuine, long-lived type errors. My own
  private-repo-B has 8 × TS18046 at HEAD. Next and Vite build despite type errors, so they
  persist.
- With the default check, carve refused before looking at the user's changes. This is the
  largest adoption barrier found so far, and it hits my own repository.

**Round 1 (for).** Where HEAD already fails, the meaningful guarantee is that *no commit adds
an error HEAD did not have*.
- Errors are matched as a multiset of (file, code, message), without positions, so an error
  that merely moved is the same error.
- Failures quote only the new errors, so D40's rescue leaves out exactly what the user broke.

**Round 2 (against).**
- It weakens the headline guarantee, and **non-negotiable 3 ("every commit carve creates passes
  the check") is my product rule.** Silently redefining "passes" would break it.
- Error identity is fuzzy: renamed files make errors look new, and an error fixed in one place
  and re-added identically elsewhere in the same file cancels out.

**Round 3 (synthesis).**
- **Opt-in:** `--no-new-errors`, an explicit, loudly labeled exception like `--no-check`,
  which the rule already allows. It is never on by default.
- **Offered at the refusal:** when HEAD fails with recognizable errors, the message counts them
  and names the flag.
- **Worded honestly everywhere:**
  - buffer: "HEAD already fails the check with N errors; no commit below adds a new one";
  - summary: "each adds no error beyond the N HEAD already had".
- **Imprecision fails safe.** A renamed file's errors count as new, which causes merges, never
  false passes. Crashes, timeouts, and failures with no recognizable errors always fail.
- **The probe (D1) still holds:** a deliberately broken file must produce *new* errors.
- **Mechanics:**
  - HEAD is now checked first (the same number of runs);
  - failed runs record their diagnostics in the check cache;
  - a failure cached before diagnostics were recorded is re-run rather than trusted.

**Measured (tier B tune; test untouched).** Scored windows went from 5 to 10. Of the 7 whose
HEAD fails, 5 now carve, 1 is honestly refused (no single group can be left to avoid new
errors), and 1 is skipped by the harness's mixed-atom rule.

**Open:** should carve switch to no-new-errors *automatically* when HEAD fails
(with the same loud wording), instead of refusing and offering the flag? That changes the
reading of non-negotiable 3, so it is a product decision, not an implementation detail
(resolved in D53).

## D44. `git carve verify`: every commit on a branch passes the check (research item 1)

**Idea (2026-10-07).** Local git CLIs plateau at a few thousand
installs. Real reach comes from becoming a project dependency that runs on every push or in
CI. "Every commit passes the check" is carve's one unclaimed guarantee, so apply it to commits
that already exist as well: check each commit of a range, oldest first.

**Round 1 (for).** It reuses everything verification already has: the scratch worktree, the
tree-keyed cache, the probe, error parsing, and `--no-new-errors`. It never touches the
working tree, index, or refs, so it is safe as a pre-push hook. And it gives a team product
surface without a hosted service.

**Round 2 (against).**
- Tools that run a command on every commit exist: `jj run`, git-branchless `git test run`,
  and mhagger's `git-test`.
- `tsc` on every commit can make a push slow.
- A range default that guesses wrong could check hundreds of commits.

**Round 3 (synthesis: carve's edge is everything around the check).**
- The default TypeScript check picks the projects that own the changed files (D27).
- The probe (D1) proves the check reads the changed files. A vacuous check cannot report
  "all pass"; an explicit one that reads nothing is reported as unverified.
- `--no-new-errors` measures each commit against the base's errors (D43). Without it, a
  failure the base shares gets a hint.
- **Attribution.** A failing commit whose errors all appeared in an earlier commit of the range
  is reported as inheriting them from that commit, so the culprit is named, as if the bisect
  were already done.
- **Speed.** The cache is keyed by tree, so a re-run or a re-push costs nothing. On the demo
  the first run takes 5.5 s and the second 0.26 s.
- **Range.** `@{upstream}..HEAD`, else commits on no remote. With no remote it refuses (exit 2)
  and names an example. `--pre-push` reads the hook's standard input: a deleted ref is
  skipped, and a new branch checks what no remote has yet.
- Exit codes are 0 when everything passes and 3 when any commit fails. `--json` is for CI.

**Decision.** Built: `src/branch.rs`, with 8 tests in `tests/branch_verify.rs`. Among them:
the culprit is named; the working tree, index, and refs are untouched even when dirty and
staged; a second run is fully cached; pre-push checks only what is pushed; the default
range; `--no-new-errors`; a vacuous check is reported; and the demo under real `tsc`.

**Found while testing.** Plain `git rev-parse @{upstream}` exits 128 when there is no
upstream, which carve treated as a crash. `--verify -q` exits 1, quietly, and that is what
the default range now uses.

## D45. A generic adapter: carve works outside TypeScript (research item 6, extended)

**Finding.** On a Python repository carve fell back to one commit per file. It split a new
function from its uses and its test, lumped a drive-by fix in with the feature, showed no
function names, and refused to check at all, with a TypeScript hint (`npx tsc`).
"TypeScript only" is a top risk: every successful git tool is
language-agnostic.

**Round 1 (for).** Most developers who would find carve on GitHub or HN do not write
TypeScript. `git carve verify` (D44) already works with any check. And the literature's
cheapest accuracy gain, identifier flow between hunks (Flexeme), is exactly the
language-agnostic technique.

**Round 2 (against).**
- A real parser per language means a grammar dependency each.
- Regex-free scanning is crude: decorators, nested scopes, strings.
- Linking by name alone can join unrelated hunks that share common words.
- Giving non-TypeScript files facts changes what the frozen judge sees.

**Round 3 (synthesis).**
- **One grammar-free adapter** for Python, Go, Rust, Java, Kotlin, C, C++, C#, Ruby, PHP, Swift,
  Scala, Elixir, Lua, and Dart. It has:
  - definitions from keyword patterns, their extents from indentation (decorators and
    attributes included), and their parents from nesting;
  - imports in the common forms;
  - every identifier as a reference;
  - unambiguous debug statements (`breakpoint()`, `pdb.set_trace()`, `binding.pry`, `dbg!`).
- **It feeds the existing rules,** with no special cases: F0, F1, F2, F3, F6, the F7 drive-by
  exemption, order constraints, labels.
- **Name-only resolution** (trait `name_only_resolution`; TypeScript says no). A reference
  resolves to the one other changed file of the same language that defines it at top level.
  - `module.name` resolves only when `module` is imported, never for `self.x`.
  - Common names, and names more than one changed file defines, resolve to nothing.
- **Default checks by ecosystem**, only when there is no tsconfig at all:
  - `cargo check --all-targets`;
  - `go build ./... && go vet ./...`;
  - `python3 -m compileall -q -f` (labeled "syntax only").

  Otherwise the refusal says how to set a check, with no TypeScript advice.
- **Checks declare which file types they read.** The probe (D1) breaks only those; other changed
  source files are reported unverified. Before, a `.py` change in a TypeScript repository
  passed `tsc` vacuously.
- **The eval corpus builder** keeps debug injection TypeScript-only (it injects TypeScript), so
  the locked manifest still rebuilds byte for byte.
- **F2 keeps the TypeScript stop words** in TypeScript. A first draft used the combined list and
  shifted two TypeScript-only windows. That was caught by the window-level diff and reverted.

**Found by the probe.** `compileall` trusts a cached `.pyc` whose modification *second* matches,
without comparing sizes. carve rewrites files within the same second, so a deliberately broken
file sometimes passed. The probe (D1) caught it intermittently, under parallel tests; `-f`
fixes it.

**Measured (development corpus of 584 windows of real history: click and httpx in Python,
cobra and bubbletea in Go, fd in Rust; sources in `eval/dev/polyglot-sources.json`;
pre-registered variants; never the locked corpus).**

| BCubed F1 | generic off | **generic, all rules** | no F2 | no F2, F7 |
|---|---|---|---|---|
| bubbletea | 0.804 | **0.812** | 0.799 | 0.763 |
| click | 0.740 | **0.768** | 0.734 | 0.652 |
| cobra | 0.802 | **0.813** | 0.786 | 0.709 |
| fd | 0.771 | **0.787** | 0.774 | 0.735 |
| httpx | 0.707 | **0.738** | 0.708 | 0.662 |
| all | 0.764 | **0.783** | 0.760 | 0.704 |

- **The generic adapter with all rules gains in all five repositories.** Extra commits per window
  go from 1.92 to 1.72, and E5 stays 0.
- **My hypothesis that F2 hurts generic files was wrong.** It came from one TypeScript-repository
  window: a new Python service whose files shared ML vocabulary (0.741 → 0.644). Without F2,
  results fall below having no adapter at all. The window is recorded as a known case and is
  not tuned to.
- **TypeScript is unchanged.** The tune split scores exactly 0.803 with the generic adapter off;
  with it on, only that window moves (0.802). The frozen judge differs only in that window
  (0.856 → 0.854).

**Decision.** Adopted for code-only mode. `GIT_CARVE_DEV_NO_GENERIC=1` is the ablation knob.

**Follow-ups:**
- real grammars where they earn it (Python and Go first);
- JavaScript through the TypeScript parser, once the probe question for `.js` configs in
  TypeScript repositories is settled;
- retraining the judge with generic facts.

## D46. Product direction: from any big change to a verified stack (2026-10-13)

**Question.** Are carve's features ones people would love and use weekly, or are they
impressive but unused? Evidence was gathered first (developer demand, the competitive map as
of October 2026), and the strategy was argued from it before this decision.

**Evidence, strongest first:**
1. **AI-sized PRs choke review.**
   - Salesforce Engineering (2026-01) reports agent PRs "regularly expanded beyond 20 files and
     1,000 lines", with reviewers "no longer meaningfully engaging".
   - A Harvard working paper on Jellyfish data (718 firms) finds PR-to-merge time up 49% after
     agent adoption.
   - Godot called AI PRs "draining and demoralizing", and GitHub added PR caps (2026-06-17).
2. **Turning existing big work into small PRs.**
   - GitHub's native stacked PRs drew 900 and 783 points on Hacker News and went GA on 2026-10-06.
   - But `gh stack` has init/add/submit/modify/rebase/merge and no split, and GitHub's own agent
     skill says to plan the stack before writing code.
   - Users ask to split after the fact. HN 47758047: "you need to know the final shape of the
     stack before you start". gh-stack issue #508 asks for `gh stack split`.
   - LLM splits are called "semantically" wrong (HN 47757676).
3. **Fixing commits after the fact.**
   - "Break a previous commit into multiple commits" has 676K Stack Overflow views; "modify a
     specific commit" has 1.85M.
   - git-absorb has 5.7K stars, and its top complaint is trust: "how would you know whether it
     got it right?"

**Weakest:**
- bisecting inside squash commits: squash-merge is at 39% and growing (arXiv 2608.03007), but no
  2025–26 complaints were found;
- "every commit builds" as an abstract virtue: "cared about a lot, by few";
- checked fixups: single-digit reactions.

**Competitive gaps:**
- **Re-splitting existing commits is nearly empty.** Every splitter is manual (`git history
  split`, jj, Sapling, Graphite, lazygit) or LLM-driven. The closest is "preen": file-level,
  it rolls back instead of re-grouping, it writes messages, and it has 0 stars. A research
  paper from 2026-10-08 (RucTangle) makes "each commit runnable" the metric, so followers will
  come.
- **Nobody turns finished work into a stack that passes the check.**
- **Crowded:** reviewer-side PR splitting (CodeRabbit, Devin Review, Stage), and prompt-only
  "atomic commit" agent skills.

**Round 1: what is carve's weakness, really?**
- The positioning weaknesses ("not for squash-merge teams", "not for solo work") are wrong
  framings.
- The value is at **review time**, not in permanent history. Squash-merge discards the commits
  after they have done their job, and a stack survives squash because each PR squashes on its
  own. Solo developers with agents make the messiest trees.
- The one hard blocker is **"uncommitted changes only."** In 2026 the mess is usually already
  committed: Aider commits every edit, agents commit when asked, humans commit "wip".
- And "every commit builds" is loved only when it is concrete. In a stack, each layer is a PR
  with required CI, so carve's check means **every PR in the stack goes green on its first
  push.**

**Round 2: which flagship?**
- **Review mode** (split someone else's PR, read-only) attacks the biggest pain, but the surface
  is wrong: reviewers read in the browser, and the person in pain cannot act alone with a CLI.
  The space is crowded.
- **Agent mode** is a channel, not a product, and alone it inherits the uncommitted-only
  blocker.
- **A verified stack from any change** answers pains 1 and 2 directly. Re-carving committed
  work answers pain 3 and removes the blocker.
- The stack needs the re-carve: the usual input is a pushed PR branch. So the re-carve is the
  engine and the stack is the surface.

**Round 3: the case against.**
- *GitHub could ship `gh stack split`.* Then it would be interactive or LLM-driven, without
  local checks. carve stays the verified, deterministic front end and also feeds Graphite,
  Sapling and plain branches.
- *History rewriting scares people* (HN 46159412: "an incomprehensibly goofed git history").
  Answer:
  - the final tree is byte-identical to the old tip;
  - working-tree files are never touched;
  - `--undo` restores the refs exactly;
  - merges, non-linear ranges and anything that isn't the user's branch fail closed.
- *Is a stack manager a tar pit?* Yes: restacking after review belongs to `gh stack`. carve
  only produces the layers and hands them over.

**Decision.** In order; each phase ships on its own:
1. **`git carve <range>`: re-carve committed work.**
   - The input is any linear range of the current branch (one commit, an agent's auto-commits, a
     whole PR branch), plus any uncommitted changes on top.
   - The base is the range's base. The new side is the current tree, built by a new snapshot
     source that diffs two trees; the grouping, ordering and check engine is unchanged.
   - The old messages appear as `#` comments in the plan buffer, for reference only; carve still
     writes no message.
   - Debug-only lines an agent committed come out of history and stay in the working tree.
   - It fails closed on merge commits and non-linear ranges. Re-carving commits already on a
     remote needs an explicit flag.
   - `--undo` restores the branch ref.
2. **`git carve --stack`:** one branch per carved commit, submitted with `gh stack` when it is
   installed (otherwise plain branches and the exact commands to run). Each layer passed the
   check locally. No restacking or merge management.
3. **An agent gate pack** (Claude Code, Cursor, Codex hooks):
   - block `git push` until `git carve verify` passes, which needs no new ruling;
   - a "carve before you push" recipe through `--plan-only --json` and `--plan-file`.

   **Open question (resolved in D49 and D58): may an agent fill the message lines of the plan
   file?** carve itself still writes none.
4. **Gate, not feature: speed.** Reuse incremental build state in the scratch worktree. A
   3–5-commit carve should take at most about 2× one check.

**Deferred or rejected:**
- **Review mode:** falls out of phase 1 as `--plan-only` on a PR range. Document it; don't
  build it as a product.
- **Bisect inside a commit:** no demand evidence. A documented recipe on phase 1.
- **Verified absorb:** real trust gap, thin moat. Revisit as "absorb review feedback into the
  right layer of a stack" after phase 2.
- **The learned judge:** keep it behind `--judge`; never lead with it.

**Positioning:** "Turn any AI-sized change into a stack your reviewers can read. Every layer
passes your check before you push. You write the messages."

**Metric:** a second use within 14 days per installer, not stars. Below 30%, suspect speed
first, then editor friction.

## D47. `git carve <base>`: re-carve committed work (D46 phase 1, built 2026-10-13)

**Process.** The design was written from the code and reviewed before building. The
review found:
- one product bug: committed large or binary files would have left the history;
- six missed behaviors;
- answers to the open questions;
- a 9-step plan that keeps the build green at every step.

The design record is `docs/design/carve-range.md`.

**Decisions.**
1. **The snapshot gets a base next to its head** (SPEC §5.6).
   - Trees and parents use the base; the compare-and-swap, the race guard and undo use the
     head. Plain carve has base = head.
   - About ten call sites; `take()` is unchanged.
2. **Building the snapshot (`take_since`).** One `diff-tree --raw` over `base..HEAD`, unioned
   with `git status`.
   - Only dirty paths are hashed: committed content is already in repository form, so
     autocrlf and filters cannot disturb it.
   - Paths whose working tree equals the base drop out.
   - A submodule inside the range is refused as a precondition (it would otherwise reach
     `blob_sizes` as an internal error).
3. **`FileSnap.in_head`.** Only files absent from HEAD are left out by default for being
   large or binary (D-leave). Without this, a committed 3 MB asset would vanish from the
   re-carved history and reappear untracked.
4. **Base resolution.**
   - `<base>`, `<base>..` and `<base>..HEAD`; any other tip is refused.
   - A non-ancestor base with exactly one merge-base uses it, with a notice. This is how
     `git carve main` keeps working after main moves.
   - Merges are refused.
   - Commits reachable from any remote-tracking ref are refused unless `--rewrite-pushed`.
     The refusal names the remote branch and suggests `git carve @{u}`.
   - There is no `--rewrite-others` flag: agents commit as themselves, and authorship is
     not preserved by design.
   - Old author dates are not kept: the mapping is many-to-many.
5. **A net-empty range** with a clean tree is refused with the `git reset --soft` hint. "Nothing to
   carve" stays for a clean plain carve.
6. **Attribution, for display only.**
   - Each block's `# from:` line comes from one `git blame --porcelain --contents` per changed
     file over the range, with boundary commits tracked per commit (porcelain prints the
     mark once).
   - Whole-file atoms name the range commits that touched the path.
   - Deletion-only regions are not attributed.
   - Attribution never enters grouping; an F8 "same old commit" edge needs the
     pre-registered experiment in the design doc.
7. **Wording.**
   - When the base fails the check, messages say "the base", not "HEAD".
   - `verify` now says "1 of N commits does not pass" (singular fixed) and hints
     `git carve @{u}`.
   - The old messages' trailers are listed to copy.

**Measured.**
- **Tests:** 217 pass (35 new). That includes the property test over random histories and
  bases, the safety cases, an insta snapshot of the buffer, and a CLI-vs-eval equivalence
  test.
- **Equivalence on real windows** (`git carve eval range-check`, dev corpora only, code-only):

  | Corpus | Windows | Identical | Expected | Mismatched | Refused |
  |---|---|---|---|---|---|
  | `train-manifest-v2` (TypeScript) | 300 | 295 | 4 | 0 | 1 |
  | `polyglot-manifest` (Python, Go, Rust) | 200 | 199 | 1 | 0 | 0 |

  - The "expected" windows add committed binary assets (icons, images), which range mode
    keeps by rule 3. Their atoms are identical in both paths.
  - The refusal is `carve.maxAtoms` (600 regions) on both paths.
  - **So range mode inherits the locked evaluation numbers** (test BCubed 0.743 code-only);
    no re-measurement on test.
- **Latency:**
  - median plan time 120 ms (eval path) vs 171 ms (range path) over 300 TypeScript windows,
    and 78 vs 118 ms over the polyglot windows;
  - without attribution (`GIT_CARVE_DEV_NO_ATTRIBUTION=1`), 103 vs 110 ms (+7%, within the
    design's 10% budget);
  - attribution costs about 50–75 ms per carve, which is negligible next to any check run,
    and is kept.

**Not yet measured:** my real agent branches, which is the design's correctness
check on real use. Run `git carve @{u} --plan-only` (then one committed run and
`git carve --undo`) on three of them.

## D48. `git carve <base> --stack`: a checked stack of PRs (D46 phase 2, built 2026-10-13)

**Process.** The design was written after verifying `gh stack` from GitHub's docs and the
gh-stack source (`cmd/init.go`, `cmd/submit.go`, `skills/gh-stack/SKILL.md`).
Design record: `docs/design/stack.md`.

**Facts that shaped it.**
- `gh stack init --base T b1 … bn` adopts existing branches.
- `gh stack submit --auto` titles a PR with the commit subject **only when the branch has
  exactly one commit**.
- There is no `split` (gh-stack#508). GitHub's own skill says to plan stacks before writing
  code.

**Decisions.**
1. **One commit per layer**, so every PR title is a subject the human wrote. carve passes no
   `--title` or `--body`; gh fills the body from the repository template or the commit body.
   Multi-commit layers and a `stack` marker in the buffer were cut.
2. **Layers.** Layers 1..N−1 are new branches `<branch>-<k>-<slug>`, never with `/`. Layer N
   is the current branch, moved by the existing compare-and-swap, so a re-carved PR branch
   keeps its PR.
3. **Creation and undo.**
   - Each layer is created with a null-old-value `update-ref`, after the race guard and
     before the main CAS.
   - If that CAS fails, the layers are deleted again.
   - `UndoRecord.stack` lists them (`serde(default)`, so older records still parse).
   - Undo deletes the unmoved layers and names the moved ones.
4. **Refusals:**
   - detached HEAD;
   - leftover `<branch>-<k>` / `<branch>-<k>-*` branches, checked before the editor opens
     (the hint points at `gh stack modify|rebase` or `git branch -D`);
   - exact name collisions after editing (nothing is committed; the messages are kept).
5. **Submitting.**
   - **By default carve prints** `gh stack init … && gh stack submit --auto`, or, without the
     extension, `git push -u` and one `gh pr create --base <below> --head <layer> --fill`
     per layer.
   - **`--submit`** runs gh stack after a local preflight (gh with the extension, a remote,
     a known trunk). On failure it exits **6**; the retry commands are idempotent.
   - carve never pushes by itself.
6. **Trunk and remote.** The trunk is the local branch, the remote-tracking branch without
   its remote, or the unique branch at the base commit; otherwise a placeholder. The remote
   is `remote.pushDefault`, else `origin`.
7. **JSON.**
   - `--plan-only --json --stack` lists the planned layers by name prefix (names take the
     typed subjects).
   - A full run with `--json` prints a run report: version, commits, left hunks, verified
     check, range, the stack with layers, and the submit result. With `--submit`, gh's
     output goes to stderr so stdout stays valid JSON.
8. **B5, dependency binds (the soundness gap from D47).**
   - An atom that starts using a dependency the manifest adds (npm, Cargo, Go) binds to that
     manifest line; removals bind the same way.
   - New code files are read through their parsed imports.
   - Python is left out (package and module names differ), and so is a brand-new manifest.
   - It is sound, not complete.
   - Ablation: `GIT_CARVE_DEV_DISABLE_EDGES=B5`.

**Measured.**
- **Tests:** 250 pass. That includes `tests/stack.rs` (21: layers, names, refusals, exact undo,
  a CAS race leaving no layer, the printed commands, and `--submit` against a stub gh with
  exit 6 and JSON) and `tests/manifest.rs` (6).
- **B5 on the dev corpora:**
  - tune (TypeScript): 0 of 72 plans changed;
  - `train-manifest-v2`: 0 of 389;
  - polyglot: 17 of 584, with BCubed 0.7831 → 0.7859. One Go window went from 10 commits to
    2, because many files start using a new module.

**Usefulness.** The weekly version is `git carve main --stack` plus one pasted line. The
risks the review flagged are tracked:
- **slow checks:** phase 4;
- **branch litter after merge:** `gh stack sync --prune`;
- **folding review fixes into a layer:** deferred, verified absorb.

**Result review (2026-10-14).** The review reproduced 5 findings; all are fixed, each with
a regression test:
1. **Major.** `git carve @{u} --stack` on `feat` tracking `origin/feat` used `feat` as its own
   trunk, and `--submit` would have run `gh stack init --base feat … feat`. The trunk is now
   never the top branch.
2. **Major.** B5 took a package.json dependency's line from the first matching key anywhere,
   so a `scripts` entry with the same name (`vitest`, `jest`, `eslint`) left the import
   unbound. That is the CI failure B5 exists to prevent. The lookup is now scoped to the
   section's braces.
3. **Minor.** B5 bound comment lines such as `// moved away from "axios"`. Only import syntax
   counts now.
4. **Minor.** Undo after a push said nothing about the remote copies. It now names them.
5. **Minor.** A re-carve that reuses pushed layer names said nothing. It now says pushing
   replaces them.

Also noted: a crash between creating the layers and the main CAS leaves layer branches with
no undo record. The next run refuses with the `branch -D` hint, so no data is lost. Accepted.

**The biggest gap the review named is iterating on a split.** Re-carving today means deleting the
layers, retyping the messages and force-pushing again. A re-carve that adopts the existing
`<branch>-<k>` layers (CAS-moves them, so their PRs survive) is on ROADMAP.md as the next
stack feature.

## D49. Agents may draft commit messages; a human authors them, unless the repository opts in (2026-10-13)

**The question** (pending since D46): when an agent drives carve, may it fill the message lines
of the plan file?

**Round 1, allow.** Typing five messages for an agent's work is the friction D46's metric
warns about. The agent knows the intent. And carve cannot tell who wrote a `--plan-file`.

**Round 2, forbid.** "Refuses to write your commit messages" dissolves if "but your agent
will". Messages are what people distrust most, and a drafted message carries no provenance.

**Round 3, the synthesis.**
- **carve writes no message text, ever, trailers included.** Non-negotiable 1 is unchanged.
- **The default agent recipe:**
  1. the agent runs `git carve @{u} --plan-only --json`;
  2. it writes its proposals as `# proposed: …` comment lines under each `commit` line;
  3. the human runs `git carve @{u} --plan-file plan.txt --edit`.

  The parser drops comments, so a proposal becomes a message only through a human keystroke.
  An untouched file aborts with "empty message", and that is enforced by code, not by
  prompt.
- **Opt-in agent messages.** `git config carve.agentMessages true` lets an agent write the
  message lines itself.
- **Auditability, either way:**
  - `UndoRecord.plan_source` (`editor` or `file`);
  - a `(plan file)` reflog suffix;
  - a summary line saying the plan came from a file.

Implemented in phase 3 (`docs/design/agent-gate-speed.md`).

**Enforcement gap (result review, 2026-10-14).** `--plan-file` already takes filled
message lines with no editor, so today an agent can skip `# proposed:` entirely. Phase 3
closes this:
- `--plan-file` without `--edit` refuses a plan whose message lines are not empty, unless
  `carve.agentMessages` is set. The refusal names both ways forward.
- A human who writes plan files by hand passes `--edit` (a review in the editor) or sets the
  config.
- `plan_source` records which path ran.

## D50. The push gate: git's pre-push hook enforces, agent hooks explain (D46 phase 3, built 2026-10-14)

**The question:** how does "every commit passes" reach the moment it matters? The moment is a
push, often from an agent.

**Round 1, agent hooks only.** Claude Code, Cursor and Codex can veto a tool call before it
runs. A hook on `git push` sees the agent at the decisive moment, and can tell it how to fix
the problem.

**Round 2, but agent hooks are not enforcement.**
- They are per-agent and per-user, and only best-effort: an alias or a script hides the push.
- Aider has no hook API.
- A human pushes too.

git's own pre-push hook covers all of them, and `verify --pre-push` exists (D44).

**Round 3, the synthesis.**
- **Enforcement is `.git/hooks/pre-push` running `git-carve verify --pre-push`.**
  `git carve hooks install` writes exactly that one file:
  - only on request;
  - never over a foreign hook;
  - never into a `core.hooksPath` another tool manages (then it says what to add);
  - `hooks uninstall` removes only its own.

  A missing `git-carve` binary skips the hook with a warning rather than blocking every push.
- **Agent hooks are the courtesy layer.**
  - `git carve hooks run claude|cursor|codex` reads the event and finds a `git push` in the
    command line, best-effort: quotes are honored, and `&&`, `;` and `|` separate commands.
  - It runs `verify --fail-fast` on what the push would publish, and denies with the failing
    commit, its errors and the fix (fix the code, or the D49 recipe).
  - It always denies the bypasses: `--no-verify`, and `-c core.hooksPath=…`.
  - Any carve error allows the push with a note (it fails open), because the pre-push hook
    is what fails closed.
- **carve never writes `.claude/`, `.cursor/` or `.codex/` files.** `git carve hooks print
  <agent>` prints the snippet, and the user merges it. Agent configuration is the user's, and
  its format changes faster than carve does.
- **Formats verified against each tool's documentation** (`docs/design/agent-gate-speed.md`;
  Codex re-checked 2026-10-14):
  - Claude Code and Codex: `PreToolUse` on `Bash`, `tool_input.command`, deny with exit 2 and
    the reason on stderr;
  - Cursor: `beforeShellExecution`, `{permission, user_message, agent_message}`.

  Claude Code's best-effort `if` field is left out: the gate re-inspects the command anyway,
  and a non-push exits in milliseconds.
- **The JSON contract for agents** is `version: 1` on `verify --json` and on `--plan-only
  --json`. Within a version, fields are only added. `--plan-only --json` carries `buffer`,
  the exact text `--plan-only` prints, so an agent can save it and add `# proposed:` lines.
- **The plugin directory is cut** (the phase 2 review) to one `agent/skills/git-carve/SKILL.md`
  carrying the D49 recipe.

**D49 is enforced in code:**
- `--plan-file` without `--edit` refuses filled messages unless `carve.agentMessages` is true;
- `--edit` seeds the editor with the plan file;
- `UndoRecord.plan_source` is `editor`, `plan-file+edit` or `plan-file`;
- a `plan-file` run's reflog entry ends in `(plan file)`, and its summary says so.

## D51. The speed gate: fail-fast verify and parallel checks (D46 phase 4, built 2026-10-14)

**The question:** how close can a carve get to "about 2× one check"?

**Built:**
- **`verify --fail-fast`** (implied by `--pre-push`): stops at the first failing commit. The
  D1 probe runs only when every commit passed.
- **Parallel checks:**
  - `Checker::check_many` checks up to `jobs` trees at once, in worker worktrees
    `<git-path carve>/w<k>/scratch`. Like the scratch worktree, each is registered under
    `.git/worktrees/` (D15's wording covers one; this is the same kind of write).
  - Base and final share the first round. Spare workers check prefixes ahead of need. The
    probe trees share rounds with the remaining prefixes.
  - A tree's outcome depends only on the tree, so the order of decisions and the plan are
    unchanged.
  - tsc build info is named by project path (it was named by index, so a different project
    set reused the wrong file).

**Benchmark:**
- **Protocol** (`eval speed`, pre-registered in `docs/design/agent-gate-speed.md`):
  - the tune split, tier B, 9 windows;
  - an untimed warm-up remembers the probe verdicts;
  - ratio = the verify step's wall time / one cold standalone check of the final tree.
- **Cold vs warm** was not pre-registered. Both are reported, and cold is primary:
  - **cold:** no build info;
  - **warm:** every worker's build info starts at HEAD.

| Jobs | Cold median | Cold p90 | Warm median | Warm p90 | Plan differences |
|---|---|---|---|---|---|
| 1 | 3.29 | 5.43 | 3.05 | 4.71 | — |
| 2 | 2.09 | 4.45 | 2.38 | 3.21 | 0 |
| 3 | 2.43 | 3.18 | 2.14 | 2.77 | 0 |

**Verdict: the target (median ≤ 2.0, p90 ≤ 3.0) is not met.**
- Parallelism is a real gain: median wall time drops from 18.1 s to 12.1 s cold, and from
  13.1 s to 8.6 s warm.
- On the demo, a cold carve drops from 5.4 s to 2.5 s at 3 jobs, and a cached re-run takes
  0.31 s (target ≤ 0.5 s, met).
- The remaining gap is structural: N groups need at least N + 1 runs (base, final, N − 1
  prefixes).
- The first run in each worker is cold, and parallel tsc runs contend.

**Default jobs = 2** (at most half the CPUs).
- 3 is better warm but worse cold, the 9 windows are noisy, and each extra tsc costs its
  full memory.
- `git config carve.checkJobs 3` is documented.

**Next levers (not built):**
- skip the base check when a verify already cached HEAD;
- pre-warm the workers after a carve, so the next one starts warm.

The result review agrees with the verdict and the default. It names the first lever: the
benchmark re-checks HEAD in every window, which a carve after a carve or a verify would find
cached. Re-run with the base cached before building anything else.

**Result review of D50/D51 (2026-10-14).** The review reproduced 4 findings; all are
fixed, each with a regression test:
1. **Critical.** git exports `GIT_DIR` to a pre-push hook in a linked worktree. The scratch
   worktrees' `read-tree -u` followed it and rewrote the user's index, breaking
   non-negotiable 2. Every git call now drops inherited `GIT_DIR`, `GIT_WORK_TREE`,
   `GIT_INDEX_FILE`, `GIT_COMMON_DIR` and `GIT_PREFIX` (`src/git.rs`). carve finds the
   repository from each handle's directory.
2. A pushed-over remote commit that was never fetched made `remote..local` fail, blocking
   the push on a git error. It now falls back to `local --not --remotes`.
3. The agent gate read shell redirections (`2>&1`, `> log`) as revisions, and failed open.
   They are dropped now.
4. `--plan-file --edit` with `GIT_EDITOR=true` committed an agent's filled messages. Seeded
   messages now become `# proposed:` comments unless `carve.agentMessages` is set.

Plus the adoption gap the review named: in a repository whose HEAD already fails the check,
the first push after `hooks install` was blocked. `carve.noNewErrors` makes `verify`, the
pre-push hook and the agent gate tolerate the base's errors (D43). The refusal names it.

## D52. Stack iteration: a re-carve adopts the earlier stack's layers (ROADMAP item 3, built 2026-10-29)

**Problem.** After review, re-splitting a stack meant deleting its layer branches, which
closes or orphans their PRs, and retyping every message. D48's review called this the
biggest gap: stacks are iterated, not made once.

**Decision.** `git carve <base> --stack` adopts `<branch>-<k>[-*]` branches instead of
refusing them, when they still form the branch's stack.
- **Valid stack:** exactly one branch per k in 1..m, each tip a commit of base..HEAD strictly
  above the one below, none checked out in another worktree. Otherwise refuse before the
  editor, naming the problem. carve never guesses which branch a PR belongs to.
- **Positional mapping.** New layer k moves adopted branch k. I weighed content matching
  (patch-id, file overlap) and rejected it: gh-stack and PR order are positional, the user
  already controls block order in the buffer, and a hidden heuristic choosing which PR gets
  which commit breaks on the very re-splits this exists for. The buffer says which commits
  move which branches.
- **Never delete a layer that may carry a PR.**
  - Extra old layers are left in place, and the summary says to close their PRs.
  - Every rollback (a failed layer CAS, a failed main CAS, undo) moves adopted branches back
    by compare-and-swap. Only branches carve created are deleted.
- **Crash safety.** `layers.journal` (under carve's git path) is written before the first
  adopted branch moves, and removed once the undo record is written. The next run moves
  journaled branches back first. Without it, a crash in that window left a moved layer
  outside base..HEAD, and the refusal would have steered the user to `git branch -D` a PR
  branch. The refusal hint now says `git branch -m`.
- **Pushing.** carve prints `git push -u <remote> --force-with-lease=<layer>:<oid> …`, with
  the remote-tracking oid carve saw: a bare `--force-with-lease` is defeated by a
  background fetch. It also prints `gh pr create` for new layers, and `gh pr edit <branch>
  --base <layer>` when the layer count changed. It never pushes.
- **`--submit`** does not resubmit an adopted stack yet, because how `gh stack init`
  behaves on an existing stack is not verified. It exits 6 (commits complete) with the
  commands.

**The design review (2026-10-15)** made these must-changes, all adopted:
1. Rollback must move adopted refs back, not delete them.
2. Write a journal before the first move.
3. Fail closed on linked worktrees.
4. Enumerate layers unbounded: the old `existing_layers(.., n)` hid layers above the new
   group count.
5. Pin each push lease to an oid.

**Not done.**
- Per-layer provenance in the `# stack:` note ("mostly from old layer 2").
- A warning when a layer's atoms mostly came from a different old layer (positional drift
  after an insert).
- A `gh stack sync --prune` hint for a squash-merged bottom layer.
- Verified absorb: review fixes are grouped by the normal planner, and D47's `# from:` lines
  show where lines came from.

**Tests:** 7 in `tests/stack.rs`:
- adoption with an exact undo;
- the buffer note;
- the worktree and duplicate-layer refusals;
- a failed adoption that moves an earlier layer back;
- journal recovery;
- leased pushes, with `--submit` exiting 6.

## D53. `--no-new-errors` as the default (BUILT 2026-10-30; amends non-negotiable 3)

**Problem.** On tier-B windows carve refused 27 against 15 scored,
because HEAD already fails `tsc`. That includes my own repository. A tool that refuses
its author's repo has no first user.

**Why it was not active at first.** Product rule 3 says every commit carve creates passes the
check, unless `--no-check` is passed. With tolerance on by default, a carve in a repository whose
HEAD fails creates commits that do not pass. They add no *new* error, but they do not pass. A
product rule does not change as a side effect of a build. The flip was built and tested on
2026-10-29, then reverted until the rule was amended (below). No outside user can run carve
before publication, so waiting cost nothing.

**Proposed amendment to non-negotiable 3:**

> Every commit carve creates passes the check, or, when HEAD already fails it, adds no error
> HEAD lacks, and the summary says which. `--strict` refuses instead. `--no-check` makes the
> commits unverified, and the summary says so.

**The flip (about an hour):**
- `--strict` restores refusal on run and verify. `--no-new-errors` stays accepted, hidden.
- `carve.noNewErrors` defaults to true.
- The hints in verify.rs, main.rs and gate.rs flip their advice.
- `eval` keeps its explicit flags: the locked protocol measured refusal.

**Review (2026-10-29) of the flip:**
1. **The failure cache.** Under tolerance, every cached failure without parsed errors was
   distrusted. That is every failure of a non-tsc check (cargo, go, `make test`), so it re-ran
   each time.
   - **Fixed now**, because it also hits opt-in users.
   - Outcomes record `diagnosed` (`src/check.rs`), so only failures cached before D43 re-run.
   - Regression test: `no_new_errors_keeps_cached_failures_of_checks_without_parsed_errors`.
     It fails without the fix.
2. **Fall back to strict when the baseline can't be parsed.** When the default, not a flag or
   config, put tolerance on and the base fails without recognizable errors, carve must quietly
   stay strict. Otherwise a Go repo's push gate errors out about a flag nobody passed.
   - Part of the flip; not built yet.
3. Amend non-negotiable 3 and D43 in the same change (above).

## D54. JavaScript through the TypeScript adapter (built 2026-10-29)

**Problem.** A TypeScript tool that could not read `.js` in 2026.
JavaScript files in TypeScript repositories were invisible: config files, legacy modules, and
JS-only packages.

**Decision.**
- **Which files.** The TypeScript adapter also reads `.js`, `.jsx`, `.mjs` and `.cjs`, with the
  TSX grammar. JavaScript may hold JSX under any of those extensions, and has no `<T>x`
  assertions for that grammar to misread.
- **Strict predicates.** `is_typescript` stays strictly TypeScript, and `is_javascript` is new.
  The "no tsconfig.json" precondition and the corpus injection candidates keep their meaning.
- **Resolution.** JavaScript extensions come after the TypeScript ones, as tsc looks them up
  under `allowJs`.
- **The probe.** The default tsc check lists JavaScript in `reads` and in the new `soft_reads`.
  - JS files are probed. tsc reads them only under `allowJs` or `checkJs`, per project.
  - So a probe on a JS file that the check misses marks the file **unverified** (a warning). It
    never makes carve refuse the check as vacuous.
  - Example: Next.js has `allowJs` but `include: ["**/*.ts", ...]`. A change to only
    `next.config.js` would otherwise have been refused.
- **JS-only repositories** still have no default check. The hint now suggests
  `--check "npm test"`.
- **Cut, by review: CommonJS** (`require`, `module.exports`). In TypeScript repositories
  the JS files that change are mostly config files: whole-file atoms that rarely import changed
  local code. Add it when the dev corpus shows `require('./…')` edges that would have merged
  something.

**Tests:**
- unit tests: JS and JSX facts, and resolution order;
- `tests/languages.rs`:
  - a JS project links a new helper to its caller;
  - `next.config.js` alone is unverified, not refused (fails with the soft rule off);
  - under `allowJs`/`checkJs` the probe proves a `.js` change.

**Invariance (TypeScript output must not change):** the dev corpus
(`train-manifest-v2`, never test) was replayed with the binaries from before and after.
- **TypeScript-only windows:** 360 of the 389. None changed: atoms, groups, scores, pairs and
  leave counts are all identical.
- **Windows with JS files:** 29 (24 scored). 12 changed.
  - BCubed F1 on the 24 went from 0.773 to 0.806: 7 better, 2 slightly worse (−0.006 and
    −0.020).
  - The largest gain: `tanstack-query:db0023e1f6` went from 0.237 to 0.684, with 28 groups
    becoming 11, because its JS files now link.
- **Whole corpus:** E1 went from 0.7394 to 0.7417.

## D55. Agent gate hardening (D50 follow-up, built 2026-10-29)

- `hooks print claude` sets `"onFailure": "block"`. Claude Code lets a timed-out hook through by
  default. The docs say so: "a timed-out command … hook doesn't block the tool call", and
  `onFailure` needs v2.1.295 or later. So a check that outlasts the 600 s timeout no longer lets
  a push through.
  - The git pre-push hook was never affected: it has no timeout.
  - Codex and Cursor snippets are unchanged; no such field is documented for them.
- **Why `hooks run` now fails open on its own errors** (stdin unreadable, a panic, via
  `catch_unwind`). The hook runs before every shell command, so with `onFailure: block` a carve
  crash would otherwise block every Bash call, not just pushes. Timeouts stay the agent's to
  handle.
- **Test:** `printed_agent_snippets_are_valid_json` pins `onFailure` to the Claude snippet only.

## D56. ARI next to BCubed in the eval (ROADMAP item 5, partly)

- Every scored window records its adjusted Rand index, and runs print the mean. Published
  commit-untangling results report ARI: AtomicCommitBench gives agents ≤ 0.46 on real
  tangles. carve's number is now comparable, given the dataset caveats.
- It is not an EVAL.md metric, and the locked protocol is untouched.
- **Hungarian-matched accuracy:** not built.
- **Test:** a worked example, plus the trivial-partition rules.

## D57. Error-guided repair (EXPERIMENT, behind `GIT_CARVE_DEV_ERROR_GUIDED=1`; not shipped)

**Why.** In a head-to-head pilot, a coding agent out-grouped carve on 4 public tune windows.
carve's worst loss, hono:1a2c0d58e5, was not parsing. The
code grouping made 7 groups and every prefix failed `tsc`. Merge-forward (D2) can only fold
group k into k+1, so it cascaded into 1 commit. The agent read the compiler errors instead.

**What (built in part).** After D51's parallel round, a dev-only phase B walks
the boundaries in order. At a failing prefix k, it pulls into group k the later groups that the
check's errors point at, from two tiers:
- **(a1)** an error line inside an atom that is not applied yet (walking the file in the prefix
  tree's coordinates);
- **(a2)** an error line outside every atom that references a same-file definition an
  unapplied atom changes (tsc reports at the use site).

**Bounds:**
- up to 3 rounds per boundary, at most 4 groups per pull, and never every remaining atom;
- a budget of 2n+4 check requests, cached ones included, so a warm cache cannot change the
  plan;
- when the repair fails, it merges forward as in D2. The buffer says what moved.

**Deviations from the design:**
- Tier (b) (quoted symbols) is not built. Neither is the O1/O2 closure, nor exposing the full
  new-error list under D43 (EG reads the tail).
- **The cap changed.** "At most half the remaining atoms" blocked the repair the motivating
  window needs. It is now "never all of them".

**Results (tier B tune, 9 scored windows, cold then warm):**
- EG changed 1 window: hono:1a2c0d58e5 went from 1 group to 3 (the truth is 2).
  - E2 merges: 6 → 2.
  - ARI: 0.000 → 0.545.
  - BCubed F1: 0.914 → 0.894. Precision went 0.84 → 1.00 and recall 1.00 → 0.81: an
    under-split became a mild over-split.
- The other 8 windows are identical.
- Repeat runs give identical plans.
- Plan time on tune: 46 ms → 212 ms mean.
- In the line-level pilot scoring, the same window went from BCubed 0.801 to 0.877 and ARI
  0.000 to 0.649. The agent scored 1.0 on both.

**The pre-registered pass rule is not met.** The rule was "no window's BCubed falls by more than
0.02, and at least one rises". The fall is −0.0197, but nothing rose.

**Verdict: inconclusive, n = 1 affected window. EG stays behind the dev flag.**

**Next:**
- Measure on a corpus where merge-forward fires more often: tier B on the dev train repos, which
  needs their installs.
- Then add tier (b) and the O1/O2 closure.

**Test:** `tests/repair.rs`. A fake check reports the missing definition at the line a
non-adjacent later group adds.
- Merge-forward gives one commit.
- EG keeps the unrelated group apart.
- Warm-cache determinism holds.

**Result review of D54–D57 (2026-10-30).** The review reproduced 3 must-fix findings; all are
fixed, each with a regression test that fails without the fix:
1. **D54: probe memory ignored the extension.** A JavaScript probe's miss was remembered under
   the same key as a TypeScript probe in the same folder.
   - A later TypeScript-only carve was refused as "vacuous".
   - The reverse order reported a `.js` file tsc never reads as proved.
   - **Fix:** soft files get their own slot and remembered key (`…|soft`). TypeScript keys are
     byte-identical, so remembered verdicts survive.
   - **Test:** `javascript_and_typescript_probe_verdicts_are_remembered_apart`, both orders.
2. **D54: a JavaScript file could take a folder's only probe slot from a TypeScript file**
   (most atoms wins), hiding a vacuous check.
   - **Fix:** one hard and one soft slot per folder. Hard slots come first under the probe
     limit.
   - **Test:** `a_javascript_file_never_takes_the_typescript_probe`.
3. **D55: with `onFailure: block`, a missing `git-carve` or a failure after the verdict would
   block every Bash call.**
   - **Fix:** the Claude snippet runs
     `command -v git-carve >/dev/null 2>&1 || exit 0; exec git-carve hooks run claude`,
     as the pre-push script does, and `hooks run` ignores write and flush errors after the
     verdict.
   - **Test:** the printed command, run without git-carve on PATH, exits 0.

The review found the rest sound:
- D57 is inert with the flag unset, and its bookkeeping and determinism hold.
- `diagnosed` defaults to false for old cache entries.
- The ARI formula is correct.


**D53, built (2026-10-30).** I approved the flip and amended non-negotiable 3.
- **The setting.** Tolerance is the default.
  - `--strict` (run and verify) or `git config carve.noNewErrors false` restores refusal.
  - `--no-new-errors` stays accepted, hidden.
  - `branch::tolerance()` returns (on, by default).
- **The review's must-change 2 is built.** When tolerance is only the default and the base fails
  without errors carve can compare, `verify` stays strict instead of erroring. The carve run
  already refused in that case, as strict does, with a hint that says why.
- **Non-negotiable 3 now reads as proposed.**
- **Tests:**
  - the strict paths take `--strict`;
  - default tolerance, and the config switching it off;
  - the unparsable-base fallback, which fails without the fallback.
- `eval` keeps its explicit flags.

## D58. Agent-filled messages by default; `carve.requireHumanMessages` opts back in (reverses D49's default)

**Why.**
- "Refuses to write your commit messages" fights mass behavior.
  Agents write messages by default, at least 14–15M public commits a month carry the Claude
  Code trailer, and the kernel and Mesa relaxed their rules.
- When an agent wrote the code, the agent knows why.
- The D49 contortions kept the slogan, not a user.

**Decision (non-negotiable 1, amended):**
- carve still generates no text.
- `--plan-file` takes filled messages by default, and `plan_source` (in `undo.json` and the run
  JSON) plus the reflog suffix record that.
- `git config carve.requireHumanMessages true` brings D49 back for a repository: filled
  messages are refused without `--edit`, and become `# proposed:` comments with it.
- `carve.agentMessages` is gone.
- The gate's deny text and `SKILL.md` teach the unattended flow: `--plan-only --json`, fill
  `buffer`, then `--plan-file`, with `--stack` for PRs.

**Tests:**
- `an_agent_fills_the_plan_and_carve_builds_the_stack_without_an_editor` (stack.rs);
- `require_human_messages_refuses_filled_plan_files_without_the_editor` (plan.rs);
- the gate's deny text.

## D59. `--layers N`: a stack has at most N layers (default 5)

**Why.** In the agent-PR study, some plans had 15–19 groups. A stack wants 3–5 reviewable
layers.

**Decision.** With `--stack`, after verification, merge the adjacent pair with the fewest
changed lines (the earlier pair on a tie) until at most N remain.
- **The default is 5.** `--layers 0` means no limit, and `--layers` requires `--stack`.
- **No re-check is needed.** Merging two neighbors only removes a boundary, so every remaining
  prefix is the same tree as before.
- The buffer says what merged, and the summary's failure count excludes it.

**Validation:** the study's three most over-split agent PRs, re-planned with the new binary and
applied through `--plan-file` with filled messages (D58):
- 19, 15 and 8 groups each became 5 layers;
- `verify` passed every layer.

**Test:** `layers_caps_the_stack_by_merging_the_smallest_neighbors`.

## D60. The eval harness and the judge flag leave the user's binary; license MIT OR Apache-2.0

- **The `eval` cargo feature (default off)** gates the `eval` subcommands and `--judge` /
  `--no-judge`, along with the eval submodules except `sha256` (the judge's weight hash).
  - The release workflow builds without it.
  - CI runs clippy for both feature sets and the full suite with `--features eval`.
  - `tests/eval.rs` and `tests/judge.rs` require the feature.
- **The judge's library code stays,** for the eval build.
- **The tryout script drops its judge scene.**
- **License: MIT OR Apache-2.0,** the Rust convention, in the npm packages too. The texts are
  copied from the canonical files in the local cargo registry. `publish = false` stays until
  publication.

## D61. F8: a barrel line goes with the new module it names (2026-10-30)

**Found by dogfooding.**
- An agent built a `timing` middleware on a hono branch, and a second agent carved it with
  `--stack`, following only `SKILL.md`.
- carve proposed 5 layers. Each one-line `export * from './middleware/timing'`, in `src/` and
  in `deno_dist/`, became a PR of its own, and the agent merged them by hand.
- **Why:** a barrel line binds no name, so the import-only post-pass found no user and no
  nearest group in its file (barrel files change only that line), and left it alone.

**Decision.**
- **What the parser records.** The TypeScript adapter records the module specifiers of
  re-exports and side-effect imports (`FileFacts.module_refs`).
- **What gets linked.** facts.rs resolves them. An atom that adds such a line, naming a file
  the change set adds, gets `adds_module`.
- **Graph:**
  - a soft edge F8 when the atom is normal code;
  - for import-only atoms, the post-pass sends them to the module's group, after name users
    and before the nearest group in the file.
- **Not covered:** a `package.json` `exports` map pointing at the new build path stays on its
  own. A JSON string match was judged too heuristic.
- **Ablation:** `GIT_CARVE_DEV_DISABLE_EDGES=F8`. The judge's features ignore F8 (mask 0).

**Measured (dev corpus and tune only; never test):**

| Corpus | Windows changed | Better | Worse | Groups after |
|---|---|---|---|---|
| `train-manifest-v2` (389 windows) | 4 | 4 | 0 | 16→14, 19→16, 11→10, 4→3 |
| tune | 1 | 1 | 0 | 7→6 (BCubed 0.837→0.852) |

The aggregate E1 is unchanged at three decimals: the rule is narrow.

**Dogfood:** the hono branch now plans 3 layers:
1. the middleware with its tests, its `deno_dist` copy and both barrel lines;
2. the unrelated typo fix;
3. the `package.json` exports map.

That is close to the agent's own hand-made split.

**Test:** `a_barrel_line_goes_with_the_new_module_it_reexports` (tests/languages.rs). It covers
`export *` and `import './x'`, and fails with F8 disabled.

**Also from the dogfood:**
- **Stack branch names** are cut at a word boundary: `feat-2-retry-failed-fetches-with`, not
  `…-with-backof`.
- **SKILL.md now says:**
  - create the carve directory;
  - pass `--stack` to both steps;
  - how to write a message body;
  - how to merge or move blocks.

**Result review of D53, D58–D61 (2026-10-30).** The review confirmed the tolerance precedence,
F8's determinism and ordering, the slug edge cases, and the feature gating. It reproduced 3
must-fix findings; all are fixed, each with a test that fails without the fix:
1. **The D58/D49 guard could be bypassed.**
   - **Cause:** `messages_as_proposals` stopped at the first `@` line, compared `commit` exactly,
     and kept `\r`, while the parser accepts messages after atoms, a trimmed `commit`, and CRLF.
     With `requireHumanMessages`, a plan file with its messages below the atoms, run with
     `--edit` and `GIT_EDITOR=true`, committed the agent's text.
   - **Fix:** it now lexes exactly like `blocks()`.
   - **Tests:** a unit test for messages after atoms and CRLF; in plan.rs, the end-to-end case
     commits nothing.
2. **The stack summary said "each passed" under default tolerance**, against non-negotiable 3.
   - **Fix:** it now says "each adds no error beyond the base's N".
   - **Test:** `a_stack_over_a_failing_base_says_no_new_error_not_passed`.
3. **The default `--layers 5` squeezed an existing bigger stack.**
   - **Example:** re-carving a 7-layer stack planned 5. The note claimed every adopted layer
     moved, and two PRs were left on old commits.
   - **Fix:** adoption is now known before planning.
     - A default cap rises to keep the adopted layers.
     - An explicit `--layers` is honored, and the buffer names the layers that stay behind.
     - `--plan-only` plans the same way.
   - **Test:** `recarving_a_bigger_stack_keeps_its_layers_under_the_default_cap`.

**Also fixed:** CI now runs the suite with the shipped feature set too, not only with
`--features eval`.

**Noted, not fixed:** a timed-out HEAD check under tolerance gets the "no path(line,col) lines"
hint.
