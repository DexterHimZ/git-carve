---
name: git-carve
description: Use before pushing commits, or when a push is denied because a commit fails the check. git carve verifies that every commit passes the project's check (tsc, cargo check, go build, ...) and re-splits unpushed work into commits that each pass. You may propose commit messages as comments; the user writes the real ones.
---

# git carve: every pushed commit passes the check

`git carve` turns a branch's unpushed commits, plus any uncommitted changes, into commits
that each pass the project's check. It never writes commit messages. The user does,
in one editor buffer.

## Before you push

```sh
git carve verify --fail-fast        # every unpushed commit, oldest first; exit 3 if one fails
```

If it passes, push as usual. If a push is denied by the git carve hook, the reason names
the failing commit and its first errors.

**Never bypass the check.** Do not use `git push --no-verify`, `-c core.hooksPath=…`,
or anything else that skips the hook.

## When a commit fails

Fix it one of two ways:

1. **The code is wrong:** fix it, then amend the failing commit or add a fixing commit.
2. **The split is wrong** (a commit uses something a later commit adds, or work-in-progress
   commits): have the user re-split the unpushed commits so each one passes. You prepare
   it, and they author it.

   ```sh
   git carve @{u} --plan-only --json > "$(git rev-parse --git-path carve)/plan.json"
   ```

   Take the `buffer` field from that JSON and save it as
   `"$(git rev-parse --git-path carve)/proposed-plan.txt"`. Keep the plan file inside
   `.git`: a file in the working tree would itself be carved.

   In that file, under each `commit` line, add your suggestion as **comment** lines:

   ```
   commit
   # proposed: Add retry with backoff to fetchAll
   # proposed: (body) Retries 3 times, doubling the delay.
     @ a8   src/net.ts:16-28  ...
   ```

   Leave the message lines themselves empty. carve refuses a plan file whose messages are
   filled, unless the repository opted in with `git config carve.agentMessages true`.
   Then ask the user to run:

   ```sh
   git carve @{u} --plan-file "$(git rev-parse --git-path carve)/proposed-plan.txt" --edit
   ```

   Their editor opens on your plan. They write (or copy) each message, and every commit is
   checked before anything moves. `git carve --undo` restores the previous state exactly.

## For stacked pull requests

`git carve main --stack` does the same, and puts each commit on its own branch for stacked
PRs. It prints the `gh stack` or `gh pr create` commands. Nothing is pushed unless the
user adds `--submit`.

## Useful details

- With no upstream yet, use the base branch instead of `@{u}`, for example `git carve main`.
- When the base already fails the check, `--no-new-errors` checks only for new errors.
- `git carve verify --json` and `git carve --plan-only --json` carry `"version": 1`. Within
  a version, fields are only ever added.
