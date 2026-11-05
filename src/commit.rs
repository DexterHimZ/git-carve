//! Creating commits (SPEC §10.1–10.4): trees in a temporary index, `commit-tree`, a race
//! guard, a compare-and-swap ref update, then the one and only write to the real index.

use std::collections::HashSet;
use std::fs;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::atom::{AtomId, Changes};
use crate::error::{Error, Result, io};
use crate::repo::Repo;
use crate::snapshot::{self, Snapshot};
use crate::tree::prefix_trees;

/// One commit to create: its atoms and the message the user wrote.
#[derive(Clone, Debug)]
pub struct Group {
    pub atoms: Vec<AtomId>,
    pub message: String,
}

/// Test seams for the safety suite: run just before the race guard and just before the
/// ref update.
#[derive(Default)]
pub struct Hooks<'a> {
    pub before_guard: Option<&'a dyn Fn()>,
    pub before_ref_update: Option<&'a dyn Fn()>,
}

/// A stack layer branch carve created (D48): undo deletes it if it still points here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackRef {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub oid: String,
}

/// Written to `<git-path carve>/undo.json` after a successful run (§10.4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoRecord {
    pub version: u32,
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub detached: bool,
    pub old: String,
    pub new: String,
    pub commits: Vec<String>,
    pub left: Vec<String>,
    pub created_at: String,
    /// `--stack` layer branches, bottom first (D48); absent in records from older runs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stack: Vec<StackRef>,
    /// Where the plan and its messages came from (D49): `editor`, `plan-file+edit` (a human
    /// reviewed a plan file in the editor) or `plan-file` (no editor; `carve.agentMessages`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_source: Option<String>,
}

pub fn commit(
    repo: &Repo,
    snap: &Snapshot,
    changes: &Changes,
    groups: &[Group],
    hooks: &Hooks,
) -> Result<UndoRecord> {
    commit_stacked(repo, snap, changes, groups, hooks, &[], None)
}

/// [`commit`] plus one new branch per commit but the last (`--stack`, D48): `layers[k]`
/// is created at commit k only if it does not exist, after the race guard and before the
/// main compare-and-swap, and deleted again if that fails.
pub fn commit_stacked(
    repo: &Repo,
    snap: &Snapshot,
    changes: &Changes,
    groups: &[Group],
    hooks: &Hooks,
    layers: &[String],
    plan_source: Option<&str>,
) -> Result<UndoRecord> {
    if !layers.is_empty() && layers.len() + 1 != groups.len() {
        return Err(Error::Internal(format!(
            "{} stack layers for {} commits",
            layers.len(),
            groups.len()
        )));
    }
    let left = validate(changes, groups)?;
    let messages = groups
        .iter()
        .enumerate()
        .map(|(k, g)| clean_message(repo, k, &g.message))
        .collect::<Result<Vec<_>>>()?;

    // Trees, one per group, each rebuilt from the base plus every atom so far.
    let atom_groups: Vec<Vec<AtomId>> = groups.iter().map(|g| g.atoms.clone()).collect();
    let trees = prefix_trees(repo, &snap.base.tree, changes, &atom_groups)?;

    let sign = repo.config_bool("commit.gpgSign", false)?;
    let mut parent = snap.base.commit.clone();
    let mut commits = Vec::with_capacity(groups.len());
    for (tree, message) in trees.iter().zip(&messages) {
        let mut args = vec!["commit-tree", tree.as_str(), "-p", parent.as_str()];
        if sign {
            args.push("-S");
        }
        args.extend(["-F", "-"]);
        let oid = String::from_utf8_lossy(&repo.git.run_with_stdin(args, message.as_bytes())?)
            .trim_end()
            .to_owned();
        commits.push(oid.clone());
        parent = oid;
    }
    let new = parent;

    if let Some(hook) = hooks.before_guard {
        hook();
    }
    if let Some(reason) = snapshot::changed_since(repo, snap)? {
        return Err(Error::Concurrent(format!(
            "working tree changed during carve ({reason}); nothing was changed, re-run"
        )));
    }
    if let Some(hook) = hooks.before_ref_update {
        hook();
    }
    let mut stack = Vec::with_capacity(layers.len());
    let null = repo.format.null_oid();
    for (k, name) in layers.iter().enumerate() {
        let ref_name = format!("refs/heads/{name}");
        let msg = format!("carve: stack layer {} of {}", k + 1, commits.len());
        if repo
            .git
            .run(["update-ref", "-m", &msg, &ref_name, &commits[k], &null])
            .is_err()
        {
            remove_layers(repo, &stack);
            return Err(Error::Concurrent(format!(
                "branch {name} appeared during carve; nothing was changed, re-run"
            )));
        }
        stack.push(StackRef {
            ref_name,
            oid: commits[k].clone(),
        });
    }

    let mut reflog = if snap.base.commit == snap.head.commit {
        format!("carve: {} commits", commits.len())
    } else {
        format!(
            "carve: re-carved since {} into {} commits",
            &snap.base.commit[..snap.base.commit.len().min(7)],
            commits.len()
        )
    };
    if plan_source == Some("plan-file") {
        reflog.push_str(" (plan file)");
    }
    let (ref_name, update) = match &snap.head.branch {
        Some(branch) => (
            branch.clone(),
            repo.git
                .run(["update-ref", "-m", &reflog, branch, &new, &snap.head.commit]),
        ),
        None => (
            "HEAD".to_owned(),
            repo.git.run([
                "update-ref",
                "--no-deref",
                "-m",
                &reflog,
                "HEAD",
                &new,
                &snap.head.commit,
            ]),
        ),
    };
    if update.is_err() {
        remove_layers(repo, &stack);
        return Err(Error::Concurrent(format!(
            "{ref_name} moved during carve; nothing was changed, re-run"
        )));
    }

    let record = UndoRecord {
        version: 1,
        ref_name,
        detached: snap.head.branch.is_none(),
        old: snap.head.commit.clone(),
        new: new.clone(),
        commits,
        left: left.iter().map(|&id| changes.position(id)).collect(),
        created_at: rfc3339_utc(SystemTime::now()),
        stack,
        plan_source: plan_source.map(str::to_owned),
    };
    write_undo(repo, &record)?;

    // The one write to the real index: match the new HEAD, then refresh stat data.
    // Leave atoms now show up as unstaged changes, exactly as before.
    repo.git.run(["read-tree", &new])?;
    repo.git.run(["update-index", "-q", "--refresh"])?;
    Ok(record)
}

/// Every atom in at most one group, no empty group, hard binds kept together (R4). Returns
/// the atoms left out, in id order.
fn validate(changes: &Changes, groups: &[Group]) -> Result<Vec<AtomId>> {
    let mut group_of = vec![None; changes.atoms.len()];
    for (k, group) in groups.iter().enumerate() {
        if group.atoms.is_empty() {
            return Err(Error::Invalid(format!("commit {} has no atoms", k + 1)));
        }
        for &id in &group.atoms {
            if !changes.contains(id) {
                return Err(Error::Invalid(format!("unknown atom {id}")));
            }
            let slot = &mut group_of[id.0 as usize - 1];
            if slot.is_some() {
                return Err(Error::Invalid(format!("atom {id} appears twice")));
            }
            *slot = Some(k);
        }
    }
    for bind in &changes.binds {
        let (a, b) = (bind.a.0 as usize - 1, bind.b.0 as usize - 1);
        if group_of[a] != group_of[b] {
            return Err(Error::Invalid(format!(
                "{} and {} must be in the same commit ({:?})",
                bind.a, bind.b, bind.reason
            )));
        }
    }
    let placed: HashSet<usize> = (0..group_of.len())
        .filter(|&i| group_of[i].is_some())
        .collect();
    Ok((0..changes.atoms.len())
        .filter(|i| !placed.contains(i))
        .map(|i| changes.atoms[i].id)
        .collect())
}

/// Normalizes whitespace like `git commit` does. Comment stripping belongs to the plan
/// parser, which knows the comment character.
fn clean_message(repo: &Repo, k: usize, message: &str) -> Result<String> {
    let cleaned = repo
        .git
        .run_with_stdin(["stripspace"], message.as_bytes())?;
    let cleaned = String::from_utf8_lossy(&cleaned).into_owned();
    if cleaned.trim().is_empty() {
        return Err(Error::Invalid(format!(
            "commit {} has an empty message",
            k + 1
        )));
    }
    Ok(cleaned)
}

/// Deletes layer branches that still point where carve put them; returns the ones that
/// moved (kept).
pub fn remove_layers(repo: &Repo, stack: &[StackRef]) -> Vec<String> {
    let mut kept = Vec::new();
    for s in stack {
        if repo
            .git
            .run(["update-ref", "-d", &s.ref_name, &s.oid])
            .is_err()
        {
            kept.push(s.ref_name.trim_start_matches("refs/heads/").to_owned());
        }
    }
    kept
}

pub fn undo_path(repo: &Repo) -> std::path::PathBuf {
    repo.carve_dir.join("undo.json")
}

fn write_undo(repo: &Repo, record: &UndoRecord) -> Result<()> {
    let path = undo_path(repo);
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_vec_pretty(record)
        .map_err(|e| Error::Internal(format!("serialize undo record: {e}")))?;
    fs::write(&tmp, json).map_err(io(format!("write {}", tmp.display())))?;
    fs::rename(&tmp, &path).map_err(io(format!("write {}", path.display())))
}

/// `YYYY-MM-DDTHH:MM:SSZ` without a date-time dependency.
fn rfc3339_utc(time: SystemTime) -> String {
    let secs = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::rfc3339_utc;
    use std::time::{Duration, SystemTime};

    #[test]
    fn formats_utc_dates() {
        let at = |s| rfc3339_utc(SystemTime::UNIX_EPOCH + Duration::from_secs(s));
        assert_eq!(at(0), "1970-01-01T00:00:00Z");
        assert_eq!(at(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(at(1_790_000_000), "2026-09-21T14:13:20Z");
    }
}
