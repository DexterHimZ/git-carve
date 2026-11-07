# Design: `git carve <base>`, re-carve committed work (D46 phase 1)

Status: **reviewed, ready to execute.** Drafted from the code, then reviewed with an
execution plan, 2026-10-13. Once it ships, the final decisions go
into DECISIONS.md D47 and the SPEC sections listed in step 9.

## 1. What and why

carve only splits **uncommitted** changes, but in 2026 the mess is usually committed already:
Aider commits every edit, agents commit when asked, humans commit "wip". `git carve <base>`
re-carves every change since `<base>` on the current branch (the commits in `<base>..HEAD`
plus any uncommitted changes) into commits that each pass the check. It's the engine for
`--stack` (phase 2).

**Guarantees:**
- **Byte-identical result:** the new tip's tree plus the left atoms equals the old working
  tree.
- **No working-tree writes:** working-tree files are never written.
- **Exact undo:** `--undo` CAS-restores the old tip.
- **No messages:** carve writes none. Old subjects appear only as `#` comments.

## 2. Core change

Split `Snapshot.base` (trees and parents) from `Snapshot.head` (CAS, guard, undo). Plain
`git carve` has base == head.

| Site | Range mode |
|---|---|
| `atom.rs:366,379` rename TreeBuilder + `diff-tree -M` | base.tree |
| `commit.rs:61` `prefix_trees`; `commit.rs:64` first parent | base.tree / base.commit |
| `commit.rs:97,108` CAS old value; `commit.rs:122` `UndoRecord.old` | head.commit (unchanged) |
| `commit.rs:83` `changed_since` | re-take with the same **resolved** base oid; compare base and head |
| `verify.rs:93,120,174,275,625` `prefix_trees`; `verify.rs:96` step 0 / D43 baseline | base.tree |
| `eval/real.rs:98`, `tests/common`, `tests/safety.rs:245` `take()` | unchanged |

**`take_since(repo, base)`:**
- **The changed paths:** one `git diff-tree -r --raw --no-renames -z B HEAD` (it carries the
  base and HEAD modes and oids), unioned with today's `git status`.
- **`base`:** B's entry.
- **`new`:** the hashed worktree content if status lists the path, otherwise HEAD's entry.
- **Hashing:** only dirty paths are hashed or `read_link`ed. That avoids `core.fileMode`,
  `autocrlf` and clean filters for committed content. Clean paths get `symlink_metadata` for
  the guard's stat only.
- **`in_head`:** a new `FileSnap.in_head`, true when the path is in HEAD's tree.

## 3. Decisions (the reviewer's answers to the open questions)

1. **Syntax.**
   - Positional `<base>`; `<base>..` and `<base>..HEAD` are accepted by stripping. `A..B` with
     B ≠ HEAD is refused: "carve rewrites the current branch only".
   - If `<base>` is not an ancestor of HEAD but has a unique merge-base, carve **uses the
     merge-base and prints one notice**. It refuses on no merge-base, or on several.
   - The docs suggest `git carve @{u}` (everything unpushed), but it is never the default.
2. **Per-block attribution** (`#   from: 57468c8 wip (12 lines), 3effcd7 fix stuff (3
   lines)`).
   - It comes in step 7, in the same release, for display only.
   - It is computed with `git blame -l --line-porcelain B..HEAD -- path` on HEAD's blob, with
     worktree lines mapped through the HEAD→worktree diff. Whole-file atoms list the commits
     that touched the path.
3. **Authorship:** no `--rewrite-others` flag. Agent commits are often authored as the agent.
   The distinct old authors are listed in the header; the pushed check covers shared
   branches.
4. **Author dates:** not kept. The old and new commits map many-to-many, so these are new
   commits.
5. **Pushed detection:** any remote-tracking ref.
   - `rev-list --count B..HEAD --not --remotes` vs `rev-list --count B..HEAD`.
   - The refusal names the remote branch (`git branch -r --contains <oldest>`).
   - The override is `--rewrite-pushed`.
6. **The `--no-new-errors` baseline is B's tree,** as in `git carve verify`. The wording
   changes from "HEAD already had" to "the base already had".
7. **An empty range with a dirty tree** is plain carve, with no header.
8. **A net-empty range** (`B..HEAD` non-empty, no diff, clean tree) is refused: "changes
   nothing net; `git reset --soft <base>` is what you want".

## 4. Missed by the draft, now in scope

- **The large-file bug.** `leave_by_default` (`atom.rs:226-229`) would leave a *committed*
  large or binary file out of the new history. Apply it only when `!in_head`, i.e. truly
  untracked.
- **Submodule added in the range** (mode 160000): refuse as a precondition. Today it would hit
  `Error::Internal` at `repo.rs:151`.
- **`verify.rs:223 head_fails`:** the wording should say the base fails the check.
- **Dependency soundness gap.** A range that adds a package checks every prefix against the
  final `node_modules`. Print a note when a manifest or lockfile is among the changed files.
  The real fix is "manifest binding" (later).
- **`report.rs` JSON:** add `range: {base, head, merge_based, commits: [{oid, subject,
  author}]}` (additive, `version: 1`). Phases 2 and 3 consume it.
- **The MAX_PATHS hint** in range mode: "carve a shorter range, for example
  `git carve HEAD~k`".
- **"Nothing to carve"** vs the net-empty refusal: two distinct messages.
- **The reflog line:** "carve: re-carved 6 since 2d8a3eb into 3".
- **The summary:** the "Re-carved N commits" line, plus a line counting hunks that were
  committed in B..T and are now uncommitted.
- **The check config must exist at the base** (a tsconfig created inside the range fails
  step 0). Document it.
- **No commit-count limit:** MAX_PATHS (200) and `carve.maxAtoms` (400) already bound the
  work. The header lists at most 20 subjects, then "and N more"; empty commits show
  "(empty)".
- **Shallow clone with B past the boundary:** a precondition, "base is not reachable
  (shallow clone)".
- **Documentation only:**
  - old signatures are gone;
  - `post-rewrite` doesn't run;
  - a `.gitattributes` change inside the range affects earlier content;
  - case-only renames on case-insensitive filesystems are a pre-existing scratch-worktree
    limit.

## 5. Execution plan

Every step must leave `cargo test`, `cargo clippy --all-targets -- -D warnings` (on stable
1.99 too), `cargo fmt --check` and `cargo test safety_` green.

1. **Split without changing behavior.**
   - `Base { commit, tree }` and `Snapshot.base`; `take()` sets base = head.
   - `take_since(repo, None)` delegates to `take()`.
   - The table's sites switch to `snap.base`.
   - Test: `snapshot_plain_base_equals_head`. No other test changes.
2. **`take_since` for real:** the union snapshot, `in_head`, the 160000 refusal, the
   net-empty refusal, the MAX_PATHS hint, and the guard re-take. Also `leave_by_default`
   gated on `!in_head`. New tests in `tests/range.rs`:
   - `range_unions_committed_and_dirty_paths`
   - `range_dirty_file_also_changed_in_range_has_base_from_B`
   - `range_added_then_deleted_file_is_absent`
   - `range_mode_only_change_is_a_mode_atom`
   - `range_symlink_in_range_is_whole_atom`
   - `range_refuses_submodule_added_in_range`
   - `range_net_empty_is_refused`
   - `range_committed_large_binary_is_not_left_by_default`
   - `range_deleted_then_untracked_recreation`
3. **Preconditions:** `resolve_base(repo, expr, rewrite_pushed) -> ResolvedBase { oid,
   merge_based, commits }` covering ancestry or merge-base, merges, pushed commits, and the
   `A..B` rules. New tests, each also asserting that HEAD and the worktree are unchanged:
   - `range_refuses_merge_in_range`
   - `range_refuses_pushed_commits_without_flag`
   - `range_uses_merge_base_when_base_diverged`
   - `range_refuses_unknown_base`
   - `range_refuses_tip_other_than_head`
   - `range_rewrite_pushed_flag_allows`
4. **Wiring:**
   - the CLI positional `<base>` and `--rewrite-pushed`;
   - `Options.base`, `Summary.range`;
   - the commit parent from the base, the reflog text, the summary line.

   New tests:
   - `range_three_wip_commits_become_verified_commits`
   - `range_tip_tree_plus_left_equals_old_tree`
   - `range_worktree_mtimes_untouched`
   - `range_undo_restores_old_tip_index_and_status`
   - `range_committed_debug_lines_end_up_uncommitted`
   - `range_plus_dirty_changes`
   - `range_deleted_file_in_range`
   - `range_rename_across_range_is_bound`
   - `range_detached_head`
   - safety: `safety_range_cas_race_aborts_without_moving_the_ref`
   - safety: `safety_range_worktree_edit_during_carve_aborts`
   - safety: `safety_range_undo_is_exact`
5. **The buffer header, JSON `range` and wording:**
   - an insta snapshot `range_buffer` (review the `.snap` by hand);
   - `range_header_lines_are_ignored_by_the_parser`;
   - `range_plan_only_json_has_range_block`.
6. **Property test:** `safety_range_random_histories_reconstruct_exactly`. It generates random
   base files, 1–4 random commits (deletes and re-adds), optional dirty edits, a random base
   in the chain, and a random partition with leave. It asserts:
   - the worktree digest is unchanged;
   - `HEAD~n == B`;
   - the tree matches the oracle, modulo left files;
   - `--undo` restores exactly.
7. **Attribution** (`# from:`): test `range_attribution_names_the_old_commit_per_block`, plus
   a snapshot update.
8. **Eval equivalence:**
   - `range_cli_equals_eval_materialization`: the `materialize` + `prepare` path vs
     `git carve <base> --plan-only --json --no-check` on a synthetic 3-commit window, with
     atoms, groups, leave and edges byte-equal.
   - A hidden `eval range-check --corpus eval/corpus/train-manifest-v2.json --limit 50` on
     real dev windows. It reports mismatches (**must be 0**), `plan_ms` for both paths, and
     refusal reasons. Record the results in `eval/RESULTS.md`.
9. **Docs:**
   - SPEC §5.1, §5.2, §9.2, §10.1–10.2, §10.5, §11, §13;
   - DECISIONS D47;
   - README ("what rewriting history means here" with the three guarantees);
   - ROADMAP;
   - a range scene in `scripts/try.sh`.

## 6. Quality bar

- **Correctness:**
  - the property test is green;
  - equivalence shows 0/50 mismatches on dev windows;
  - on 3 of my real agent branches, the invariant (new tip tree + left atoms == old
    tip tree) holds, checked with `--plan-only` plus one committed run and `--undo`.
- **Latency:** range-mode `plan_ms` is within 10% of plain mode on the same (B, worktree);
  check runs stay N+1 plus the probe; the dependency fingerprint doesn't invalidate the cache.
- **Grouping quality:** byte-identical plans mean range mode inherits the locked numbers (test
  BCubed 0.743 code-only). Say so; don't re-measure on test.
- **Failure modes:** refusals counted by reason over the dev windows. Internal errors must be
  0.

**Experiment (dev/tune only):** are old commit boundaries a grouping signal? On
`train-manifest-v2`, re-slice each window's net diff into synthetic wip histories:
- (a) random hunk partitions;
- (b) time-sliced contiguous prefixes in file order (the realistic agent case);
- (c) the true commits (an upper bound).

Pre-register: a co-membership edge F8 ships only if (b) gains ≥ 0.02 BCubed and (a) loses
≤ 0.01. Step 7's attribution is the instrument.

## 7. Feature additions (reviewer's suggestions, scheduled)

| Addition | Phase | Effort |
|---|---|---|
| **Before/after line** in the summary: "6 wip commits → 3 commits that each pass tsc" | 1 | ~1 h |
| **Old trailers** (`Co-authored-by:`, `Signed-off-by:`) as `#` lines in the header, ready to copy | 1 | ~30 min |
| **`git carve verify` failure hint** → "re-carve with `git carve @{u}`" | 1 | ~15 min |
| **JSON `range` block with commits and attribution** (steps 5 and 7), the data `--stack` and agent hooks consume | 1 | — |
| **Manifest binding:** an atom importing a package that the range adds to `package.json` binds to the manifest atom. Closes the dependency gap; `--stack` needs it. | 2 | 1–2 days |
