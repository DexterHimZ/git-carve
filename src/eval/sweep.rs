//! `git carve eval sweep` (DECISIONS.md D38): window-level BCubed F1 for many judges at many
//! thresholds, with one analysis per window and one merge sequence per judge (a sequence cut
//! at θ is the grouping at θ). It exists for leave-one-repository-out model selection on the
//! training corpus, where an arm-per-run loop would take hours.

use std::io::Write;
use std::path::Path;

use serde::Serialize;

use super::corpus::{Manifest, Split};
use super::metrics::bcubed;
use super::run::{Truth, clone_dir, materialize, truth};
use crate::error::{Error, Result, io};
use crate::facts::Class;
use crate::git::Git;
use crate::graph::{self, Plan, Soft};
use crate::group::UnionFind;
use crate::judge::LearnedJudge;
use crate::repo::Repo;
use crate::session::{self, Options};

pub struct SweepOptions {
    pub split: Option<Split>,
    /// Named judges; each is scored at every θ in `thetas` (their own θ is ignored).
    pub judges: Vec<(String, LearnedJudge)>,
    pub thetas: Vec<f64>,
}

#[derive(Serialize)]
struct Row<'a> {
    window: &'a str,
    source: &'a str,
    commits: usize,
    /// Hard-bound components of placeable atoms: what the judge merges.
    components: usize,
    /// `code` for the code-only plan.
    judge: &'a str,
    theta: Option<f64>,
    f1: f64,
    groups: usize,
    true_commits: usize,
}

/// BCubed F1 of `plan` over the window's labeled atoms (EVAL E1), and its group count.
fn score(plan: &Plan, truths: &[Truth], changes: &crate::atom::Changes) -> Option<(f64, usize)> {
    let mut items = Vec::new();
    for (g, ids) in plan.groups.iter().enumerate() {
        for id in ids {
            let i = changes.atoms.iter().position(|a| a.id == *id)?;
            if let Truth::Commit(c) = truths[i] {
                items.push((c, g));
            }
        }
    }
    bcubed(&items).map(|b| (b.f1, plan.groups.len()))
}

pub fn sweep(
    manifest: &Manifest,
    opts: &SweepOptions,
    cache: &Path,
    env: &Git,
    out: &mut dyn Write,
) -> Result<usize> {
    let min_theta = opts.thetas.iter().copied().fold(f64::INFINITY, f64::min);
    let mut scored = 0;
    let windows: Vec<_> = manifest
        .windows
        .iter()
        .filter(|w| opts.split.is_none_or(|s| s == w.split))
        .collect();
    for (n, w) in windows.iter().enumerate() {
        let git = env.with_dir(clone_dir(cache, &w.source));
        materialize(&git, w)?;
        let repo = Repo::discover(git)?;
        let prepared = match session::prepare(
            &repo,
            &Options {
                no_check: true,
                ..Options::default()
            },
        ) {
            Ok(p) => p,
            Err(Error::Precondition(_)) => continue,
            Err(e) => return Err(e),
        };
        let (changes, analysis) = (&prepared.changes, &prepared.analysis);
        let truths: Vec<Truth> = changes
            .atoms
            .iter()
            .map(|a| truth(changes, a.id, w))
            .collect();
        let labeled = truths
            .iter()
            .filter(|t| matches!(t, Truth::Commit(_)))
            .count();
        let mixed = truths.iter().filter(|t| **t == Truth::Mixed).count();
        if labeled + mixed == 0 || mixed as f64 / (labeled + mixed) as f64 > 0.2 {
            continue;
        }
        let true_commits = truths
            .iter()
            .filter_map(|t| match t {
                Truth::Commit(c) => Some(*c),
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        let edges = graph::edges(changes, analysis);
        let mut sets = UnionFind::new(changes.atoms.len());
        for &(a, b, _) in &edges.hard {
            sets.union(a, b);
        }
        let components = (0..changes.atoms.len())
            .filter(|&i| matches!(analysis.atoms[i].class, Class::Normal | Class::Symbolless))
            .map(|i| sets.find(i))
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        let mut emit = |judge: &str, theta: Option<f64>, plan: &Plan| -> Result<()> {
            if let Some((f1, groups)) = score(plan, &truths, changes) {
                let row = Row {
                    window: &w.id,
                    source: &w.source,
                    commits: w.commits.len(),
                    components,
                    judge,
                    theta,
                    f1,
                    groups,
                    true_commits,
                };
                let line =
                    serde_json::to_string(&row).map_err(|e| Error::Internal(e.to_string()))?;
                writeln!(out, "{line}").map_err(io("write sweep"))?;
            }
            Ok(())
        };
        emit("code", None, &prepared.plan)?;
        for (name, judge) in &opts.judges {
            let merges = graph::judge_sequence(changes, analysis, judge, min_theta);
            for &theta in &opts.thetas {
                let plan = graph::plan_soft(
                    changes,
                    analysis,
                    Soft::Replay {
                        judge: name,
                        merges: &merges,
                        theta,
                    },
                );
                emit(name, Some(theta), &plan)?;
            }
        }
        scored += 1;
        eprintln!("eval sweep: [{}/{}] {}", n + 1, windows.len(), w.id);
    }
    Ok(scored)
}
