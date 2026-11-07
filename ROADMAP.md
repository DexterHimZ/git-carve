# git carve roadmap

What is done, what is in progress, and what is planned. Update this file whenever an item
changes state. The design debate behind each item lives in `docs/DECISIONS.md` (D-numbers).

## In progress

**Direction (2026-10-30, D58–D60):** *turn a branch, yours or your agent's,
into a stack of PRs that each pass your check before you push; deterministic, undoable,
callable by your agent.* It comes from a pre-registered study of 40 public agent PRs and a
carve-vs-agent pilot.
- **For it:** agents' PRs hold material for passing stacks (72% have ≥ 2 layers).
- **Against the old pitch:** agents pushed no red intermediate commits (0 of 14).
- **Open:** a strong agent grouped better in the pilot.

**The path to v0.1.0, in order:**
1. [x] D53: tolerance of HEAD's errors is the default (`--strict` refuses).
2. [x] D58: agent-filled plan files are the default (`carve.requireHumanMessages` opts out).
3. [x] D59: `--layers` (default 5).
4. [x] D60: eval and judge out of the user binary; MIT OR Apache-2.0.
5. [x] The README leads with stacks.
6. [ ] Publish: npm and crates.io (D39).
7. [ ] Outside users: 10 people who run agents on TypeScript; go/kill on 2026-11-12.
8. [ ] Error-guided repair (D57) to its pass rule: a tier-B dev corpus, plus tier (b).

## Planned (in order)

1. [x] **Agent gate pack** (D46 phase 3). Built 2026-10-14 (D50): `hooks install|print|run`,
   D49 enforced, the JSON `buffer`, `SKILL.md`. A result review found 4 issues; all are
   fixed.
   - Claude Code, Cursor and Codex hooks that block `git push` until `git carve verify`
     passes.
   - A "carve before you push" recipe through `--plan-only --json` and `--plan-file`.
   - Messages, per D49: the agent drafts `# proposed:` lines and the human authors them with
     `--edit`. Code enforces this: `--plan-file` refuses filled messages unless
     `carve.agentMessages` is set.
2. [x] **Speed gate** (D46 phase 4). Built 2026-10-14 (D51): fail-fast verify and parallel
   checks, with a default of 2 jobs. **The 2× target is missed** (cold median 2.09, p90 4.45).
   Next lever: the base already cached.
   - First, fail-fast `verify --pre-push`.
   - Then parallel prefix checks (`check_many`) and incremental build state in the scratch
     worktree.
   - Target: a 3–5-commit carve takes at most about 2× one check.
3. [x] **Stack iteration** (the D48 review's biggest gap). Built 2026-10-29 (D52): adoption,
   CAS moves, a crash journal, leased push commands. Verified absorb and per-layer
   provenance are still open.
   - A re-carve adopts the existing `<branch>-<k>` layers and CAS-moves them, so their PRs
     survive.
   - Their old subjects are shown for reference, as in D47.
   - Today, a re-split means deleting the layers, retyping the messages and force-pushing.
   - Folds in "verified absorb": put review fixes into the right layer.
4. [ ] **Accuracy features from the literature:**
   - hunk-role typing;
   - name-flow edges;
   - co-change coupling;
   - F7 as weaker evidence.
5. [~] **ARI and Hungarian-matched accuracy in the eval**, next to BCubed. ARI is built
   (2026-10-29, D56). Hungarian-matched accuracy is still open.
6. [ ] **Deeper non-TypeScript support:**
   - Python and Go grammars;
   - [x] JavaScript through the TypeScript parser (2026-10-29, D54: ES modules; CommonJS
     waits for corpus evidence);
   - retraining the judge with generic facts.
7. [ ] **Commit hooks:** validate messages with `commit-msg` (commitlint).
8. [ ] **Rung 1.5 judge** (D35, D37, D38), under a new pre-registration with the D34 holdout.
9. [ ] **D40 v2:** leave just the rejected hunks plus their dependents.

**Deferred by D46** (built on phase 1 as recipes, not products):
- **review mode:** `--plan-only` on someone else's PR range;
- **bisect inside a squashed commit;**
- **verified absorb:** now part of planned item 3 (stack iteration).

## Open decisions

- **E7:** capture 10–15 real messy trees with `git carve eval capture --note "..."`. It is the
  only evidence about real use, and the judge verdict stays provisional without it.
- **D43:** should `--no-new-errors` turn on automatically when HEAD already fails the check?
- **Agent-written messages:** decided by D49 (agents draft as `# proposed:` comments; a human authors; `carve.agentMessages` opts a repo in). See planned item 1.
- **The README GIF:** record `docs/demo.tape` with VHS. VHS is not installed here, so the tape
  is untested.
- **Before publishing:** a license, the npm organization `@git-carve`, registry tokens, and the
  go-ahead (D39).

## Done

| Item | Decision | Notes |
|---|---|---|
| Core: snapshot, atoms, reconstruction, commit, compare-and-swap refs, exact undo | M1 | property-tested, safety suite |
| Code graph grouping: renames, definitions and uses, systematic edits (F6) | M2, D26 | |
| Same-file grouping with a drive-by exemption and the `[same file]` tag | D41 | tune BCubed 0.769 → 0.803 |
| Error-path logging is committed, never left as debug output | D42 | |
| Per-commit verification: probe, cache, merge-forward, syntax pre-check | M3, D1, D2, D13, D14 | |
| Default TypeScript check covers the projects that own changed files | D12, D27 | |
| Commit what passes, leave what the compiler rejects | D40 | |
| `--no-new-errors` for a HEAD that already fails the check | D43 | opt-in |
| One editor buffer: scissors diff, message recovery, deterministic | M4, D16, D17, D19 | |
| Pre-registered evaluation harness and locked protocol | M5, D22–D25 | |
| Learned judge (rung 1) behind `--judge`, with merge explanations and unsure notes | M6, D28–D33, D30 | verdict: optional, provisional |
| Real-tree capture that writes nothing to the user's repository | D36 | |
| npm packaging (no install scripts) and the release workflow | D39 | not yet published |
| Viability research (five workstreams) | 2026-10-07 | |
| Branch verification: `git carve verify` (range, pre-push, CI, `--no-new-errors`, names the commit that introduced each error) | D44, 2026-10-07 | 8 tests; a cached re-run takes 0.26 s on the demo |
| Languages beyond TypeScript: keyword-based reading for 20+ languages, default checks for Rust, Go, and Python, files the check does not read reported as unverified | D45, 2026-10-08 | polyglot BCubed 0.764 → 0.783, better in all 5 repos; TypeScript unchanged (0.803) |
| Re-carve committed work: `git carve <base>` (commits since the base plus uncommitted changes), old commits listed and attributed per block, `--rewrite-pushed`, exact undo | D46 phase 1, D47, 2026-10-13 | 217 tests; on 500 real windows the plan equals the evaluated path (494 identical, 5 expected asset differences, 0 mismatches) |
| Stack output: `git carve <base> --stack [--submit]`, one checked branch per commit for `gh stack` or plain `gh pr create`, exact undo, exit 6 on a failed submit, JSON run report; B5 binds new dependency uses to their manifest line | D46 phase 2, D48, 2026-10-13 | 255 tests after the result review (5 fixes, 2026-10-14); B5 changes 0 of 461 TypeScript dev windows and 17 of 584 polyglot windows (BCubed 0.7831 → 0.7859, unchanged by the fixes) |
