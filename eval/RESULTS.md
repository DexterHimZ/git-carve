# git-carve: evaluation results (EVAL.md §4 step 6)

**Decision (computed by `git carve eval decide`, DECISIONS.md D32): `Optional`, provisional.**
The judge ships as an optional `--judge` flag, not mentioned in the launch copy. The verdict is
provisional because E7 (real trees, §1.B) has not been scored yet. If E7 fails its guard, the
verdict becomes code-only.

- Protocol: `docs/EVAL.md`, locked (SHA-256 in `eval/LOCK`: `eb85109d…`). EVAL.md unchanged
  since the lock; no crash fixes were needed (§8 is empty).
- Judge arm: D21 ladder rung 1, the learned merge model (D28). Pooled linkage, θ = 0.70 after
  Platt scaling (D31). Frozen in `eval/LOCK` before step 4: weights `797b0395…`, training
  repositories ky, zod, jotai, swr, and tanstack-query (disjoint by root commit).
- Code arm: re-run at the judge arm's source hash `2efde96c…`. It reproduces
  `baseline.json` window for window on both tiers (D25).
- Step 4 ran once, 2026-09-24.

## Test split, tier A (every window; `--no-check`)

| Arm | Windows | E1 BCubed F1 (95% CI) | Pairwise P / R / F1 | E3 | E4 P / R | E5 | E6 ms |
|---|---|---|---|---|---|---|---|
| code | 139 | 0.743 [0.716, 0.770] | 0.796 / 0.602 / 0.686 | 2.20 | 1.00 / 0.88 | 0 | 223 |
| judge | 139 | 0.798 [0.772, 0.823] | 0.752 / 0.772 / 0.762 | 1.17 | 1.00 / 0.88 | 0 | 278 |

**E1 gain: +0.055** (paired bootstrap 95% CI [+0.032, +0.079], 139 windows, seed
20260924). Δ_optional = 0.03 ≤ gain < Δ_headline = 0.1.

## Test split, tier B (windows whose dependencies installed; the check runs)

| Arm | Windows | E1 BCubed F1 | E2 merge-forwards per window |
|---|---|---|---|
| code | 10 | 0.765 | 0.70 |
| judge | 10 | 0.778 | 0.10 |

## Guards (§5)

| Guard | Result |
|---|---|
| E1 CI lower bound > 0 | holds |
| E2 mean(judge) <= mean(code) | holds |
| E4 precision(judge) >= P_debug_min | holds |
| E5(judge) = 0 | holds |
| E7 median cuts(judge) <= median cuts(code), judge preferred >= code | pending |

E5(code) = 0: no launch blocker.

## Per source (EVAL §7: report per repository, not only pooled)

| Source | Windows | code | judge | gain |
|---|---|---|---|---|
| hono | 86 | 0.720 | 0.804 | +0.084 |
| private-repo-C | 11 | 0.789 | 0.776 | -0.013 |
| private-repo-D | 3 | 0.797 | 0.758 | -0.039 |
| private-repo-A | 7 | 0.688 | 0.765 | +0.077 |
| private-repo-B | 4 | 0.717 | 0.741 | +0.023 |
| zustand | 28 | 0.807 | 0.808 | +0.001 |

- Most of the gain comes from hono (86 of 139 windows).
- On my own four repositories the gain is small and mixed: private-repo-A and
  private-repo-B improve; private-repo-C and private-repo-D are slightly worse (with 11 and 3
  windows).
- The tune split over-stated the gain (+0.087 on tune vs +0.055 on test), as expected after
  selecting θ and linkage on tune.

## What the judge gets wrong (DECISIONS.md D37)

It trades precision for recall (pairwise precision 0.796 → 0.752, recall 0.602 → 0.772). Some
merges rest on the corpus's base rate alone. On the demo repository it folds the drive-by fix
into the rename commit, on `same_dir` and `same_ext` evidence at exactly θ. Code-only stays
the default. The fix (an evidence floor) is a rung-1.5 candidate under a fresh protocol
(D34, D35, D37).

**Exploratory, from the training repositories, not the locked corpus (DECISIONS.md D38).**
With leave-one-repository-out, the rung-1 recipe beats code-only on 2–4-commit windows
(+0.035, 95% CI [+0.017, +0.054]). On 5–8-commit windows no gain is detectable (−0.017,
[−0.053, +0.019]). The verdict above rests on 2–4-commit windows. Real messy trees can be
larger, which is one more reason E7 decides.

## Range carves inherit these numbers (D47, dev corpora only)

`git carve <base>` re-carves committed work. `git carve eval range-check` runs each window
twice, code-only:
- **the eval path:** the window materialized as a dirty tree on its base;
- **the product path:** the window's last commit checked out, the injected debug lines
  applied as uncommitted edits, then `git carve <base>`.

It compares the plans byte for byte. This is never run on the locked test split.

| Corpus | Windows | Identical | Expected | Mismatched | Refused | Median plan ms (eval / range) |
|---|---|---|---|---|---|---|
| `train-manifest-v2` (TypeScript) | 300 | 295 | 4 | 0 | 1 | 120 / 171 |
| `polyglot-manifest` (Python, Go, Rust) | 200 | 199 | 1 | 0 | 0 | 78 / 118 |

- **"Expected" windows** add committed binary assets. Their atoms are identical, but range
  mode keeps those files in history while the eval path, which sees them as untracked, leaves
  them out (D47 rule 3).
- **The refusal** is `carve.maxAtoms` on both paths.
- **The plan-time difference is the `# from:` attribution.** Without it
  (`GIT_CARVE_DEV_NO_ATTRIBUTION=1`) the median is 103 / 110 ms on 50 windows.
- **So the numbers above (code-only test BCubed 0.743) apply to range carves as they stand.**

## B5 dependency binds (D48, dev corpora only)

Code-only `eval run`, with B5 on and off (`GIT_CARVE_DEV_DISABLE_EDGES=B5`):

| Corpus | Windows | Plans changed | BCubed F1 off → on |
|---|---|---|---|
| tune split (TypeScript) | 72 | 0 | 0.8016 → 0.8016 |
| `train-manifest-v2` (TypeScript) | 389 | 0 | 0.7394 → 0.7394 |
| `polyglot-manifest` (Go, Rust, Python) | 584 | 17 | 0.7831 → 0.7859 |

The TypeScript corpora barely change manifests: the corpus builder rejects lockfile windows.
In one Go window, many files start using a newly required module, and the plan goes from 10
commits to 2: B5's sound-not-complete trade-off.

After the review fixes of 2026-10-14 (each dependency's line is looked up inside its own
section, and only import syntax counts), every number above is unchanged.

## Speed: parallel checks (D51, tune split, tier B)

`git carve eval speed`: 9 windows, each carved at 1, 2 and 3 jobs. The ratio is the verify
step's wall time over one cold `tsc` of the final tree.

| Jobs | Cold median | Cold p90 | Warm median | Warm p90 | Plans differing |
|---|---|---|---|---|---|
| 1 | 3.29 | 5.43 | 3.05 | 4.71 | — |
| 2 | 2.09 | 4.45 | 2.38 | 3.21 | 0 |
| 3 | 2.43 | 3.18 | 2.14 | 2.77 | 0 |

- **The pre-registered target (median ≤ 2.0, p90 ≤ 3.0) is not met.**
- The default is 2 jobs.
- **Cold** means no build info; **warm** means each worker's build info starts at HEAD.

## Pending: E7 (real trees; steps 5 and 6)

Capture 10–15 real messy trees while working:
1. `git carve eval capture --note "..."` in the repository. This writes nothing to the
   repository (D36).
2. `git carve eval real-render` renders blinded A/B plans into the eval cache, outside this
   project.
3. Fill in `scores.json`.
4. `git carve eval real-score --scores <file>`.
5. `git carve eval decide --code eval/results/dev/judge/test-code-rerun.json --code-tier-b
   eval/results/dev/judge/test-tierb-code-rerun.json --real eval/results/real.json`.

**Deviation, recorded:** EVAL §1.B's `git stash create` was replaced by `eval capture`.
`stash create` drops untracked files, its commit can be pruned by gc, and it writes into the
user's repository (D36). What is measured does not change.
