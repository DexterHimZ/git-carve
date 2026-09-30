//! `git carve eval pairs` (DECISIONS.md D28): labeled training rows for the learned judge.
//! For each window: carve's hard-bound components, each labeled with its true commit, and a
//! feature row per candidate component pair, next to what the frozen code arm decided.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;

use serde::Serialize;

use super::corpus::{Manifest, Split};
use super::run::{Truth, clone_dir, materialize, truth};
use crate::error::{Error, Result, io};
use crate::facts::Class;
use crate::git::Git;
use crate::graph;
use crate::judge::Features;
use crate::repo::Repo;
use crate::session::{self, Options};

/// Windows with at most this many components contribute every pair; larger ones only pairs
/// with some signal between them.
const ALL_PAIRS_MAX: usize = 60;
const MAX_ROWS_PER_WINDOW: usize = 3000;

#[derive(Serialize)]
struct Row<'a> {
    window: &'a str,
    source: &'a str,
    split: Split,
    a: usize,
    b: usize,
    same_commit: bool,
    /// Whether the frozen code arm put the two components in one group.
    code_same: bool,
    x: Vec<f64>,
}

#[derive(Debug, Default)]
pub struct PairStats {
    pub windows: usize,
    pub rows: usize,
    pub skipped: usize,
}

pub fn pairs(
    manifest: &Manifest,
    split: Option<Split>,
    cache: &Path,
    env: &Git,
    out: &mut dyn Write,
) -> Result<PairStats> {
    let mut stats = PairStats::default();
    for w in manifest
        .windows
        .iter()
        .filter(|w| split.is_none_or(|s| s == w.split))
    {
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
            Err(Error::Precondition(_)) => {
                stats.skipped += 1;
                continue;
            }
            Err(e) => return Err(e),
        };
        let (changes, analysis, plan) = (&prepared.changes, &prepared.analysis, &prepared.plan);
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
            stats.skipped += 1;
            continue;
        }
        let edges = graph::edges(changes, analysis);
        let features = Features::new(changes, analysis, &edges);
        let placeable =
            |i: usize| matches!(analysis.atoms[i].class, Class::Normal | Class::Symbolless);

        // Hard-bound components over the atoms the judge would place.
        let mut sets = crate::group::UnionFind::new(changes.atoms.len());
        for &(a, b, _) in &edges.hard {
            sets.union(a, b);
        }
        let mut by_root: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for i in (0..changes.atoms.len()).filter(|&i| placeable(i)) {
            by_root.entry(sets.find(i)).or_default().push(i);
        }
        let code_group: BTreeMap<usize, usize> = plan
            .groups
            .iter()
            .enumerate()
            .flat_map(|(g, ids)| ids.iter().map(move |id| (id.0 as usize - 1, g)))
            .collect();
        // Components with one true commit (unknown atoms ignored; mixed ones dropped).
        let mut comps: Vec<(Vec<usize>, u8)> = Vec::new();
        for atoms in by_root.into_values() {
            let commits: BTreeSet<u8> = atoms
                .iter()
                .filter_map(|&i| {
                    if let Truth::Commit(c) = truths[i] {
                        Some(c)
                    } else {
                        None
                    }
                })
                .collect();
            let has_mixed = atoms.iter().any(|&i| truths[i] == Truth::Mixed);
            if commits.len() == 1 && !has_mixed {
                comps.push((atoms, *commits.first().expect("one")));
            }
        }
        let mut rows = 0;
        'outer: for i in 0..comps.len() {
            for j in i + 1..comps.len() {
                let (a, b) = (&comps[i].0, &comps[j].0);
                let x = features.of(a, b);
                let signal = x[..5].iter().any(|&v| v > 0.0)
                    || x[6] > 0.0
                    || x[8] > 0.0
                    || x[10] > 0.0
                    || x[13] > 0.0;
                if comps.len() > ALL_PAIRS_MAX && !signal {
                    continue;
                }
                let row = Row {
                    window: &w.id,
                    source: &w.source,
                    split: w.split,
                    a: a.len(),
                    b: b.len(),
                    same_commit: comps[i].1 == comps[j].1,
                    code_same: code_group.contains_key(&a[0])
                        && code_group.get(&a[0]) == code_group.get(&b[0]),
                    x,
                };
                let line =
                    serde_json::to_string(&row).map_err(|e| Error::Internal(e.to_string()))?;
                writeln!(out, "{line}").map_err(io("write pairs"))?;
                rows += 1;
                if rows >= MAX_ROWS_PER_WINDOW {
                    break 'outer;
                }
            }
        }
        stats.windows += 1;
        stats.rows += rows;
        eprintln!("eval pairs: {} {} rows", w.id, rows);
    }
    Ok(stats)
}
