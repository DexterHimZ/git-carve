//! Builds trees in a temporary index (SPEC §8.5). The real index is never touched here.

use std::fs;
use std::path::PathBuf;

use crate::atom::{AtomId, Changes};
use crate::error::Result;
use crate::git::Git;
use crate::reconstruct::Target;
use crate::repo::Repo;

pub struct TreeBuilder<'r> {
    repo: &'r Repo,
    git: Git,
    index: PathBuf,
}

impl<'r> TreeBuilder<'r> {
    /// A temporary index named `name`, starting from `base_tree`.
    pub fn new(repo: &'r Repo, name: &str, base_tree: &str) -> Result<Self> {
        let index = repo.ensure_carve_dir()?.join(format!("index-{name}.tmp"));
        let _ = fs::remove_file(&index);
        let git = repo.git.clone().env("GIT_INDEX_FILE", &index);
        git.run(["read-tree", base_tree])?;
        Ok(Self { repo, git, index })
    }

    /// Sets each path to its target. Removals go first so a directory/file swap is valid.
    pub fn set(&mut self, mut updates: Vec<(Vec<u8>, Target)>) -> Result<()> {
        updates.sort_by_key(|(_, target)| !matches!(target, Target::Absent));
        let blobs: Vec<&[u8]> = updates
            .iter()
            .filter_map(|(_, t)| match t {
                Target::Blob { bytes, .. } => Some(bytes.as_slice()),
                _ => None,
            })
            .collect();
        let mut written = self.repo.write_blobs(&blobs)?.into_iter();
        let null = self.repo.format.null_oid();
        let mut info = Vec::new();
        for (path, target) in &updates {
            let line = match target {
                Target::Entry(e) => format!("{:o} {}\t", e.mode.0, e.oid),
                Target::Blob { mode, .. } => {
                    let oid = written.next().expect("one oid per blob");
                    format!("{:o} {oid}\t", mode.0)
                }
                Target::Absent => format!("0 {null}\t"),
            };
            info.extend_from_slice(line.as_bytes());
            info.extend_from_slice(path);
            info.push(0);
        }
        self.git
            .run_with_stdin(["update-index", "-z", "--index-info"], &info)?;
        Ok(())
    }

    pub fn write(&self) -> Result<String> {
        Ok(self.git.run_str(["write-tree"])?)
    }
}

impl Drop for TreeBuilder<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.index);
    }
}

/// The tree after each group, cumulatively: tree k holds the base plus every atom of
/// groups 0..=k (SPEC §8.5).
pub fn prefix_trees(
    repo: &Repo,
    base_tree: &str,
    changes: &Changes,
    groups: &[Vec<AtomId>],
) -> Result<Vec<String>> {
    let mut builder = TreeBuilder::new(repo, "prefix", base_tree)?;
    let mut selected = vec![false; changes.atoms.len()];
    let mut trees = Vec::with_capacity(groups.len());
    for group in groups {
        let mut touched: Vec<usize> = Vec::new();
        for &id in group {
            selected[id.0 as usize - 1] = true;
            touched.push(changes.atom(id).file);
        }
        touched.sort_unstable();
        touched.dedup();
        let is_selected = |id: AtomId| selected[id.0 as usize - 1];
        let updates = touched
            .into_iter()
            .map(|f| {
                (
                    changes.files[f].path.clone(),
                    changes.target(f, &is_selected),
                )
            })
            .collect();
        builder.set(updates)?;
        trees.push(builder.write()?);
    }
    Ok(trees)
}
