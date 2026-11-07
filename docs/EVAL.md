# git-carve: evaluation protocol (pre-registered)

**Purpose:** decide, before looking at the results, whether the judge (Laya; it replaced Jev on 2026-09-24, DECISIONS.md D20) improves on the
code-only graph enough to be the headline, to ship as an optional flag, or to go unmentioned.

**Rule:** the "Locked values" section must be filled in and committed by the user **before**
the judge arm runs on the test split. After that, no changes except bug fixes that crash
runs, and every such fix is logged at the bottom of this file.

---

## 1. Corpora

### A. Synthetic tangles (automatic ground truth)

**Sources:** the user's own repos in the target language, plus 2 popular open-source repos in
the same language (the user picks them and writes the names in the Locked values section).

**Sampling (DECISIONS.md D24):** non-overlapping windows of 2–4 consecutive commits along
first-parent history, forming a parent chain (a merge breaks a window), by one author, touching
at most 15 files in total. At position i the window size is
`2 + (sha256(root_commit_oid + commit_i_oid) mod 3)`; an accepted window advances past itself,
a rejected one advances by one commit. Skip windows that
contain submodule, LFS, or vendored-directory changes. Skip windows that change
`package.json` or a lockfile: base and final would need different `node_modules`.

**Tiers (DECISIONS.md D23):**
- **Tier A** is every sampled window, scored on `--no-check` plans: E1, E3, E4, E5, E6.
- **Tier B** is the windows whose base passes the check after a cached
  `npm ci --ignore-scripts` (keyed by lockfile blob id, at most 3 lockfile versions per
  repository): E2 and a secondary "E1 verified". Tier B results are reported with their own
  sample size.
- **Tier B mechanics (M5):**
  - Dependencies are installed with the lockfile's own manager: `npm ci`, yarn 1 via corepack,
    or pnpm via corepack, chosen by the pnpm lockfile version.
  - Every install runs with `--ignore-scripts`, and every cache and store is redirected into
    `~/.cache/git-carve/eval/`.
  - `node_modules` is linked into the *cache clone* only; the user's repositories are never
    touched (user approval 2026-09-24).
  - Windows whose lockfile is bun, yarn berry, not among the source's three most common
    versions, or fails to install are excluded from tier B, and the reason is recorded.

**Dedupe:** identify each repo by its root commit (`git rev-list --max-parents=0 HEAD`,
smallest id if there are several). Two clones of one repository contribute windows once.

**Tangling:**

- Base = the parent of the first commit in the window.
- Dirty tree = the tree of the last commit, materialized as uncommitted worktree changes on
  top of the base.

**Ground truth:** map each atom to the window commit that produced it.

- For added lines, use `git blame --porcelain <base>..<last> -L <range> -- <path>`.
- For atoms that only remove lines, use `git blame --reverse` on the base file to find the
  last commit containing the line. The removing commit is its child within the window.
- Atoms that map to more than one commit are **mixed**. Exclude them from pairwise scoring.
  Drop the whole window if more than 20% of its atoms are mixed.

**Debug injection:** in 50% of windows, insert 1–2 debug statements in the language's usual
form (`console.log(...)` or `debugger;`) after a statement inside a function body. Each must be
separated from other changes by at least one unchanged line and must still parse (Tier A) or
pass the check (Tier B). These are labeled as leftover ground
truth. Record the random seed.

**Split:** assign each window to **tune (30%)** or **test (70%)** using
`sha256(root_commit_oid + window_base_oid) mod 10 < 3`. Keying on the root commit, not the
repo name, keeps a window in the same split no matter which clone it came from. Thresholds are tuned only on tune.

**Size target:** at least 60 test windows. If there are fewer, report the numbers but treat
the decision as provisional.

### B. Real messy trees (human judgment)

Collect 10–15 real dirty trees from the user's own work over the coming days. Before
committing normally, run `git stash create` and save the oid and a note in
`eval/real/index.jsonl`. These are **not** used for tuning.

For each tree, run both arms with `--plan-only --json`, render both plans side by side,
**blinded** (arm labels hidden and order randomized by the script), and have the user score:

- `cuts_to_change`: how many regroupings the user would make.
- `leave_disagreements`: leave decisions the user disagrees with.
- `preferred`: A, B, or tie.

## 2. Arms

- **code**: `--no-judge`.
- **judge**: `--judge` with `FixtureJudge` replaying cached real Laya answers (pinned checkpoint and Hugging Face revision). The first run
  populates the cache from the live API, and every scoring run replays it. This makes the
  arm deterministic and re-scorable.

Both arms use the same check command, carve version, and corpus.

**If the judge is trained** (DECISIONS.md D21: zero-shot Laya measured at chance on this task):
- Training data comes only from repositories **disjoint from this corpus**, checked by root
  commit. Never the user's repositories, never a tune or test window.
- The trained judge (weights, Hugging Face revision or file hash, and training-repo list) is
  frozen and recorded in `eval/LOCK` **before** step 4. The tune split may be used for
  calibration and thresholds only (step 3), never for training.
- The judge arm is whichever rung of D21's ladder is frozen at that point; the decision
  rules below do not change.

## 3. Metrics

| ID | Metric | Better |
|---|---|---|
| E1 | **Primary:** BCubed F1 of the grouping per window, over non-mixed, non-leave atoms, macro-averaged over windows. **Secondary:** pairwise precision, recall, and F1 of the "same commit" relation (micro-averaged) | higher |
| E2 | Merge-forward count per window (SPEC §8.4) | lower |
| E3 | Group-count error: absolute difference between produced groups and true commits | lower |
| E4 | Debug-leftover precision and recall against injected statements | higher |
| E5 | **Real code wrongly left**: atoms put in `leave` that are not injected debug lines, excluding SPEC §5.3's default leave of large or binary untracked files (identical in both arms, and shown to the user) | must be 0, for both arms |
| E6 | Wall time excluding check runs; judge round-trip latency | report only |
| E7 | Real trees: median `cuts_to_change`, total `leave_disagreements`, preference counts | lower / higher |

## 4. Procedure

1. Build the corpora and commit `eval/corpus/manifest.json` (window list, seeds, split).
2. Run the **code** arm on tune and test, then commit `eval/results/baseline.json`.
3. On **tune only**, before tuning: count each code-edge kind's precision to set
   `prior(kind)` (SPEC §7.4); measure the judge's expected calibration error (10 equal-width bins)
   and fit Platt scaling if it exceeds 0.1; ask every `same_change` pair in both orders once
   and, if the mean |Δp| exceeds 0.1, average both orders' logits from then on.
   Then tune `θ` and `T_debug` on **tune only**: grid search maximizing E1, subject to E4
   precision ≥ the locked minimum and E5 = 0. Commit all of it in
   `eval/results/thresholds.json`.
4. Run the **judge** arm on **test** once. Record `eval/results/judge.json`.
5. Score the real trees (B) blinded. Commit `eval/results/real.json`.
6. Apply the decision rules in §5 mechanically. Write `eval/RESULTS.md` with the table and
   the decision.

No re-runs of step 4 after seeing its output, except to fix crashes (logged below).

## 5. Decision rules

**The judge (Laya) is the headline** if all of these hold on the test split:

- E1(judge) − E1(code) ≥ **Δ_headline**, and the 95% paired-bootstrap CI of that difference
  (10,000 resamples of test windows, seed recorded) has a lower bound above 0
- E2 mean(judge) ≤ mean(code)
- E4 precision(judge) ≥ **P_debug_min**
- E5(judge) = 0
- E7: median `cuts_to_change`(judge) ≤ median(code), and the judge is preferred at least as often
  as code

**The judge ships as an optional `--judge` flag, not mentioned in the launch copy,** if
Δ_optional ≤ the E1 gain < Δ_headline, the CI lower bound is above 0, and every other guard
holds.

**Otherwise, ship code-only.** The README and launch do not mention the judge.

**Either way:** if E5(code) > 0 on the test split, the code-only debug patterns are a launch
blocker. Fix them and re-run the code arm (SPEC §15).

## 6. Locked values (locked 2026-09-24; see `eval/LOCK`)

```
Δ_headline      = 0.10    # E1 (macro BCubed F1) gain of the judge arm over the code arm
Δ_optional      = 0.03
P_debug_min     = 0.95
OSS repos       = honojs/hono, pmndrs/zustand   (chosen 2026-09-24)
Check command   = carve's default check (DECISIONS.md D12, D27): `tsc --noEmit -p <project>` for the
                  root project, its references, and every project owning a changed file
Bootstrap seed  = 20260924
Injection seed  = 20260924
Corpus          = eval/corpus/manifest.json (228 windows: 72 tune, 156 test; SHA-256 in eval/LOCK)
Code arm        = frozen at carve source SHA-256 56f76184e62a… (eval/results/baseline.json)
Locked          = 2026-09-24. The SHA-256 of this file is recorded in eval/LOCK.
```

**Tier B at lock time (DECISIONS.md D23):** 10 test and 5 tune windows scored. The limits
are recorded, not hidden:
- bun-era hono windows are excluded (bun not installed);
- private-repo-C's historical lockfiles are out of sync with its package.json, so `npm ci`
  refuses them;
- private-repo-A's sub-project (`frontend`) has its own lockfile, which tier B does not install;
- at most 3 lockfile versions per source are installed (the user chose to stop disk growth on
  a nearly full drive).

The E2 guard compares both arms on the same tier B windows.

## 7. Known threats to validity

- Original commits are imperfect ground truth, since humans tangle changes too. That is
  why the real-tree scoring (B) is the tiebreaker in the headline rule.
- Synthetic windows over-represent clean boundaries between commits.
- Injected debug lines are easier to spot than real ones. E4 on synthetic data is an upper
  bound.
- Name-only symbol resolution behaves differently on large versus small repos. Report the
  results per repo, not just pooled.

## 8. Change log (after locking)

| Date | Change | Reason | Affects results? |
|---|---|---|---|
| 2026-10-30 | Private repository names replaced with aliases (private-repo-A to D) | Privacy | No: names only; no threshold, metric, split or procedure changed |
