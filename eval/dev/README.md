# Dev previews (not the pre-registered harness)

Quick measurement tools used while building M2. The pre-registered evaluation is
`docs/EVAL.md` and lands as `git carve eval` in M5. These scripts exist so the numbers
quoted in `docs/DECISIONS.md` (D11) can be reproduced.

- `smoke.sh <repository>...`: robustness pass. Clones each repository into
  `/tmp/carve-smoke`, rebuilds 3-commit windows as dirty trees, and runs
  `git carve --plan-only --json` on each. Checks for crashes and timing; reports sizes only.
- `tune_preview.py <clone>...`: grouping quality on **tune-split windows only** (EVAL.md
  split key: `sha256(root_commit + window_base) mod 10 < 3`). Ground truth comes from
  `git blame`. Reports pairwise precision, recall, F1, BCubed F1, per-rule edge
  precision, and the same-commit base rate. Never run quality analysis on test windows.
- Ablations: `GIT_CARVE_DEV_DISABLE_EDGES=F2 python3 tune_preview.py ...`.
