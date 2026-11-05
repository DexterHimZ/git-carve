# Design: the agent gate pack and the speed gate (D46 phases 3–4)

Status: **reviewed, queued after phase 2.** Architecture and plan reviewed (2026-10-13);

## Verified hook APIs (2026-10-13)

- **Claude Code** (code.claude.com/docs/en/hooks):
  - `PreToolUse` with `matcher: "Bash"` and the handler field `if: "Bash(git push *)"`, which
    is best-effort, so the hook re-inspects the command itself;
  - stdin gives `tool_input.command` and `cwd`;
  - deny with exit 2 (stderr is the reason) or with
    `hookSpecificOutput.permissionDecision: "deny"`;
  - plugins hold `.claude-plugin/plugin.json`, `hooks/hooks.json` and `skills/<name>/SKILL.md`.
- **Cursor** (cursor.com/docs/agent/hooks):
  - `.cursor/hooks.json` with `{version:1, hooks:{beforeShellExecution:[{command}]}}`;
  - stdin gives `command` and `cwd`;
  - output `{permission: allow|deny|ask, user_message, agent_message}`; exit 2 denies.
- **Codex CLI** (developers.openai.com/codex/hooks):
  - `.codex/hooks.json` or `[hooks]` in `config.toml`;
  - the `PreToolUse` matcher is a regex on the tool name, with the command in
    `tool_input.command`;
  - deny with exit 2 or with `hookSpecificOutput.permissionDecision: "deny"`.
- **Aider:** no hook API. It auto-commits.

## Phase 3 decisions

- **Enforcement** is git's own `.git/hooks/pre-push` running `git carve verify --pre-push`.
  It covers every agent, every human, and Aider.
- **Agent hooks are a courtesy layer.** They give an early, structured refusal with the fix
  (`git carve @{u}`), and they close the bypass: they deny `git push --no-verify` and
  `-c core.hooksPath=…`.
  - The gate fails open on carve errors; pre-push is what fails closed.
  - No Stop hooks: they loop and are non-deterministic.
- **The message ruling (D49):**
  - carve never writes message text, trailers included.
  - Default recipe: the agent drafts proposals as `# proposed:` comment lines in the plan
    file, and the human runs `git carve @{u} --plan-file plan.txt --edit`. The parser drops
    comments, so a proposal becomes a message only through a human keystroke, and an
    untouched file aborts with "empty message".
  - Opt-in agent messages: `git config carve.agentMessages true` lets the agent write the message
    lines itself.
  - Either way it is auditable: `UndoRecord.plan_source` is `editor` or `file`, the reflog
    gets a `(plan file)` suffix, and the summary says so.
- **Packaging (D50):**
  - `git carve hooks install` writes only `.git/hooks/pre-push`. It refuses to overwrite a
    foreign hook, and prints instead of writing when `core.hooksPath` is set.
  - `git carve hooks print claude|cursor|codex|git` prints snippets.
  - `git carve hooks run claude|cursor|codex` is the gate itself (`src/gate.rs`).
  - A `SKILL.md` carrying the D49 recipe lives in the repo (`agent/claude-code/`). The hook
    part is `hooks print claude`. This replaces the earlier plugin directory, per the
    2026-10-14 review below.
  - carve never writes `.claude/`, `.cursor/`, `.codex/` or `.husky/` files.
- **JSON contract:**
  - `verify --json` gets `version: 1`;
  - `--plan-only --json` gets `buffer` (the exact text `--plan-only` prints);
  - exit codes and fields stay stable; changes within version 1 are additive only;
  - documented in SPEC §11.

## Phase 4 decisions

- **Today:** checks run serially in one scratch worktree; tsc `--incremental` and a shared
  `CARGO_TARGET_DIR` already exist.
- **The lever is parallelism:**
  - `Checker::check_many(trees)` over worker worktrees `carve/w<k>/{scratch,tsbuildinfo}`;
  - jobs > 1 only for the default TypeScript check, or explicit checks with
    `carve.checkJobs`;
  - cargo stays at 1 job (target-dir lock);
  - base and final run in parallel; the baseline is applied after.
- **Fix:** tsbuildinfo files are named by the project path, not its index; a reorder
  currently discards the build info.
- **Pre-registered benchmark:**
  - instrument first (`wall_ms`, tier-B `check_wall_ms`, `final_run_ms`, the ratio);
  - run jobs = 1, 2 and 3 on the tune split (never test), on `train-manifest-v2`, and on the
    demo;
  - accept when the median ratio is ≤ 2.0 and p90 ≤ 3.0, with **zero** plan differences
    between jobs = 1 and jobs = k;
  - a demo re-verify must take ≤ 0.5 s.
- **Correctness guards:**
  - warm incremental state never hides a failing prefix;
  - unchanged dependents still report errors;
  - workers don't share tsbuildinfo.
- **Order note:** without parallel `verify`, the gate's first run on a long agent branch is
  slow. Build phase 4's `check_many` for `branch.rs` first if the phases run together.

## Changes after the phase 2 result review (2026-10-14)

1. **Done before phase 3:** findings 1 and 2 of the review (DECISIONS.md D48). The trunk is
   never the top branch, and B5 looks up each dependency inside its own section.
   Phase 3's `git carve @{u}` recipe would have made the first one common.
2. **D49 must be enforced by code.** `--plan-file` without `--edit` refuses message lines
   that are not empty, unless `carve.agentMessages` is set, and `plan_source` records which
   path ran. This goes in phase 3, next to `--edit`.
3. **A cheaper lever before `check_many`:**
   - `verify --pre-push` stops at the first failing commit, because the gate needs a
     verdict, not a full report;
   - the full report stays the default for `verify` without `--pre-push`;
   - the check cache is shared with carve, so the gate is slow only on un-carved agent
     pushes, and there failing fast usually ends early.

   The phase 4 benchmark stays as written.
4. **Order:**
   1. fail-fast `--pre-push`;
   2. `check_many` for `branch.rs`, with the benchmark;
   3. the phase 3 gate, hooks and JSON contract;
   4. `--edit` with D49 enforcement;
   5. `SKILL.md`.

## Cut (anti-LARP)

- Stop hooks;
- writing agent config files;
- Cursor and Codex plugins beyond printed snippets;
- an MCP server;
- incremental work for cargo and python.
