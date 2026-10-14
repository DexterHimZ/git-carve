//! Atoms: the zero-context changed regions, whole-file changes, and mode changes between
//! HEAD and the working tree (SPEC §5.3), plus the structural hard binds known at this
//! stage (renames, directory/file swaps).

use std::collections::HashMap;
use std::fmt;

use crate::error::{Error, Result};
use crate::model::{Entry, Mode, show};
use crate::reconstruct::{self, Target};
use crate::repo::Repo;
use crate::snapshot::Snapshot;
use crate::text::{Region, TextPair};
use crate::tree::TreeBuilder;

/// Bytes inspected for a NUL when deciding a file is binary (same as git).
const BINARY_SNIFF: usize = 8000;
/// Larger text files are whole-file atoms.
const MAX_TEXT_BYTES: u64 = 2 << 20;
/// Untracked files larger than this default to `leave` (§5.3).
const LARGE_UNTRACKED: u64 = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AtomId(pub u32);

impl fmt::Display for AtomId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a{}", self.0)
    }
}

impl serde::Serialize for AtomId {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Whole {
    Added,
    Deleted,
    Binary,
    Filtered,
    Symlink,
    TypeChange,
    NonUtf8,
    TooLarge,
}

impl fmt::Display for Whole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Added => "new file",
            Self::Deleted => "deleted",
            Self::Binary => "binary",
            Self::Filtered => "filtered",
            Self::Symlink => "symlink",
            Self::TypeChange => "type change",
            Self::NonUtf8 => "not UTF-8",
            Self::TooLarge => "large file",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AtomKind {
    Region(Region),
    Whole(Whole),
    Mode { from: Mode, to: Mode },
}

#[derive(Clone, Debug)]
pub struct Atom {
    pub id: AtomId,
    /// Index into [`Changes::files`].
    pub file: usize,
    pub kind: AtomKind,
    pub leave_by_default: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindReason {
    Rename,
    /// A file replaced by a directory of the same name, or the reverse: both halves must
    /// land in one commit or the intermediate tree is invalid.
    DirectoryFile,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bind {
    pub a: AtomId,
    pub b: AtomId,
    pub reason: BindReason,
}

#[derive(Clone, Debug)]
pub struct FileChange {
    pub path: Vec<u8>,
    pub base: Option<Entry>,
    pub new: Option<Entry>,
    /// Present when the file is diffed line by line.
    pub text: Option<TextPair>,
}

#[derive(Clone, Debug)]
pub struct Changes {
    pub files: Vec<FileChange>,
    /// Sorted by id; `atoms[i].id == AtomId(i + 1)`.
    pub atoms: Vec<Atom>,
    /// Atom indices per file.
    pub file_atoms: Vec<Vec<usize>>,
    pub binds: Vec<Bind>,
}

impl Changes {
    pub fn atom(&self, id: AtomId) -> &Atom {
        &self.atoms[id.0 as usize - 1]
    }

    pub fn contains(&self, id: AtomId) -> bool {
        id.0 >= 1 && (id.0 as usize) <= self.atoms.len()
    }

    /// `path:lines` for regions, `path` otherwise. Used for the leave list and undo record.
    pub fn position(&self, id: AtomId) -> String {
        let atom = self.atom(id);
        let path = show(&self.files[atom.file].path);
        match &atom.kind {
            AtomKind::Region(r) => format!("{path}:{}", lines(r)),
            _ => path,
        }
    }

    /// The code-generated label shown after an atom id in the plan buffer (§9.2).
    pub fn label(&self, id: AtomId) -> String {
        let atom = self.atom(id);
        match &atom.kind {
            AtomKind::Region(r) => format!(
                "{:<32} +{} -{}",
                self.position(id),
                r.new.len(),
                r.base.len()
            ),
            AtomKind::Whole(w) => {
                format!("{:<32} [{w}]", show(&self.files[atom.file].path))
            }
            AtomKind::Mode { from, to } => format!(
                "{:<32} [mode {from} -> {to}]",
                show(&self.files[atom.file].path)
            ),
        }
    }

    /// What `file` looks like when exactly the atoms for which `selected` is true apply.
    pub fn target(&self, file: usize, selected: &dyn Fn(AtomId) -> bool) -> Target {
        reconstruct::target(self, file, selected)
    }
}

/// 1-based display lines: the new side if it has lines, else the base position.
fn lines(r: &Region) -> String {
    let (start, end) = if r.new.is_empty() {
        (r.base.start, r.base.end)
    } else {
        (r.new.start, r.new.end)
    };
    if end <= start + 1 {
        format!("{}", start + 1)
    } else {
        format!("{}-{}", start + 1, end)
    }
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(BINARY_SNIFF)].contains(&0)
}

pub fn atomize(repo: &Repo, snap: &Snapshot) -> Result<Changes> {
    // Blob sizes first, so nothing huge is loaded into memory.
    let mut wanted: Vec<&str> = Vec::new();
    for f in &snap.files {
        match (&f.base, &f.new) {
            (None, Some(new)) if new.mode != Mode::LINK => wanted.push(&new.oid),
            (Some(base), Some(new))
                if base.mode.is_regular()
                    && new.mode.is_regular()
                    && base.oid != new.oid
                    && !f.filtered
                    && !f.binary_attr =>
            {
                wanted.extend([base.oid.as_str(), new.oid.as_str()]);
            }
            _ => {}
        }
    }
    wanted.sort_unstable();
    wanted.dedup();
    let sizes: HashMap<&str, u64> = wanted
        .iter()
        .copied()
        .zip(repo.blob_sizes(&wanted)?)
        .collect();
    let loadable: Vec<&str> = wanted
        .iter()
        .copied()
        .filter(|oid| sizes[oid] <= MAX_TEXT_BYTES)
        .collect();
    let blobs: HashMap<&str, Vec<u8>> = loadable
        .iter()
        .copied()
        .zip(repo.read_blobs(&loadable)?)
        .collect();

    let mut files = Vec::with_capacity(snap.files.len());
    let mut drafts: Vec<(usize, AtomKind, bool)> = Vec::new();
    for (fi, f) in snap.files.iter().enumerate() {
        let mut text = None;
        match (&f.base, &f.new) {
            (None, None) => {
                return Err(Error::Internal(format!("{} has no sides", show(&f.path))));
            }
            (None, Some(new)) => {
                let leave = new.mode != Mode::LINK
                    && (sizes[new.oid.as_str()] > LARGE_UNTRACKED
                        || blobs.get(new.oid.as_str()).is_none_or(|b| is_binary(b)));
                drafts.push((fi, AtomKind::Whole(Whole::Added), leave));
            }
            (Some(_), None) => drafts.push((fi, AtomKind::Whole(Whole::Deleted), false)),
            (Some(base), Some(new)) if base.mode == Mode::LINK || new.mode == Mode::LINK => {
                let kind = if base.mode == new.mode {
                    Whole::Symlink
                } else {
                    Whole::TypeChange
                };
                drafts.push((fi, AtomKind::Whole(kind), false));
            }
            (Some(base), Some(new)) => {
                if base.mode != new.mode {
                    drafts.push((
                        fi,
                        AtomKind::Mode {
                            from: base.mode,
                            to: new.mode,
                        },
                        false,
                    ));
                }
                if base.oid != new.oid {
                    let whole = if f.filtered {
                        Some(Whole::Filtered)
                    } else if f.binary_attr {
                        Some(Whole::Binary)
                    } else {
                        match (blobs.get(base.oid.as_str()), blobs.get(new.oid.as_str())) {
                            (Some(b), Some(n)) if is_binary(b) || is_binary(n) => {
                                Some(Whole::Binary)
                            }
                            (Some(b), Some(n))
                                if std::str::from_utf8(b).is_err()
                                    || std::str::from_utf8(n).is_err() =>
                            {
                                Some(Whole::NonUtf8)
                            }
                            (Some(_), Some(_)) => None,
                            _ => Some(Whole::TooLarge),
                        }
                    };
                    match whole {
                        Some(kind) => drafts.push((fi, AtomKind::Whole(kind), false)),
                        None => {
                            // Cloned, not taken: two files can share a blob.
                            let pair = TextPair::new(
                                blobs[base.oid.as_str()].clone(),
                                blobs[new.oid.as_str()].clone(),
                            );
                            for region in pair.regions() {
                                drafts.push((fi, AtomKind::Region(region), false));
                            }
                            text = Some(pair);
                        }
                    }
                }
            }
        }
        files.push(FileChange {
            path: f.path.clone(),
            base: f.base.clone(),
            new: f.new.clone(),
            text,
        });
    }

    // Ids by (path, base line, kind); files are already in path order.
    let key = |kind: &AtomKind| match kind {
        AtomKind::Region(r) => (r.base.start, 0),
        AtomKind::Whole(_) => (0, 1),
        AtomKind::Mode { .. } => (0, 2),
    };
    drafts.sort_by_key(|(fi, kind, _)| (*fi, key(kind)));
    let mut file_atoms = vec![Vec::new(); files.len()];
    let atoms: Vec<Atom> = drafts
        .into_iter()
        .enumerate()
        .map(|(i, (file, kind, leave_by_default))| {
            file_atoms[file].push(i);
            Atom {
                id: AtomId(i as u32 + 1),
                file,
                kind,
                leave_by_default,
            }
        })
        .collect();

    let mut changes = Changes {
        files,
        atoms,
        file_atoms,
        binds: Vec::new(),
    };
    changes.binds = structural_binds(repo, snap, &changes)?;
    Ok(changes)
}

/// Renames (B3, via `diff-tree -M` between HEAD and the all-atoms tree) and
/// directory/file swaps.
fn structural_binds(repo: &Repo, snap: &Snapshot, changes: &Changes) -> Result<Vec<Bind>> {
    let whole_atom = |file: usize| {
        changes.file_atoms[file]
            .iter()
            .map(|&i| &changes.atoms[i])
            .find(|a| matches!(a.kind, AtomKind::Whole(_)))
            .map(|a| a.id)
    };
    let mut binds = Vec::new();

    let by_path: HashMap<&[u8], usize> = changes
        .files
        .iter()
        .enumerate()
        .map(|(i, f)| (f.path.as_slice(), i))
        .collect();
    for (i, dir) in changes.files.iter().enumerate() {
        for (j, inside) in changes.files.iter().enumerate() {
            let nested = inside.path.len() > dir.path.len()
                && inside.path.starts_with(&dir.path)
                && inside.path[dir.path.len()] == b'/';
            if i != j && nested {
                if let (Some(a), Some(b)) = (whole_atom(i), whole_atom(j)) {
                    binds.push(Bind {
                        a,
                        b,
                        reason: BindReason::DirectoryFile,
                    });
                }
            }
        }
    }

    let has_deleted = changes.files.iter().any(|f| f.new.is_none());
    let has_added = changes.files.iter().any(|f| f.base.is_none());
    if has_deleted && has_added {
        let mut builder = TreeBuilder::new(repo, "renames", &snap.head.tree)?;
        let all = |_: AtomId| true;
        let updates = (0..changes.files.len())
            .map(|i| (changes.files[i].path.clone(), changes.target(i, &all)))
            .collect();
        builder.set(updates)?;
        let final_tree = builder.write()?;
        let out = repo.git.run([
            "diff-tree",
            "-r",
            "-M",
            "-z",
            "--name-status",
            &snap.head.tree,
            &final_tree,
        ])?;
        let mut fields = out.split(|&b| b == 0).filter(|f| !f.is_empty());
        while let Some(status) = fields.next() {
            let pair = status.first() == Some(&b'R') || status.first() == Some(&b'C');
            let from = fields.next();
            let to = if pair { fields.next() } else { None };
            if let (true, Some(from), Some(to)) = (status.first() == Some(&b'R'), from, to) {
                let atoms = (by_path.get(from).copied().and_then(whole_atom))
                    .zip(by_path.get(to).copied().and_then(whole_atom));
                if let Some((a, b)) = atoms {
                    binds.push(Bind {
                        a,
                        b,
                        reason: BindReason::Rename,
                    });
                }
            }
        }
    }
    Ok(binds)
}
