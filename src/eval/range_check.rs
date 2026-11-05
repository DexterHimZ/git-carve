//! `git carve eval range-check` (D46 phase 1, docs/design/carve-range.md step 8): proves the
//! product path `git carve <base>` produces the same plan as the evaluated path.
//!
//! For each window, path A is the eval harness: `materialize` turns the window into a dirty
//! tree on top of its base, then a plain carve. Path B is the product: the window's last
//! commit checked out, the window's injected debug lines applied as uncommitted edits, then
//! `git carve <base>`. Both run code-only, without the check. Atoms, groups, leave, and
//! edges must match exactly.

use std::time::Instant;

use super::corpus::{Manifest, Split, Window, apply};
use super::run::{clone_dir, materialize};
use crate::error::{Error, Result, io};
use crate::git::Git;
use crate::precondition::resolve_base;
use crate::repo::Repo;
use crate::report;
use crate::session::{self, Options};

#[derive(Debug, Default)]
pub struct RangeCheck {
    pub windows: usize,
    pub identical: usize,
    /// Window id and how many committed new binary or large files range mode keeps, where
    /// the eval path (which sees them as untracked) leaves them out: the intended `in_head`
    /// rule (design §4). Atoms are identical; only those files' placement differs.
    pub expected: Vec<(String, usize)>,
    /// Window id and the first differing line of the two plans' JSON.
    pub mismatches: Vec<(String, String)>,
    /// `(window, reason)`; refusals and errors are counted, never hidden.
    pub refused: Vec<(String, String)>,
    pub plan_ms_eval: Vec<u64>,
    pub plan_ms_range: Vec<u64>,
}

pub fn range_check(
    manifest: &Manifest,
    split: Option<Split>,
    limit: usize,
    cache: &std::path::Path,
    env: &Git,
) -> Result<RangeCheck> {
    let mut out = RangeCheck::default();
    let windows = manifest
        .windows
        .iter()
        .filter(|w| split.is_none_or(|s| w.split == s))
        .take(limit);
    for w in windows {
        out.windows += 1;
        let dir = clone_dir(cache, &w.source);
        let git = env.with_dir(&dir);
        let a = match plan_eval(&git, w) {
            Ok(a) => a,
            Err(e) => {
                out.refused
                    .push((w.id.clone(), format!("eval path: {}", first_line(&e))));
                continue;
            }
        };
        let b = match plan_range(&git, w) {
            Ok(b) => b,
            Err(e) => {
                out.refused
                    .push((w.id.clone(), format!("range path: {}", first_line(&e))));
                continue;
            }
        };
        out.plan_ms_eval.push(a.1);
        out.plan_ms_range.push(b.1);
        if a.0 == b.0 {
            out.identical += 1;
        } else if let Some(kept) = committed_assets_only(&a.0, &b.0) {
            out.expected.push((w.id.clone(), kept));
        } else {
            let diff =
                a.0.lines()
                    .zip(b.0.lines())
                    .find(|(x, y)| x != y)
                    .map_or_else(
                        || "the plans differ in length".to_owned(),
                        |(x, y)| format!("eval `{}` vs range `{}`", x.trim(), y.trim()),
                    );
            out.mismatches.push((w.id.clone(), diff));
        }
    }
    Ok(out)
}

/// `Some(n)` when the plans' atoms are identical and the only difference is `n` whole-file
/// additions the eval path leaves out by default and range mode keeps.
fn committed_assets_only(eval: &str, range: &str) -> Option<usize> {
    let a: serde_json::Value = serde_json::from_str(eval).ok()?;
    let b: serde_json::Value = serde_json::from_str(range).ok()?;
    if a["atoms"] != b["atoms"] {
        return None;
    }
    let ids = |v: &serde_json::Value| -> std::collections::BTreeSet<String> {
        v.as_array()
            .into_iter()
            .flatten()
            .filter_map(|x| x.as_str().map(str::to_owned))
            .collect()
    };
    let (left_a, left_b) = (ids(&a["leave"]), ids(&b["leave"]));
    if !left_b.is_subset(&left_a) {
        return None;
    }
    let kept: Vec<&String> = left_a.difference(&left_b).collect();
    let added = |id: &str| {
        a["atoms"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|x| x["id"].as_str() == Some(id) && x["kind"].as_str() == Some("new file"))
    };
    (!kept.is_empty() && kept.iter().all(|id| added(id))).then_some(kept.len())
}

fn first_line(e: &Error) -> String {
    e.to_string().lines().next().unwrap_or_default().to_owned()
}

/// The plan as JSON without the `range` block (the only intended difference), and the
/// planning time in milliseconds.
fn plan_json(repo: &Repo, opts: &Options) -> Result<(String, u64)> {
    let started = Instant::now();
    let p = session::prepare(repo, opts)?;
    let ms = started.elapsed().as_millis() as u64;
    let json = report::json(&p.changes, &p.analysis, &p.plan, None);
    Ok((json, ms))
}

fn plan_eval(git: &Git, w: &Window) -> Result<(String, u64)> {
    materialize(git, w)?;
    let repo = Repo::discover(git.clone())?;
    plan_json(
        &repo,
        &Options {
            no_check: true,
            ..Options::default()
        },
    )
}

fn plan_range(git: &Git, w: &Window) -> Result<(String, u64)> {
    git.run(["checkout", "-q", "-f", "--detach", w.last()])?;
    git.run(["clean", "-q", "-f", "-d", "-x"])?;
    let mut by_path: std::collections::BTreeMap<&str, Vec<_>> = std::collections::BTreeMap::new();
    for inj in &w.injections {
        by_path
            .entry(inj.path.as_str())
            .or_default()
            .push(inj.clone());
    }
    for (path, injections) in by_path {
        let file = git.dir().join(path);
        let content = std::fs::read(&file).map_err(io(format!("read {}", file.display())))?;
        std::fs::write(&file, apply(&content, &injections))
            .map_err(io(format!("write {}", file.display())))?;
    }
    let repo = Repo::discover(git.clone())?;
    let base = resolve_base(&repo, &w.base, true)?;
    plan_json(
        &repo,
        &Options {
            no_check: true,
            base: Some(base),
            ..Options::default()
        },
    )
}
