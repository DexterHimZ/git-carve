//! `git carve <base> --stack` (D46 phase 2, DECISIONS.md D48, docs/design/stack.md): one
//! branch per carved commit, so each layer becomes one reviewable PR whose tree already
//! passed the check. carve creates local branches and prints (or, with `--submit`, runs) the
//! `gh stack` commands; it writes no PR text and never pushes by itself.

use crate::error::Result;
use crate::repo::Repo;

/// The most characters a layer name takes from the subject.
const SLUG_MAX: usize = 32;

/// `Add retry with backoff!` → `add-retry-with-backoff`: lowercase ASCII letters and digits,
/// everything else collapsed to one `-`, trimmed, at most [`SLUG_MAX`] characters.
pub fn slug(subject: &str) -> String {
    let mut out = String::new();
    for c in subject.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let mut out: String = out.trim_matches('-').chars().take(SLUG_MAX).collect();
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Branch names for layers 1..N-1 (the top layer is the current branch itself):
/// `<branch>-<k>-<slug>`, or `<branch>-<k>` when the subject has no usable characters.
/// Never `<branch>/<k>`: a ref cannot be both a file and a directory.
pub fn layer_names(branch: &str, subjects: &[String]) -> Vec<String> {
    subjects
        .iter()
        .take(subjects.len().saturating_sub(1))
        .enumerate()
        .map(|(i, subject)| match slug(subject) {
            s if s.is_empty() => format!("{branch}-{}", i + 1),
            s => format!("{branch}-{}-{s}", i + 1),
        })
        .collect()
}

/// The branch a stack's bottom layer targets, for `gh stack init --base`: the base
/// expression when it names a local branch, a remote-tracking branch without its remote
/// (`origin/main`, `@{u}` → `main`), else the one branch whose tip is the base commit.
/// `None` when that is ambiguous or unknown.
pub fn trunk_name(
    repo: &Repo,
    expr: &str,
    base_oid: &str,
    current: &str,
) -> Result<Option<String>> {
    let expr = expr
        .strip_suffix("..HEAD")
        .or_else(|| expr.strip_suffix(".."))
        .unwrap_or(expr);
    // A stack never targets its own top branch: `@{u}` on `feat` tracking `origin/feat` names
    // `feat` itself, so that answer falls through to the tip search (and then to `None`).
    if !expr.is_empty()
        && expr != current
        && repo
            .git
            .test(["show-ref", "--verify", "-q", &format!("refs/heads/{expr}")])?
    {
        return Ok(Some(expr.to_owned()));
    }
    let remotes: Vec<String> = repo
        .git
        .run_str(["remote"])?
        .lines()
        .map(str::to_owned)
        .collect();
    let strip = |name: &str| -> String {
        match name.split_once('/') {
            Some((remote, rest)) if remotes.iter().any(|r| r == remote) => rest.to_owned(),
            _ => name.to_owned(),
        }
    };
    if !expr.is_empty() {
        if let Some(full) = repo
            .git
            .try_run_str(["rev-parse", "--symbolic-full-name", expr])?
            .filter(|f| !f.is_empty())
        {
            let named = full
                .strip_prefix("refs/remotes/")
                .map(strip)
                .or_else(|| full.strip_prefix("refs/heads/").map(str::to_owned));
            if let Some(name) = named.filter(|n| n != current) {
                return Ok(Some(name));
            }
        }
    }
    let mut names: Vec<String> = repo
        .git
        .run_str([
            "for-each-ref",
            "--format=%(refname)",
            "--points-at",
            base_oid,
            "refs/heads",
            "refs/remotes",
        ])?
        .lines()
        .filter_map(|r| {
            r.strip_prefix("refs/heads/")
                .map(str::to_owned)
                .or_else(|| r.strip_prefix("refs/remotes/").map(strip))
        })
        .filter(|n| n != "HEAD" && !n.ends_with("/HEAD") && n != current)
        .collect();
    names.sort();
    names.dedup();
    Ok(match names.as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    })
}

/// Branches an earlier `--stack` left for this branch (`<branch>-<k>` or `<branch>-<k>-*`,
/// k in 1..=n). A re-carve adopts them ([`adopt`], D52) or refuses.
pub fn existing_layers(repo: &Repo, branch: &str, n: usize) -> Result<Vec<String>> {
    let listed = repo.git.run_str([
        "for-each-ref",
        "--format=%(refname:short)",
        &format!("refs/heads/{branch}-*"),
    ])?;
    Ok(listed
        .lines()
        .filter(|name| {
            let Some(rest) = name.strip_prefix(&format!("{branch}-")) else {
                return false;
            };
            let k: String = rest.chars().take_while(char::is_ascii_digit).collect();
            let after = &rest[k.len()..];
            k.parse::<usize>().is_ok_and(|k| (1..=n).contains(&k))
                && (after.is_empty() || after.starts_with('-'))
        })
        .map(str::to_owned)
        .collect())
}

/// A layer branch an earlier `--stack` left, adopted by a re-carve (D52).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Adopted {
    pub name: String,
    pub oid: String,
}

/// D52: the layers an earlier `--stack` left for `branch`, bottom first, when they still form
/// its stack: exactly one `<branch>-<k>[-*]` for each k in 1..=m, each tip a commit of
/// base..head, each strictly below the next. `Err` says what is wrong instead: carve never
/// guesses which branch a PR belongs to.
pub fn adopt(
    repo: &Repo,
    branch: &str,
    base: &str,
    head: &str,
) -> Result<std::result::Result<Vec<Adopted>, String>> {
    let mut by_k: Vec<(usize, String)> = existing_layers(repo, branch, usize::MAX)?
        .into_iter()
        .map(|name| {
            let k: String = name[branch.len() + 1..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            (k.parse().unwrap_or(0), name)
        })
        .collect();
    by_k.sort();
    // Moving a branch another worktree has checked out would leave that worktree's index
    // disagreeing with its HEAD.
    let checked_out: Vec<String> = repo
        .git
        .run_str(["worktree", "list", "--porcelain"])?
        .lines()
        .filter_map(|l| l.strip_prefix("branch refs/heads/"))
        .map(str::to_owned)
        .collect();
    if let Some((_, name)) = by_k.iter().find(|(_, n)| checked_out.contains(n)) {
        return Ok(Err(format!("{name} is checked out in another worktree")));
    }
    let mut out: Vec<Adopted> = Vec::with_capacity(by_k.len());
    for (i, (k, name)) in by_k.iter().enumerate() {
        if let Some((_, other)) = by_k[..i].iter().find(|(j, _)| j == k) {
            return Ok(Err(format!("{other} and {name} both claim layer {k}")));
        }
        if *k != i + 1 {
            return Ok(Err(format!(
                "layer {} is missing below {name}",
                out.len() + 1
            )));
        }
        let oid = repo
            .git
            .run_str(["rev-parse", &format!("refs/heads/{name}^{{commit}}")])?;
        let below = out.last().map_or(base, |a| a.oid.as_str());
        let inside = oid != below
            && repo
                .git
                .test(["merge-base", "--is-ancestor", below, &oid])?
            && repo.git.test(["merge-base", "--is-ancestor", &oid, head])?;
        if !inside || oid == head {
            let place = match out.last() {
                Some(a) => format!("above {}", a.name),
                None => "above the base".to_owned(),
            };
            return Ok(Err(format!(
                "{name} is not a commit of this branch {place} and below its top"
            )));
        }
        out.push(Adopted {
            name: name.clone(),
            oid,
        });
    }
    Ok(Ok(out))
}

/// How the stack gets submitted: the `gh stack` extension when installed, else plain
/// branches and one `gh pr create` per layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tool {
    GhStack,
    Plain,
}

/// `gh extension list` names `github/gh-stack`. A missing `gh` means [`Tool::Plain`].
pub fn detect_tool() -> Tool {
    let listed = std::process::Command::new("gh")
        .args(["extension", "list"])
        .stdin(std::process::Stdio::null())
        .output();
    match listed {
        Ok(out)
            if out.status.success()
                && String::from_utf8_lossy(&out.stdout).contains("gh-stack") =>
        {
            Tool::GhStack
        }
        _ => Tool::Plain,
    }
}

/// The commands that submit `branches` (bottom first, the top layer last) on `trunk`.
/// carve prints these and runs them only under `--submit`; they carry no PR text, so each
/// PR's title is that layer's one commit subject, which the human wrote.
pub fn commands(
    tool: Tool,
    trunk: Option<&str>,
    remote: Option<&str>,
    branches: &[String],
) -> Vec<String> {
    let trunk = trunk.unwrap_or("<trunk>");
    let remote = remote.unwrap_or("<remote>");
    match tool {
        Tool::GhStack => vec![format!(
            "gh stack init --base {trunk} {} && gh stack submit --auto",
            branches.join(" ")
        )],
        Tool::Plain => {
            let mut out = vec![format!("git push -u {remote} {}", branches.join(" "))];
            let mut below = trunk;
            for b in branches {
                out.push(format!("gh pr create --base {below} --head {b} --fill"));
                below = b;
            }
            out
        }
    }
}

/// The remote a stack is submitted to: `remote.pushDefault`, else `origin`, else the only
/// remote.
pub fn remote_name(repo: &Repo) -> Result<Option<String>> {
    if let Some(r) = repo
        .git
        .try_run_str(["config", "--get", "remote.pushDefault"])?
        .filter(|r| !r.is_empty())
    {
        return Ok(Some(r));
    }
    let remotes: Vec<String> = repo
        .git
        .run_str(["remote"])?
        .lines()
        .map(str::to_owned)
        .collect();
    Ok(if remotes.iter().any(|r| r == "origin") {
        Some("origin".to_owned())
    } else if remotes.len() == 1 {
        remotes.into_iter().next()
    } else {
        None
    })
}

/// The remote-tracking copies of a local branch name (`origin/feat-1-x`), sorted: carve never
/// touches remotes, so a re-carve or an undo says which ones it leaves as they are.
pub fn on_remotes(repo: &Repo, branch: &str) -> Result<Vec<String>> {
    let mut found = Vec::new();
    for remote in repo.git.run_str(["remote"])?.lines() {
        let full = format!("refs/remotes/{remote}/{branch}");
        if repo.git.test(["show-ref", "--verify", "-q", &full])? {
            found.push(format!("{remote}/{branch}"));
        }
    }
    found.sort();
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stack_slug_handles_unicode_punctuation_and_empty() {
        assert_eq!(slug("Add retry with backoff!"), "add-retry-with-backoff");
        assert_eq!(
            slug("  Fix off-by-one in parseHeader  "),
            "fix-off-by-one-in-parseheader"
        );
        assert_eq!(slug("Résumé: naïve café"), "r-sum-na-ve-caf");
        assert_eq!(slug("!!!"), "");
        assert_eq!(slug("日本語"), "");
        let long = slug("A very long subject line that keeps going well past the limit");
        assert!(long.len() <= SLUG_MAX && !long.ends_with('-'), "{long}");
    }

    #[test]
    fn stack_names_are_deterministic() {
        let subjects = vec![
            "Rename fmtBytes to formatBytes".to_owned(),
            "Retry failed fetches with backoff".to_owned(),
            "Fix off-by-one in parseHeader".to_owned(),
        ];
        let names = layer_names("feat", &subjects);
        assert_eq!(
            names,
            [
                "feat-1-rename-fmtbytes-to-formatbytes",
                "feat-2-retry-failed-fetches-with-backof"
            ]
        );
        assert_eq!(layer_names("feat", &subjects), names);
        assert!(
            layer_names("feat", &subjects[..1]).is_empty(),
            "one commit, no layer branch"
        );
        assert_eq!(layer_names("feat", &["???".into(), "x".into()]), ["feat-1"]);
    }

    #[test]
    fn stack_names_never_use_slash() {
        for name in layer_names("feature/login", &["a/b/c".into(), "top".into()]) {
            let tail = name.strip_prefix("feature/login").unwrap();
            assert!(!tail.contains('/'), "{name}");
        }
    }
}
