---
name: git-carve
description: Use before pushing a branch, when a push is denied because a commit fails the check, or to turn a big branch into a stack of reviewable PRs. git carve re-splits unpushed work into commits (or stacked branches) that each pass the project's check (tsc, cargo check, go build, ...), deterministically and with exact undo.
---

# git carve: a branch into commits or PRs that each pass the check

`git carve <base>` takes a branch's unpushed commits, plus any uncommitted changes, and re-splits
them into commits that each pass the project's check. carve verifies every one before anything
moves. It never invents message text. The messages come from the plan you hand it, or from the
user's editor.

## Before you push

```sh
git carve verify --fail-fast        # every unpushed commit, oldest first; exit 3 if one fails
```

If it passes, push as usual. If a push is denied by the git carve hook, the reason names the
failing commit and its first errors.

**Never bypass the check.** Do not use `git push --no-verify`, `-c core.hooksPath=…`, or
anything else that skips the hook.

Errors the base already had do not count; only new ones do (`--strict` counts them too).

## Re-split so each commit passes (or make a stack)

1. Get the plan carve verified:

   ```sh
   dir="$(git rev-parse --git-path carve)"; mkdir -p "$dir"
   git carve @{u} --plan-only --json > "$dir/plan.json"
   ```

   For a stack, pass the same flags to both steps: `git carve main --stack --plan-only --json`,
   then `git carve main --stack --plan-file …`.

   Keep files inside `.git`. A file in the working tree would itself be carved.

2. Take the `buffer` field from that JSON and save it as `"$dir/plan.txt"`. Write one message under
   each `commit` line: a subject, then optionally a blank line and a body. To merge two blocks,
   delete the second `commit` line; to move a change, move its `@` line. carve re-checks a
   changed grouping before committing. Lines starting with `#` are comments.

   ```
   commit
   Add retry with backoff to fetchAll

   Retries 3 times, doubling the delay.
     @ a8   src/net.ts:16-28  ...
   ```

3. Apply it:

   ```sh
   git carve @{u} --plan-file "$dir/plan.txt"            # add --stack for one branch per commit
   ```

   carve checks every commit before anything moves, records that the messages came from a
   plan file, and prints what it made. `git carve --undo` restores the previous state exactly.

If the repository sets `git config carve.requireHumanMessages true`, carve refuses filled
messages. Then write your suggestions as `# proposed: …` comment lines, leave the message lines
empty, and ask the user to run the same command with `--edit`, so they write the messages.

## For stacked pull requests

`git carve main --stack` puts each commit on its own branch, at most 5 layers by default
(`--layers N`). It prints the `gh stack` or `gh pr create` commands. Running it again after a
review moves the existing layer branches, so the PRs keep their numbers. Nothing is pushed unless
the user adds `--submit`.

## Useful details

- With no upstream yet, use the base branch instead of `@{u}`, for example `git carve main`.
- Commits already on a remote need `--rewrite-pushed`, and afterwards a force-push.
- `git carve verify --json` and `git carve --plan-only --json` carry `"version": 1`. Within a
  version, fields are only ever added.
