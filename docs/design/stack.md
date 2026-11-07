# Design: `git carve <base> --stack` (D46 phase 2)

Status: **reviewed, executing.** Written from the code and reviewed on 2026-10-13. Final
decisions go to DECISIONS.md D48 and SPEC §10.6.

## Verified facts about `gh stack` (2026-10-13)

Sources: GitHub docs (stacked PRs CLI reference, quickstart), the changelog of 2026-10-06,
and the github/gh-stack README, `skills/gh-stack/SKILL.md` and `cmd/{init,link,submit}.go`.

- **Availability:** GA on 2026-10-06. Needs `gh` 2.90+ and `gh extension install
  github/gh-stack`. Exit 9 means stacked PRs are not enabled for the repository.
- **`gh stack init [--base T] b1 … bn`:** adopts existing branches as they are (no ancestry
  check). It also:
  - checks out the last branch;
  - enables `rerere`;
  - keeps its state in `<common-dir>/gh-stack`;
  - pushes nothing.
- **`gh stack submit --auto`:**
  - the PR title is the commit subject **only when a branch has exactly one commit over its
    base**, otherwise a humanized branch name;
  - the body is the repository's PR template, or the commit body plus a gh-stack footer;
  - new PRs are drafts unless `--open`;
  - it force-pushes branch by branch;
  - it is idempotent.
- **There is no `split`** (issue #508). GitHub's own skill says to plan the stack first. That
  is the gap carve fills.

## Decisions

- **One commit per layer**, so the human's subject is the PR title. The `stack` marker and
  multi-commit layers are cut.
- **Layers:** layers 1..N−1 are new branches at carve's commits 1..N−1. Layer N is the
  current branch, which the existing compare-and-swap already moves. N = 1 creates nothing.
- **Names:** `<branch>-<k>-<slug>`.
  - The slug comes from the human's subject: lowercase ASCII alphanumerics, everything else
    collapsed to `-`, at most 32 characters. With an empty slug the name is `<branch>-<k>`.
  - Never `/` (a ref can't be both a file and a directory).
  - Checked with `git check-ref-format --branch`.
- **CLI:**
  - `--stack` requires `<base>`;
  - `--submit` requires `--stack` and conflicts with `--no-check` and `--plan-only`;
  - no `--open`, `--draft`, tool or prefix flags.
- **Refusals:**
  - detached HEAD ("a stack needs a branch");
  - existing `<branch>-<k>` / `<branch>-<k>-*` branches, checked before the editor opens,
    with a hint to use `gh stack modify|rebase` or `git branch -D` to start over;
  - an exact name collision after editing: nothing is committed, and the messages are kept.
- **Refs:** between the race guard and the main CAS, `update-ref refs/heads/<name>
  <commit_k> <null_oid>` (it must not exist). If the main CAS fails, those refs are deleted
  again. `UndoRecord.stack` lists them (with `serde(default)`, so older records still
  parse). Undo restores the main ref, then deletes each layer that still points where carve
  left it, and lists any that moved. The reflog says "carve: stack layer k of N".
- **"Every layer passed the check":** a layer's tree is prefix tree k, which `verify`
  already checked. There are no new check runs. With `--no-check` the branches are still
  created and marked unverified, and `--submit` is refused.
- **Trunk:** `<base>` if it is a local branch; `origin/x` or `@{u}` becomes `x`; a commit
  resolves to the unique branch whose tip equals the base. Otherwise the trunk is unknown,
  and the printed commands show a placeholder.
- **Remote:** `remote.pushDefault`, else `origin`.
- **Output:**
  - **Default (print only):** the layer list and `gh stack init --base T l1 … top && gh
    stack submit --auto` when the extension is installed. Otherwise `git push -u` plus
    `gh pr create --base … --head … --fill`.
  - **Divergence:** if the top branch's upstream diverged, a warning that the submit
    force-pushes it.
  - **`--submit`:** a local preflight (gh on PATH, the extension, a remote, a known trunk)
    before the snapshot. After the commits it runs `gh stack init` then `gh stack submit
    --auto`. On failure it exits **6**, "submission failed; local commits and branches are
    complete", with the retry commands.
- **No PR text from carve:** carve writes no `--title` or `--body`. Titles come from the
  human's subjects; bodies from gh (template or commit body).
- **JSON:**
  - `--plan-only --json` adds `stack {trunk, remote, tool, layers:[{k, prefix}]}`;
  - a full run with `--json` prints a run report: `{version, commits:[{oid, subject}],
    stack:{trunk, remote, tool, layers:[{k, branch, oid, subject}], submit:{ran, ok,
    commands}}}`.
- **Manifest binding (soundness; ships in this phase).** A new hard bind next to B2: an atom
  whose added lines import a package that the changed manifest adds binds to the manifest
  atom containing that dependency line. Removed dependencies work the same way.
  - Covers npm (`package.json` dependency maps), Cargo (`[*dependencies]` sections; `-` is
    read as `_`) and go.mod (`require`).
  - Python gets only the existing note (package and module names differ).
  - It is sound, not complete.

## Execution plan (each step: test, clippy on 1.93 and 1.99, fmt, safety)

1. `src/stack.rs`, pure functions: slug, layer names, trunk.
2. Refs and undo: `StackRef`, `UndoRecord.stack`, create between guard and CAS, clean up
   when the CAS fails, undo deletes the layers that didn't move.
3. Collisions and preconditions.
4. Summary, printed commands, trunk, remote, tool detection, divergence warning.
5. `--submit` with a stub `gh`, and exit 6.
6. JSON: the plan stack block and the run report.
7. Manifest binding (npm, Cargo, go), plus `eval range-check` and the tune BCubed delta.
8. Docs (SPEC §10.6, §10.4, §11 and exit 6, §6.3 B3, §13; DECISIONS D48; README "From a
   branch to a stack" with a Graphite recipe; ROADMAP), the try.sh scene, and the
   queue batch with a rehearsal.

**What makes people stop using it:** slow checks (phase 4), branch litter (`gh stack sync
--prune`), no way to fold review fixes into a layer (deferred verified absorb), and red layers
from the dependency gap (step 7).
