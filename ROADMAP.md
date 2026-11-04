# git carve roadmap

What is done, what is in progress, and what is planned. Update this file whenever an item
changes state. The design debate behind each item lives in `docs/DECISIONS.md` (D-numbers),
and the dated history lives in `WORKLOG.md`.

## In progress

Next pick: D46 phase 1, `git carve <range>` (planned item 1). D46 (2026-10-13) set the order
below from researched developer demand and the October 2026 competitive map.

## Planned (in order)

1. [ ] **Re-carve committed work: `git carve <range>`** (D46 phase 1).
   - Input: any linear range of the current branch (one commit, an agent's auto-commits, a
     whole PR branch), plus uncommitted changes on top.
   - A new snapshot source diffs two trees; grouping, ordering and checking are unchanged.
   - Old messages appear as `#` comments in the plan buffer. Debug-only lines committed by an
     agent come out of history and stay in the working tree.
   - It fails closed on merges and non-linear ranges; pushed commits need an explicit flag.
   - `--undo` restores the ref.

   This removes carve's one hard blocker: in 2026 the mess is usually already committed.
2. [ ] **Verified stack output: `git carve --stack`** (D46 phase 2).
   - One branch per carved commit, submitted with `gh stack` (GitHub stacked PRs, GA
     2026-10-06) when installed; otherwise plain branches and the exact commands.
   - Every layer passed the check locally.
   - No restacking or merge management: that stays with `gh stack`.
3. [ ] **Agent gate pack** (D46 phase 3; replaces "agent-callable mode").
   - Claude Code, Cursor and Codex hooks that block `git push` until `git carve verify`
     passes.
   - A "carve before you push" recipe through `--plan-only --json` and `--plan-file`.
   - **Decision pending (owner):** may an agent fill the message lines of the plan file? carve
     itself still writes none (non-negotiable 1).
4. [ ] **Speed gate** (D46 phase 4). Reuse incremental build state in the scratch worktree,
   and parallel prefix checks. A 3–5-commit carve should take at most about 2× one check.
5. [ ] **Accuracy features from the literature:**
   - hunk-role typing;
   - name-flow edges;
   - co-change coupling;
   - F7 as weaker evidence.
6. [ ] **ARI and Hungarian-matched accuracy in the eval**, next to BCubed.
7. [ ] **Deeper non-TypeScript support:**
   - Python and Go grammars;
   - JavaScript through the TypeScript parser;
   - retraining the judge with generic facts.
8. [ ] **Commit hooks:** validate messages with `commit-msg` (commitlint).
9. [ ] **Rung 1.5 judge** (D35, D37, D38), under a new pre-registration with the D34 holdout.
10. [ ] **D40 v2:** leave just the rejected hunks plus their dependents.

**Deferred by D46** (built on phase 1 as recipes, not products):
- **review mode:** `--plan-only` on someone else's PR range;
- **bisect inside a squashed commit;**
- **verified absorb:** revisit as "absorb review feedback into the right stack layer" after
  phase 2.

## Open decisions

- **E7:** capture 10–15 real messy trees with `git carve eval capture --note "..."`. It is the
  only evidence about real use, and the judge verdict stays provisional without it.
- **D43:** should `--no-new-errors` turn on automatically when HEAD already fails the check?
- **Agent-written messages:** see planned item 3 (D46).
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
| npm packaging (no install scripts) and the release workflow | D39 | publishing waits for my go-ahead |
| Viability research (five workstreams) | 2026-10-07 | |
| Branch verification: `git carve verify` (range, pre-push, CI, `--no-new-errors`, names the commit that introduced each error) | D44, 2026-10-07 | 8 tests; a cached re-run takes 0.26 s on the demo |
| Languages beyond TypeScript: keyword-based reading for 20+ languages, default checks for Rust, Go, and Python, files the check does not read reported as unverified | D45, 2026-10-08 | polyglot BCubed 0.764 → 0.783, better in all 5 repos; TypeScript unchanged (0.803) |
