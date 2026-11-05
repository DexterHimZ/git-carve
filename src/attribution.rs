//! Where a re-carved atom's lines came from (D46 phase 1, docs/design/carve-range.md §3.2).
//! Display only: the plan buffer's `# from:` lines and `--json`. It never enters grouping.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

use crate::atom::{AtomId, AtomKind, Changes};
use crate::error::Result;
use crate::repo::Repo;
use crate::snapshot::Snapshot;

/// Per atom, the old commits its lines come from with a line count, oldest first.
/// `None` is "uncommitted"; a count of 0 means the whole file (an add, a delete, a mode).
pub type Sources = BTreeMap<AtomId, Vec<(Option<String>, u32)>>;

/// Empty unless the snapshot is a range (its base is not HEAD).
pub fn attribute(repo: &Repo, snap: &Snapshot, changes: &Changes) -> Result<Sources> {
    let mut sources = Sources::new();
    // GIT_CARVE_DEV_NO_ATTRIBUTION: the ablation knob for timing (eval range-check).
    if snap.base.commit == snap.head.commit
        || std::env::var_os("GIT_CARVE_DEV_NO_ATTRIBUTION").is_some()
    {
        return Ok(sources);
    }
    let range = format!("{}..{}", snap.base.commit, snap.head.commit);
    for (fi, file) in changes.files.iter().enumerate() {
        let atoms: Vec<_> = changes.atoms.iter().filter(|a| a.file == fi).collect();
        if atoms.is_empty() {
            continue;
        }
        let path = OsStr::from_bytes(&file.path);
        let in_head = snap
            .files
            .iter()
            .find(|f| f.path == file.path)
            .is_some_and(|f| f.in_head);
        let regions = atoms
            .iter()
            .any(|a| matches!(&a.kind, AtomKind::Region(r) if !r.new.is_empty()));
        let lines = if regions && in_head {
            blame(repo, &range, &snap.base.commit, path)?
        } else {
            HashMap::new()
        };
        let mut touched: Option<Vec<String>> = None;
        for atom in atoms {
            let found = match &atom.kind {
                AtomKind::Region(r) => {
                    let mut counts: Vec<(Option<String>, u32)> = Vec::new();
                    for line in r.new.clone() {
                        // Lines blame does not name are new in the working tree.
                        let Some(src) = lines.get(&(line + 1)).cloned().unwrap_or(Some(None))
                        else {
                            continue; // older than the base: unchanged context
                        };
                        match counts.iter_mut().find(|(o, _)| *o == src) {
                            Some((_, n)) => *n += 1,
                            None => counts.push((src, 1)),
                        }
                    }
                    counts
                }
                _ => {
                    let commits = match &touched {
                        Some(c) => c.clone(),
                        None => {
                            let c: Vec<String> = repo
                                .git
                                .run_str([
                                    OsStr::new("log"),
                                    OsStr::new("--reverse"),
                                    OsStr::new("--format=%H"),
                                    OsStr::new(&range),
                                    OsStr::new("--"),
                                    path,
                                ])?
                                .lines()
                                .map(str::to_owned)
                                .collect();
                            touched = Some(c.clone());
                            c
                        }
                    };
                    if commits.is_empty() {
                        vec![(None, 0)]
                    } else {
                        commits.into_iter().map(|c| (Some(c), 0)).collect()
                    }
                }
            };
            if !found.is_empty() {
                sources.insert(atom.id, found);
            }
        }
    }
    Ok(sources)
}

/// Final line number (1-based) → `Some(Some(commit))` for a range commit, `Some(None)` for
/// an uncommitted line, `None` for a line older than the base.
fn blame(
    repo: &Repo,
    range: &str,
    base: &str,
    path: &OsStr,
) -> Result<HashMap<u32, Option<Option<String>>>> {
    let file = repo.path_of(path.as_bytes());
    let out = repo.git.run([
        OsStr::new("blame"),
        OsStr::new("--porcelain"),
        OsStr::new("-l"),
        OsStr::new("--contents"),
        file.as_os_str(),
        OsStr::new(range),
        OsStr::new("--"),
        path,
    ])?;
    let text = String::from_utf8_lossy(&out);
    let mut lines = HashMap::new();
    let mut current: Option<(String, u32)> = None;
    // Porcelain prints a commit's metadata (and its `boundary` mark) only once.
    let mut boundary: HashSet<String> = HashSet::new();
    for line in text.lines() {
        if let Some(_content) = line.strip_prefix('\t') {
            if let Some((oid, fin)) = current.take() {
                let src = if boundary.contains(&oid) || oid == base {
                    None
                } else if oid.bytes().all(|b| b == b'0') {
                    Some(None)
                } else {
                    Some(Some(oid))
                };
                lines.insert(fin, src);
            }
            continue;
        }
        let mut fields = line.split(' ');
        let first = fields.next().unwrap_or_default();
        if first.len() >= 40 && first.bytes().all(|b| b.is_ascii_hexdigit()) {
            let _orig = fields.next();
            if let Some(fin) = fields.next().and_then(|f| f.parse().ok()) {
                current = Some((first.to_owned(), fin));
            }
        } else if line == "boundary" {
            if let Some((oid, _)) = &current {
                boundary.insert(oid.clone());
            }
        }
    }
    Ok(lines)
}
