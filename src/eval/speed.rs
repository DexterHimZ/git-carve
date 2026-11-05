//! D51's pre-registered speed benchmark (`docs/design/agent-gate-speed.md`, phase 4). An
//! untimed carve first remembers the probe verdicts (D1), as any later carve on a
//! developer's machine finds them. Then each tier-B window is carved once per job count with
//! no cached tree outcomes, and either:
//! - **cold:** no build info at all, the worst case (a first carve after `--clean`); or
//! - **warm** (`--warm`): every worker's build info starts at HEAD, as an earlier carve or
//!   verify leaves it.
//!
//! The ratio is the verify step's wall time over one cold check of the final tree (a
//! developer's plain `tsc --noEmit`). Plans must not differ between job counts.

use std::fs;
use std::path::Path;
use std::time::Instant;

use serde::Serialize;

use super::corpus::{Manifest, Split};
use super::run::{setup_window, tier_b_allowed};
use crate::check::{self, Checker};
use crate::error::{Error, Result};
use crate::git::Git;
use crate::repo::Repo;
use crate::session::{self, Options};

#[derive(Clone, Debug, Serialize)]
pub struct JobRun {
    pub jobs: usize,
    /// The verify step's wall time.
    pub wall_ms: u64,
    pub runs: usize,
    /// SHA-256 of the planned groups and leave, for "zero plan differences".
    pub plan: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct SpeedRow {
    pub id: String,
    pub source: String,
    pub groups: usize,
    /// One cold check of the final tree: the "one check" of "about 2x one check".
    pub final_ms: u64,
    pub runs: Vec<JobRun>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
}

pub fn speed(
    manifest: &Manifest,
    split: Option<Split>,
    jobs: &[usize],
    warm: bool,
    limit: usize,
    cache: &Path,
    env: &Git,
) -> Result<Vec<SpeedRow>> {
    let allowed = tier_b_allowed(manifest, cache, env)?;
    let mut rows = Vec::new();
    for w in manifest
        .windows
        .iter()
        .filter(|w| split.is_none_or(|s| s == w.split))
    {
        if rows
            .iter()
            .filter(|r: &&SpeedRow| r.skipped.is_none())
            .count()
            >= limit
        {
            break;
        }
        let mut row = SpeedRow {
            id: w.id.clone(),
            source: w.source.clone(),
            groups: 0,
            final_ms: 0,
            runs: Vec::new(),
            skipped: None,
        };
        match window(w, cache, env, &allowed, jobs, warm, &mut row) {
            Ok(()) => {}
            Err(Error::Precondition(m) | Error::CheckFailed(m)) => {
                row.skipped = Some(m.lines().next().unwrap_or_default().to_owned());
            }
            Err(e) => row.skipped = Some(format!("error: {e}")),
        }
        eprintln!(
            "speed: {} {}",
            row.id,
            row.skipped.as_deref().unwrap_or("measured")
        );
        rows.push(row);
    }
    Ok(rows)
}

fn window(
    w: &super::corpus::Window,
    cache: &Path,
    env: &Git,
    allowed: &std::collections::BTreeMap<String, Vec<String>>,
    jobs: &[usize],
    warm: bool,
    row: &mut SpeedRow,
) -> Result<()> {
    let git = match setup_window(w, cache, true, env, allowed)? {
        Ok(git) => git,
        Err(reason) => {
            row.skipped = Some(reason);
            return Ok(());
        }
    };
    let repo = Repo::discover(git)?;
    let options = |jobs: usize| Options {
        jobs: Some(jobs),
        // D43: windows whose base already fails still measure the same machinery.
        no_new_errors: true,
        ..Options::default()
    };
    // Untimed: remembers the probe verdicts and creates every worker.
    let most = jobs.iter().copied().max().unwrap_or(1);
    let base_tree = session::prepare(&repo, &options(most))?.snap.base.tree;
    let mut final_tree = None;
    for &k in jobs {
        forget(&repo)?;
        if warm {
            let carved: Vec<&[u8]> = Vec::new();
            let mut checker = Checker::new(&repo, check::resolve(&repo, None, &carved)?, &carved)?;
            checker.jobs = k;
            checker.warm(&base_tree)?;
        }
        let prepared = session::prepare(&repo, &options(k))?;
        let v = prepared.verification.as_ref().expect("checked");
        let plan = format!("{:?}|{:?}", prepared.plan.groups, prepared.plan.leave);
        row.groups = prepared.plan.groups.len();
        row.runs.push(JobRun {
            jobs: k,
            wall_ms: v.wall_ms,
            runs: v.runs,
            plan: super::sha256::hex(&super::sha256::sha256(plan.as_bytes())),
        });
        final_tree = crate::tree::prefix_trees(
            &repo,
            &prepared.snap.base.tree,
            &prepared.changes,
            &prepared.plan.groups,
        )?
        .pop();
    }
    // One cold check of the final tree, in the scratch worktree.
    let Some(tree) = final_tree else {
        return Ok(());
    };
    forget(&repo)?;
    let carved: Vec<&[u8]> = Vec::new();
    let spec = check::resolve(&repo, None, &carved)?;
    let mut checker = Checker::new(&repo, spec, &carved)?;
    let started = Instant::now();
    checker.check(&tree)?;
    row.final_ms = started.elapsed().as_millis() as u64;
    Ok(())
}

/// Cold state: the cached tree outcomes and every worker's build info go; probe verdicts
/// stay (a developer's second carve remembers them, D1).
fn forget(repo: &Repo) -> Result<()> {
    let carve = &repo.carve_dir;
    let cache = carve.join("checks.json");
    if let Ok(bytes) = fs::read(&cache) {
        let mut doc: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| Error::Internal(format!("read check cache: {e}")))?;
        if let Some(entries) = doc.get_mut("entries").and_then(|e| e.as_array_mut()) {
            entries.retain(|e| e[0].as_str().is_some_and(|k| k.starts_with("probe|")));
        }
        fs::write(&cache, doc.to_string())
            .map_err(|e| Error::Internal(format!("write check cache: {e}")))?;
    }
    let mut dirs = vec![carve.join("tsbuildinfo")];
    for entry in fs::read_dir(carve).into_iter().flatten().flatten() {
        if entry.file_name().to_string_lossy().starts_with('w') {
            dirs.push(entry.path().join("tsbuildinfo"));
        }
    }
    for dir in dirs {
        for file in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let _ = fs::remove_file(file.path());
        }
    }
    Ok(())
}
