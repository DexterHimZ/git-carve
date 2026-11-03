# The launch recording (SPEC §14.2), for VHS (github.com/charmbracelet/vhs): `vhs docs/demo.tape`.
# Needs vhs, vim, node, and TypeScript (as for scripts/make-demo-repo.sh --typescript).
# Untested draft (M7): VHS is not installed on the development machine. Every command in it
# was run by hand on 2026-10-08.

Output docs/demo.gif
Set Shell bash
Set FontSize 18
Set Width 1280
Set Height 760
Set TypingSpeed 35ms
Set Padding 24
Env GIT_EDITOR "vim -u NONE -N"

Hide
Type "./scripts/make-demo-repo.sh /tmp/carve-demo --typescript >/dev/null 2>&1 && cd /tmp/carve-demo && clear" Enter
Sleep 10s
Show

# 1. The mess.
Type "git status --short" Enter
Sleep 2s

# 2. One command; the editor opens once with a checked plan.
Type "git carve" Enter
Sleep 6s

# 3. Three one-line messages: the user's words, typed by the user.
Type "/^commit$" Enter
Type "ji" Sleep 200ms
Type "Rename fmtBytes to formatBytes" Escape
Type "n" Type "ji" Sleep 200ms
Type "Retry failed fetches with backoff" Escape
Type "n" Type "ji" Sleep 200ms
Type "Fix off-by-one in parseHeader" Escape
Sleep 500ms
Type ":wq" Enter

# 4. The summary.
Sleep 3s

# 5. The history.
Type "git log --oneline -3" Enter
Sleep 2s

# 6. Proof: every commit type-checks on its own. (Not `git rebase -x`: it refuses to run while
# the two debug lines are still uncommitted. verify never touches the working tree, and its
# results come from the cache carve just filled.)
Type "git carve verify HEAD~3..HEAD" Enter
Sleep 3s

# End card.
Hide
Type "clear" Enter
Show
Type "# Splits your mess into commits that each pass your check. Refuses to write your commit messages."
Sleep 4s
