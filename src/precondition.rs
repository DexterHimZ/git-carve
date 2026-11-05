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

/// One commit of a re-carved range, oldest first (for the plan header and `--json`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RangeCommit {
    pub oid: String,
    pub subject: String,
    pub author: String,
}

/// The base a range carve builds on (D46 phase 1, docs/design/carve-range.md §3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedBase {
    /// The commit new commits are parented on: an ancestor of HEAD.
    pub oid: String,
    /// `<base>` was not an ancestor of HEAD, so its merge-base with HEAD is used.
    pub merge_based: bool,
    /// The commits in `oid..HEAD`, oldest first; empty for a plain carve.
    pub commits: Vec<RangeCommit>,
    /// Distinct trailers (`Co-authored-by:`, `Signed-off-by:`, ...) from the old messages,
    /// shown as comments for the user to copy. carve copies none of them itself.
    pub trailers: Vec<String>,
}

/// Resolves `git carve <base>` and refuses ranges carve must not rewrite: merges, commits
/// already on a remote (unless `rewrite_pushed`), and any tip but HEAD.
pub fn resolve_base(repo: &Repo, expr: &str, rewrite_pushed: bool) -> Result<ResolvedBase> {
    let head = repo
        .git
        .try_run_str(["rev-parse", "--verify", "-q", "HEAD^{commit}"])?
        .ok_or_else(|| precondition("HEAD has no commits yet", None))?;
    let (from, tip) = match expr.split_once("..") {
        Some((from, tip)) => (from, tip),
        None => (expr, ""),
    };
    if !tip.is_empty() && tip != "HEAD" {
        let resolved =
            repo.git
                .try_run_str(["rev-parse", "--verify", "-q", &format!("{tip}^{{commit}}")])?;
        if resolved.as_deref() != Some(head.as_str()) {
            return Err(precondition(
                format!("`{expr}` ends at {tip}, but carve rewrites the current branch only"),
                Some(&format!(
                    "check out {tip} first, or carve `{from}` from HEAD"
                )),
            ));
        }
    }
    let from = if from.is_empty() { "HEAD" } else { from };
    let given = repo
        .git
        .try_run_str(["rev-parse", "--verify", "-q", &format!("{from}^{{commit}}")])?
        .ok_or_else(|| precondition(format!("unknown revision `{from}`"), None))?;
    let (oid, merge_based) = if repo
        .git
        .test(["merge-base", "--is-ancestor", &given, &head])?
    {
        (given, false)
    } else {
        let bases = repo
            .git
            .try_run_str(["merge-base", "--all", &given, &head])?
            .unwrap_or_default();
        let bases: Vec<&str> = bases.lines().collect();
        match bases[..] {
            [one] => (one.to_owned(), true),
            [] if repo.git.run_str(["rev-parse", "--is-shallow-repository"])? == "true" => {
                return Err(precondition(
                    format!("`{from}` is not reachable from HEAD (shallow clone)"),
                    Some("`git fetch --unshallow` fetches the missing history"),
                ));
            }
            [] => {
                return Err(precondition(
                    format!("`{from}` and HEAD share no history"),
                    None,
                ));
            }
            _ => {
                return Err(precondition(
                    format!("`{from}` and HEAD have several merge bases"),
                    Some("name the base commit explicitly"),
                ));
            }
        }
    };
    if oid == head {
        return Ok(ResolvedBase {
            oid,
            merge_based,
            commits: Vec::new(),
            trailers: Vec::new(),
        });
    }
    let range = format!("{oid}..{head}");
    let merges = repo
        .git
        .run_str(["rev-list", "--merges", "--count", &range])?;
    if merges != "0" {
        return Err(precondition(
            format!(
                "{} has a merge commit; carve needs a linear range",
                short_range(&oid)
            ),
            Some("carve from a base after the merge"),
        ));
    }
    let total: usize = repo
        .git
        .run_str(["rev-list", "--count", &range])?
        .parse()
        .unwrap_or(0);
    let unpushed: usize = repo
        .git
        .run_str(["rev-list", "--count", &range, "--not", "--remotes"])?
        .parse()
        .unwrap_or(0);
    if unpushed < total && !rewrite_pushed {
        let oldest = repo
            .git
            .run_str(["rev-list", "--reverse", &range])?
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned();
        let remote = repo
            .git
            .run_str([
                "branch",
                "-r",
                "--contains",
                &oldest,
                "--format=%(refname:short)",
            ])?
            .lines()
            .next()
            .unwrap_or("a remote")
            .to_owned();
        return Err(precondition(
            format!(
                "{} of the {total} commits since {} are already on {remote}; rewriting them means a force-push",
                total - unpushed,
                &oid[..oid.len().min(7)]
            ),
            Some("carve only what is unpushed with `git carve @{u}`, or pass --rewrite-pushed"),
        ));
    }
    let log = repo
        .git
        .run_str(["log", "--reverse", "--format=%H%x1f%s%x1f%an <%ae>", &range])?;
    let commits = log
        .lines()
        .filter_map(|line| {
            let mut f = line.splitn(3, '\u{1f}');
            Some(RangeCommit {
                oid: f.next()?.to_owned(),
                subject: f.next()?.to_owned(),
                author: f.next()?.to_owned(),
            })
        })
        .collect();
    let mut trailers: Vec<String> = Vec::new();
    for line in repo
        .git
        .run_str([
            "log",
            "--reverse",
            "--format=%(trailers:only,unfold)",
            &range,
        ])?
        .lines()
    {
        let line = line.trim();
        if !line.is_empty() && !trailers.iter().any(|t| t == line) {
            trailers.push(line.to_owned());
        }
    }
    Ok(ResolvedBase {
        oid,
        merge_based,
        commits,
        trailers,
    })
}

fn short_range(oid: &str) -> String {
    format!("{}..HEAD", &oid[..oid.len().min(7)])
}
