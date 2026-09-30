//! `git carve eval capture` (EVAL.md §1.B, DECISIONS.md D36): saves a real messy tree for the
//! blinded E7 scoring, without writing anything to the user's repository.
//!
//! git runs with `GIT_DIR` pointing at a private store under the eval cache and
//! `GIT_WORK_TREE` at the user's repository, so it *reads* the user's files while every write
//! (objects, index, refs) lands in the store. Untracked files are captured too (unlike
//! `git stash create`), and a ref pins each capture so `git gc` cannot drop it.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, io};
use crate::git::Git;
use crate::reconstruct::Target;
use crate::repo::Repo;
use crate::snapshot;
use crate::tree::TreeBuilder;

/// Identity for the store's capture commits (never the user's commits).
const IDENTITY: &[(&str, &str)] = &[
    ("GIT_AUTHOR_NAME", "git-carve eval"),
    ("GIT_AUTHOR_EMAIL", "eval@git-carve.invalid"),
    ("GIT_COMMITTER_NAME", "git-carve eval"),
    ("GIT_COMMITTER_EMAIL", "eval@git-carve.invalid"),
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// The capture commit's id: its tree is the messy working tree, its parent the user's HEAD.
    pub id: String,
    /// The user's repository (local path; stays on this machine).
    pub repo: String,
    /// The repository's root commit, which names its store.
    pub root: String,
    pub head: String,
    pub note: String,
    /// Seconds since the epoch.
    pub captured_at: u64,
}

/// The store for a repository: one per root commit, so object formats never mix.
pub fn store_dir(base: &Path, root: &str) -> PathBuf {
    base.join(format!("{}.git", &root[..root.len().min(16)]))
}

/// Captures the working tree of the repository at `dir` into a store under `base`.
pub fn capture(env: &Git, dir: &Path, base: &Path, note: &str) -> Result<Record> {
    // Read-only questions to the user's repository.
    let user = env.with_dir(dir);
    let toplevel = PathBuf::from(user.run_str(["rev-parse", "--show-toplevel"])?);
    let user = user.with_dir(&toplevel);
    let head = user.run_str(["rev-parse", "--verify", "HEAD^{commit}"])?;
    let root = user
        .run_str(["rev-list", "--max-parents=0", "HEAD"])?
        .lines()
        .min()
        .unwrap_or_default()
        .to_owned();
    let format = user.run_str(["rev-parse", "--show-object-format"])?;
    let exclude = toplevel.join(user.run_str(["rev-parse", "--git-path", "info/exclude"])?);

    // The store: HEAD's commit only (shallow), fetched over file:// so the user's side only
    // runs upload-pack, which does not write.
    let store = store_dir(base, &root);
    fs::create_dir_all(base).map_err(io(format!("create {}", base.display())))?;
    let git = env.with_dir(base);
    if !store.exists() {
        git.run([
            "init",
            "-q",
            "--bare",
            &format!("--object-format={format}"),
            &store.to_string_lossy(),
        ])?;
    }
    let store_git = env.with_dir(&store);
    store_git.run(["config", "core.bare", "false"])?;
    let url = format!("file://{}", toplevel.display());
    store_git.run(["fetch", "-q", "--no-tags", "--depth=1", &url, &head])?;
    store_git.run(["update-ref", "--no-deref", "HEAD", &head])?;
    // The user's own ignore rules, so the capture sees what `git status` shows them.
    let info = store.join("info");
    fs::create_dir_all(&info).map_err(io("create store info"))?;
    let rules = fs::read(&exclude).unwrap_or_default();
    fs::write(info.join("exclude"), rules).map_err(io("write store exclude"))?;

    // The store's index starts at HEAD; status then compares the user's files by content.
    let work = env
        .with_dir(&toplevel)
        .env("GIT_DIR", &store)
        .env("GIT_WORK_TREE", &toplevel);
    work.run(["read-tree", &head])?;
    let repo = Repo::discover(work.clone())?;
    let snap = snapshot::take(&repo)?;
    let mut builder = TreeBuilder::new(&repo, "capture", &snap.head.tree)?;
    builder.set(
        snap.files
            .iter()
            .map(|f| {
                let target = f.new.clone().map_or(Target::Absent, Target::Entry);
                (f.path.clone(), target)
            })
            .collect(),
    )?;
    let tree = builder.write()?;
    drop(builder);
    let mut commit_git = work.clone();
    for (k, v) in IDENTITY {
        commit_git = commit_git.env(k, v);
    }
    let id = commit_git.run_str(["commit-tree", &tree, "-p", &head, "-m", "real tree"])?;
    store_git.run(["update-ref", &format!("refs/real/{id}"), &id])?;

    let record = Record {
        id,
        repo: toplevel.to_string_lossy().into_owned(),
        root,
        head,
        note: note.to_owned(),
        captured_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
    };
    let line = serde_json::to_string(&record).map_err(|e| Error::Internal(e.to_string()))?;
    let mut index = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(base.join("index.jsonl"))
        .map_err(io("open real-tree index"))?;
    writeln!(index, "{line}").map_err(io("write real-tree index"))?;
    Ok(record)
}

/// Every capture so far, oldest first.
pub fn records(base: &Path) -> Result<Vec<Record>> {
    let Ok(text) = fs::read_to_string(base.join("index.jsonl")) else {
        return Ok(Vec::new());
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str(l).map_err(|e| Error::Invalid(format!("real-tree index: {e}")))
        })
        .collect()
}

/// Which arm is shown as plan A for a capture: a seeded hash, so the order is random to the
/// scorer but reproducible at unblinding.
pub fn judge_is_a(seed: u64, id: &str) -> bool {
    super::sha256::sha256(format!("real:{seed}:{id}").as_bytes())[0] & 1 == 1
}

/// One capture's blinded scores, filled in by the user. `A`/`B` are the two plans as shown.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Scored {
    pub id: String,
    pub note: String,
    pub cuts_to_change: AB,
    pub leave_disagreements: AB,
    /// "A", "B", or "tie".
    pub preferred: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub struct AB {
    pub a: Option<f64>,
    pub b: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Scores {
    pub seed: u64,
    pub trees: Vec<Scored>,
}

/// Renders both arms' plan buffers for every capture into `out` (blinded as A and B), and a
/// `scores.json` template. Captures carve refuses (for example, a clean tree) are skipped
/// with their reason. The files hold the user's code, so `out` belongs outside the project.
pub fn render(env: &Git, base: &Path, out: &Path, seed: u64) -> Result<Vec<(String, String)>> {
    use crate::plan::{self, RenderOptions};
    use crate::session::{self, Options};

    let mut skipped = Vec::new();
    let mut scores = Scores {
        seed,
        trees: Vec::new(),
    };
    fs::create_dir_all(out).map_err(io(format!("create {}", out.display())))?;
    for (n, record) in records(base)?.iter().enumerate() {
        let store = store_dir(base, &record.root);
        let work = base
            .join("work")
            .join(&record.root[..record.root.len().min(16)]);
        if !work.exists() {
            env.with_dir(&store).run([
                "worktree",
                "add",
                "-q",
                "--detach",
                &work.to_string_lossy(),
                &record.head,
            ])?;
        }
        let git = env.with_dir(&work);
        git.run(["checkout", "-q", "-f", "--detach", &record.head])?;
        git.run(["clean", "-q", "-f", "-d", "-x"])?;
        git.run(["read-tree", "-u", "--reset", &record.id])?;
        git.run(["reset", "-q", &record.head])?;
        let repo = Repo::discover(git)?;
        let mut buffers = Vec::new();
        for judge in [None, Some(crate::judge::LearnedJudge::builtin())] {
            let opts = Options {
                no_check: true,
                judge,
                ..Options::default()
            };
            match session::prepare(&repo, &opts) {
                Ok(p) => buffers.push(plan::render(
                    &p.changes,
                    &p.analysis,
                    &p.plan,
                    &RenderOptions {
                        verification: None,
                        messages: &vec![None; p.plan.groups.len()],
                        notes: &[],
                        diff_lines: plan::DIFF_TOTAL_DEFAULT,
                        // Blinded: notes exist only in judge mode and would reveal the arm.
                        unsure: false,
                    },
                )),
                Err(e) => {
                    skipped.push((record.id.clone(), e.to_string()));
                    break;
                }
            }
        }
        let [code, judged] = <[String; 2]>::try_from(buffers).unwrap_or_default();
        if code.is_empty() {
            continue;
        }
        let (a, b) = if judge_is_a(seed, &record.id) {
            (judged, code)
        } else {
            (code, judged)
        };
        let dir = out.join(format!("tree-{:02}", n + 1));
        fs::create_dir_all(&dir).map_err(io(format!("create {}", dir.display())))?;
        fs::write(dir.join("A.txt"), a).map_err(io("write plan A"))?;
        fs::write(dir.join("B.txt"), b).map_err(io("write plan B"))?;
        scores.trees.push(Scored {
            id: record.id.clone(),
            note: record.note.clone(),
            ..Scored::default()
        });
    }
    let template =
        serde_json::to_string_pretty(&scores).map_err(|e| Error::Internal(e.to_string()))?;
    let path = out.join("scores.json");
    if !path.exists() {
        fs::write(&path, template + "\n").map_err(io("write scores template"))?;
    }
    Ok(skipped)
}

/// Unblinds filled-in scores into E7's inputs. Every tree must be fully scored.
pub fn unblind(scores: &Scores) -> Result<super::decide::Real> {
    let mut real = super::decide::Real::default();
    for t in &scores.trees {
        let missing = || {
            Error::Invalid(format!(
                "tree {} is not fully scored",
                &t.id[..t.id.len().min(12)]
            ))
        };
        let (ca, cb) = (
            t.cuts_to_change.a.ok_or_else(missing)?,
            t.cuts_to_change.b.ok_or_else(missing)?,
        );
        let (la, lb) = (
            t.leave_disagreements.a.ok_or_else(missing)?,
            t.leave_disagreements.b.ok_or_else(missing)?,
        );
        let judge_a = judge_is_a(scores.seed, &t.id);
        let (cut_code, cut_judge, leave_code, leave_judge) = if judge_a {
            (cb, ca, lb, la)
        } else {
            (ca, cb, la, lb)
        };
        real.cuts_code.push(cut_code);
        real.cuts_judge.push(cut_judge);
        real.leave_code += leave_code;
        real.leave_judge += leave_judge;
        match (t.preferred.as_deref(), judge_a) {
            (Some("tie"), _) => real.ties += 1,
            (Some("A"), true) | (Some("B"), false) => real.preferred_judge += 1,
            (Some("A"), false) | (Some("B"), true) => real.preferred_code += 1,
            _ => return Err(missing()),
        }
    }
    Ok(real)
}
