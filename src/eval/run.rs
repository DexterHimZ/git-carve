//! Running an arm over the corpus (EVAL.md §2–§4): materialize each window in its cached
//! clone, run carve's own pipeline, map atoms to ground truth, score, aggregate.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Instant;

use serde::Serialize;

use super::corpus::{Injection, Manifest, Split, Window, apply};
use super::metrics::{Prf, ari, bcubed, bootstrap_ci, mean, pair_counts};
use super::sha256::{hex, sha256};
use crate::atom::{AtomId, AtomKind, Changes, Whole};
use crate::error::{Error, Result, io};
use crate::git::Git;
use crate::graph::Tag;
use crate::repo::Repo;
use crate::session::{self, Options};

/// Mixed atoms beyond this share drop the window (EVAL.md §1).
const MAX_MIXED: f64 = 0.2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Truth {
    Commit(u8),
    Mixed,
    Unknown,
    Injected,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct WindowScore {
    pub id: String,
    pub source: String,
    pub split: Option<Split>,
    /// Why the window was not scored, if it was not.
    pub skipped: Option<String>,
    pub atoms: usize,
    pub labeled: usize,
    pub mixed: usize,
    pub unknown: usize,
    pub injected: usize,
    pub groups: usize,
    pub true_commits: usize,
    pub bcubed: Option<Prf>,
    /// Adjusted Rand index (ROADMAP item 5), next to BCubed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ari: Option<f64>,
    pub pairs: (usize, usize, usize),
    pub debug_left: usize,
    pub debug_left_injected: usize,
    pub real_code_left: usize,
    /// Tier B only: boundaries merge-forward dropped (E2).
    pub merges: Option<usize>,
    pub plan_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Aggregate {
    pub windows_scored: usize,
    pub windows_skipped: BTreeMap<String, usize>,
    /// E1 primary: macro BCubed F1 and its 95% bootstrap interval.
    pub e1_bcubed_f1: f64,
    pub e1_bcubed_ci95: Option<(f64, f64)>,
    /// E1 secondary: micro pairwise.
    pub e1_pairwise: Prf,
    /// Mean adjusted Rand index over scored windows (not part of the locked EVAL.md metrics).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ari_mean: Option<f64>,
    pub e2_merges_mean: Option<f64>,
    pub e3_group_count_error: f64,
    pub e4_debug: Prf,
    pub e5_real_code_left: usize,
    pub e6_plan_ms_mean: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Results {
    pub version: u32,
    pub arm: String,
    pub split: String,
    pub tier: String,
    /// D25: the code arm is frozen by this hash of the carve sources.
    pub carve_source_sha256: String,
    pub ablation: String,
    pub aggregate: Aggregate,
    pub per_source: BTreeMap<String, Aggregate>,
    pub windows: Vec<WindowScore>,
}

pub struct RunOptions {
    pub split: Option<Split>,
    /// Tier B: run the check (merge-forward included). Tier A otherwise.
    pub check: bool,
    /// The judge arm (DECISIONS.md D28); the code arm when `None`.
    pub judge: Option<crate::judge::LearnedJudge>,
    /// Tier B: `--no-new-errors` (D43), for sources whose HEAD already fails the check.
    pub no_new_errors: bool,
}

/// Tier B: which lockfile versions each source installs (D23), decided before any install.
pub(crate) fn tier_b_allowed(
    manifest: &Manifest,
    cache: &Path,
    env: &Git,
) -> Result<BTreeMap<String, Vec<String>>> {
    let mut counts: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    for w in &manifest.windows {
        let git = env.with_dir(clone_dir(cache, &w.source));
        if let Ok((_, oid, _)) = super::deps::lockfile(&git, &w.base)? {
            *counts
                .entry(w.source.clone())
                .or_default()
                .entry(oid)
                .or_default() += 1;
        }
    }
    Ok(super::deps::allowed(&counts))
}

/// Materializes a window in its cache clone; tier B first installs and links the window's
/// historical dependencies (into the cache only). `Err(reason)` when it cannot be scored.
pub(crate) fn setup_window(
    w: &Window,
    cache: &Path,
    check: bool,
    env: &Git,
    allowed: &BTreeMap<String, Vec<String>>,
) -> Result<std::result::Result<Git, String>> {
    let dir = clone_dir(cache, &w.source);
    let git = env.with_dir(&dir);
    if check {
        let (file, oid, manager) = match super::deps::lockfile(&git, &w.base)? {
            Ok(found) => found,
            Err(reason) => return Ok(Err(format!("deps: {reason}"))),
        };
        if !allowed.get(&w.source).is_some_and(|v| v.contains(&oid)) {
            return Ok(Err(
                "deps: lockfile version not among the installed three".into()
            ));
        }
        let modules = match super::deps::install(&git, &w.base, &file, &oid, &manager, cache)? {
            Ok(modules) => modules,
            Err(reason) => return Ok(Err(format!("deps: {reason}"))),
        };
        materialize(&git, w)?;
        super::deps::link(&dir, &modules)?;
    } else {
        materialize(&git, w)?;
    }
    Ok(Ok(git))
}

pub fn run(manifest: &Manifest, opts: &RunOptions, cache: &Path, env: &Git) -> Result<Results> {
    let allowed = if opts.check {
        tier_b_allowed(manifest, cache, env)?
    } else {
        BTreeMap::new()
    };
    let mut scores = Vec::new();
    let total = manifest
        .windows
        .iter()
        .filter(|w| opts.split.is_none_or(|s| s == w.split))
        .count();
    for w in manifest
        .windows
        .iter()
        .filter(|w| opts.split.is_none_or(|s| s == w.split))
    {
        let score = match score_window(w, cache, opts, env, &allowed) {
            Ok(score) => score,
            Err(err) => WindowScore {
                id: w.id.clone(),
                source: w.source.clone(),
                split: Some(w.split),
                skipped: Some(format!(
                    "error: {}",
                    err.to_string().lines().next().unwrap_or_default()
                )),
                ..WindowScore::default()
            },
        };
        eprintln!(
            "eval: [{}/{total}] {} {}",
            scores.len() + 1,
            score.id,
            score.skipped.as_deref().unwrap_or("scored")
        );
        scores.push(score);
    }
    let mut per_source: BTreeMap<String, Vec<WindowScore>> = BTreeMap::new();
    for s in &scores {
        per_source
            .entry(s.source.clone())
            .or_default()
            .push(s.clone());
    }
    Ok(Results {
        version: 1,
        arm: opts.judge.as_ref().map_or("code".into(), |j| {
            format!(
                "judge {} theta {} linkage {:?} platt {:?}",
                j.name, j.theta, j.linkage, j.platt
            )
        }),
        split: match opts.split {
            Some(Split::Tune) => "tune",
            Some(Split::Test) => "test",
            None => "all",
        }
        .into(),
        tier: if opts.check { "B" } else { "A" }.into(),
        carve_source_sha256: source_hash(),
        ablation: std::env::var("GIT_CARVE_DEV_DISABLE_EDGES").unwrap_or_default(),
        aggregate: aggregate(&scores, manifest.seed),
        per_source: per_source
            .iter()
            .map(|(k, v)| (k.clone(), aggregate(v, manifest.seed)))
            .collect(),
        windows: scores,
    })
}

pub(crate) fn materialize(git: &Git, w: &Window) -> Result<()> {
    git.run(["checkout", "-q", "-f", "--detach", w.base.as_str()])?;
    git.run(["clean", "-q", "-f", "-d", "-x"])?;
    git.run(["read-tree", "-u", "--reset", w.last()])?;
    git.run(["reset", "-q", w.base.as_str()])?;
    let mut by_path: BTreeMap<&str, Vec<Injection>> = BTreeMap::new();
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
    Ok(())
}

pub(crate) fn clone_dir(cache: &Path, source: &str) -> std::path::PathBuf {
    let name: String = source
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    cache.join("repos").join(name)
}

fn score_window(
    w: &Window,
    cache: &Path,
    opts: &RunOptions,
    env: &Git,
    allowed: &BTreeMap<String, Vec<String>>,
) -> Result<WindowScore> {
    let mut score = WindowScore {
        id: w.id.clone(),
        source: w.source.clone(),
        split: Some(w.split),
        ..WindowScore::default()
    };
    // Tier B: the window's historical dependencies, linked into the cache clone only.
    let git = match setup_window(w, cache, opts.check, env, allowed)? {
        Ok(git) => git,
        Err(reason) => {
            score.skipped = Some(reason);
            return Ok(score);
        }
    };
    let repo = Repo::discover(git)?;
    let started = Instant::now();
    let prepared = match session::prepare(
        &repo,
        &Options {
            no_check: !opts.check,
            judge: opts.judge.clone(),
            no_new_errors: opts.no_new_errors,
            ..Options::default()
        },
    ) {
        Ok(p) => p,
        Err(Error::Precondition(m)) => {
            score.skipped = Some(format!(
                "precondition: {}",
                m.lines().next().unwrap_or_default()
            ));
            return Ok(score);
        }
        Err(Error::CheckFailed(m)) => {
            score.skipped = Some(format!("check: {}", m.lines().next().unwrap_or_default()));
            return Ok(score);
        }
        Err(e) => return Err(e),
    };
    let check_ms = prepared.verification.as_ref().map_or(0, |v| v.millis);
    score.plan_ms = (started.elapsed().as_millis() as u64).saturating_sub(check_ms);
    score.merges = prepared.verification.as_ref().map(|v| v.dropped.len());
    let (changes, plan) = (&prepared.changes, &prepared.plan);
    score.atoms = changes.atoms.len();
    score.groups = plan.groups.len();

    let truths: BTreeMap<AtomId, Truth> = changes
        .atoms
        .iter()
        .map(|a| (a.id, truth(changes, a.id, w)))
        .collect();
    score.labeled = truths
        .values()
        .filter(|t| matches!(t, Truth::Commit(_)))
        .count();
    score.mixed = truths.values().filter(|t| **t == Truth::Mixed).count();
    score.unknown = truths.values().filter(|t| **t == Truth::Unknown).count();
    score.injected = truths.values().filter(|t| **t == Truth::Injected).count();
    if score.labeled + score.mixed > 0
        && score.mixed as f64 / (score.labeled + score.mixed) as f64 > MAX_MIXED
    {
        score.skipped = Some("more than 20% mixed atoms".into());
        return Ok(score);
    }
    let group_of: BTreeMap<AtomId, usize> = plan
        .groups
        .iter()
        .enumerate()
        .flat_map(|(g, ids)| ids.iter().map(move |&id| (id, g)))
        .collect();
    let items: Vec<(u8, usize)> = truths
        .iter()
        .filter_map(|(id, t)| match (t, group_of.get(id)) {
            (Truth::Commit(c), Some(&g)) => Some((*c, g)),
            _ => None,
        })
        .collect();
    score.true_commits = truths
        .values()
        .filter_map(|t| {
            if let Truth::Commit(c) = t {
                Some(*c)
            } else {
                None
            }
        })
        .collect::<BTreeSet<_>>()
        .len();
    score.bcubed = bcubed(&items);
    score.ari = ari(&items);
    score.pairs = pair_counts(&items);
    for id in &plan.leave {
        let debug = plan
            .tags
            .get(id)
            .is_some_and(|t| t.contains(&Tag::DebugOnly));
        if !debug {
            continue; // the §5.3 default leave of large or binary new files is not scored (E5)
        }
        score.debug_left += 1;
        if truths.get(id) == Some(&Truth::Injected) {
            score.debug_left_injected += 1;
        } else {
            score.real_code_left += 1;
        }
    }
    if score.bcubed.is_none() {
        score.skipped = Some("no labeled atoms in groups".into());
    }
    Ok(score)
}

/// Ground truth for one atom: the window commit(s) behind its lines.
pub(crate) fn truth(changes: &Changes, id: AtomId, w: &Window) -> Truth {
    let atom = changes.atom(id);
    let path = String::from_utf8_lossy(&changes.files[atom.file].path).into_owned();
    let Some(prov) = w.provenance.get(&path) else {
        return Truth::Unknown;
    };
    // Injected lines, in the injected file's coordinates.
    let mut at: Vec<u32> = w
        .injections
        .iter()
        .filter(|i| i.path == path)
        .map(|i| i.at)
        .collect();
    at.sort_unstable();
    let injected: Vec<u32> = at.iter().enumerate().map(|(j, a)| a + j as u32).collect();
    let original = |line: u32| line - injected.iter().filter(|&&i| i < line).count() as u32;
    let mut commits: BTreeSet<u8> = BTreeSet::new();
    match &atom.kind {
        AtomKind::Region(r) => {
            let inj = r.new.clone().filter(|l| injected.contains(l)).count();
            if inj > 0 {
                return if inj == r.new.len() && r.base.is_empty() {
                    Truth::Injected
                } else {
                    Truth::Mixed
                };
            }
            commits.extend(
                r.new
                    .clone()
                    .filter_map(|l| prov.added.get(&original(l)).copied()),
            );
            commits.extend(r.base.clone().filter_map(|l| prov.removed.get(&l).copied()));
        }
        AtomKind::Whole(Whole::Added) => commits.extend(prov.added.values().copied()),
        AtomKind::Whole(Whole::Deleted) => commits.extend(prov.removed.values().copied()),
        _ => return Truth::Unknown,
    }
    match commits.len() {
        0 => Truth::Unknown,
        1 => Truth::Commit(*commits.first().expect("one")),
        _ => Truth::Mixed,
    }
}

fn aggregate(scores: &[WindowScore], seed: u64) -> Aggregate {
    let scored: Vec<&WindowScore> = scores.iter().filter(|s| s.skipped.is_none()).collect();
    let mut skipped: BTreeMap<String, usize> = BTreeMap::new();
    for s in scores.iter().filter_map(|s| s.skipped.as_ref()) {
        let reason = s.split(':').next().unwrap_or(s).trim().to_owned();
        *skipped
            .entry(
                if reason.starts_with("more than")
                    || reason.starts_with("no labeled")
                    || reason == "deps"
                {
                    s.chars().take(90).collect()
                } else {
                    reason
                },
            )
            .or_default() += 1;
    }
    let f1s: Vec<f64> = scored
        .iter()
        .filter_map(|s| s.bcubed.map(|b| b.f1))
        .collect();
    let (tp, fp, fn_) = scored.iter().fold((0, 0, 0), |a, s| {
        (a.0 + s.pairs.0, a.1 + s.pairs.1, a.2 + s.pairs.2)
    });
    let left: usize = scored.iter().map(|s| s.debug_left).sum();
    let left_ok: usize = scored.iter().map(|s| s.debug_left_injected).sum();
    let injected: usize = scored.iter().map(|s| s.injected).sum();
    let merges: Vec<f64> = scored
        .iter()
        .filter_map(|s| s.merges.map(|m| m as f64))
        .collect();
    let aris: Vec<f64> = scored.iter().filter_map(|s| s.ari).collect();
    Aggregate {
        windows_scored: scored.len(),
        windows_skipped: skipped,
        e1_bcubed_f1: mean(&f1s),
        e1_bcubed_ci95: bootstrap_ci(&f1s, seed),
        e1_pairwise: Prf::from_counts(tp, fp, fn_),
        ari_mean: (!aris.is_empty()).then(|| mean(&aris)),
        e2_merges_mean: (!merges.is_empty()).then(|| mean(&merges)),
        e3_group_count_error: mean(
            &scored
                .iter()
                .map(|s| s.groups.abs_diff(s.true_commits) as f64)
                .collect::<Vec<_>>(),
        ),
        e4_debug: Prf::from_counts(left_ok, left - left_ok, injected - left_ok.min(injected)),
        e5_real_code_left: scored.iter().map(|s| s.real_code_left).sum(),
        e6_plan_ms_mean: mean(&scored.iter().map(|s| s.plan_ms as f64).collect::<Vec<_>>()),
    }
}

/// D25: SHA-256 over carve's sources (`src/**` and `Cargo.lock`) when they are available.
fn source_hash() -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = vec![root.join("Cargo.lock")];
    let mut stack = vec![root.join("src")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return "unavailable".into();
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p)
            } else {
                files.push(p)
            }
        }
    }
    files.sort();
    let mut all = Vec::new();
    for f in files {
        all.extend_from_slice(
            f.strip_prefix(root)
                .unwrap_or(&f)
                .to_string_lossy()
                .as_bytes(),
        );
        all.push(0);
        all.extend(std::fs::read(&f).unwrap_or_default());
    }
    hex(&sha256(&all))
}
