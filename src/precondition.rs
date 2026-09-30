//! Refuse unsupported repository states before doing anything (SPEC §5.1). Checks that
//! need `git status` output live in `snapshot`.

use crate::atom::Changes;
use crate::error::{Result, precondition};
use crate::repo::Repo;

/// Upper bound on changed paths (§5.1).
pub const MAX_PATHS: usize = 200;
const DEFAULT_MAX_ATOMS: u64 = 400;

pub fn check_state(repo: &Repo) -> Result<()> {
    if repo
        .git
        .try_run_str(["rev-parse", "--verify", "-q", "HEAD^{commit}"])?
        .is_none()
    {
        return Err(precondition(
            "HEAD has no commits yet",
            Some("make a first commit, then carve the rest"),
        ));
    }

    let markers = [
        ("MERGE_HEAD", "merge"),
        ("CHERRY_PICK_HEAD", "cherry-pick"),
        ("REVERT_HEAD", "revert"),
        ("rebase-merge", "rebase"),
        ("rebase-apply", "rebase"),
        ("BISECT_LOG", "bisect"),
    ];
    let mut args = vec!["rev-parse"];
    for (name, _) in markers {
        args.extend(["--git-path", name]);
    }
    let paths = repo.git.run_str(args)?;
    for ((_, operation), rel) in markers.iter().zip(paths.lines()) {
        if repo.toplevel.join(rel).exists() {
            return Err(precondition(
                format!("a {operation} is in progress"),
                Some(&format!("finish it or run `git {operation} --abort` first")),
            ));
        }
    }

    if repo.config_bool("core.sparseCheckout", false)? {
        return Err(precondition("sparse checkouts are not supported yet", None));
    }

    if !repo.git.run(["ls-files", "-u", "-z"])?.is_empty() {
        return Err(precondition(
            "the index has unmerged entries",
            Some("resolve the conflicts first"),
        ));
    }

    if !repo.git.test(["diff", "--cached", "--quiet"])? {
        return Err(staged_changes());
    }
    Ok(())
}

/// Runs after atomization, when the atom count is known.
pub fn check_size(repo: &Repo, changes: &Changes) -> Result<()> {
    let max = repo.config_u64("carve.maxAtoms", DEFAULT_MAX_ATOMS)?;
    if changes.atoms.len() as u64 > max {
        return Err(precondition(
            format!(
                "{} changed regions is more than carve.maxAtoms ({max})",
                changes.atoms.len()
            ),
            Some("commit part of the work first, or raise carve.maxAtoms"),
        ));
    }
    Ok(())
}

pub fn staged_changes() -> crate::error::Error {
    precondition(
        "you have staged changes; git carve works on unstaged changes only",
        Some("`git reset` unstages everything and keeps all changes in your working tree"),
    )
}
