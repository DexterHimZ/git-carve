//! A read-only snapshot of HEAD and every changed path (SPEC §5.2). The working tree is
//! only ever read; blobs are written in repository ("clean") form.

use std::collections::HashMap;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::time::UNIX_EPOCH;

use crate::error::{Error, Result, io, precondition};
use crate::model::{Entry, Mode, show};
use crate::precondition::{MAX_PATHS, staged_changes};
use crate::repo::Repo;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Head {
    pub commit: String,
    pub tree: String,
    /// `refs/heads/<name>`, or `None` when HEAD is detached.
    pub branch: Option<String>,
}

/// Size and modification time, part of the race-guard fingerprint (§10.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    pub size: u64,
    pub mtime_ns: i128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileSnap {
    pub path: Vec<u8>,
    /// The entry in HEAD; `None` for an untracked file.
    pub base: Option<Entry>,
    /// The working-tree entry in clean form; `None` for a deleted file.
    pub new: Option<Entry>,
    pub stat: Option<Stat>,
    /// The path has a `filter` attribute (LFS and friends): whole-file only.
    pub filtered: bool,
    /// `-diff` or `binary` attribute: whole-file only.
    pub binary_attr: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub head: Head,
    /// Sorted by path bytes.
    pub files: Vec<FileSnap>,
}

struct Changed {
    path: Vec<u8>,
    base: Option<Entry>,
    deleted: bool,
    /// Git's view of the working-tree mode for tracked paths (respects core.fileMode).
    tracked_mode: Option<Mode>,
}

pub fn take(repo: &Repo) -> Result<Snapshot> {
    let head = read_head(repo)?;
    let status = repo.git.run([
        "status",
        "--porcelain=v2",
        "-z",
        "--untracked-files=all",
        "--no-renames",
    ])?;
    let changed = parse_status(&status)?;
    if changed.is_empty() {
        return Err(precondition(
            "nothing to carve: the working tree is clean",
            None,
        ));
    }
    if changed.len() > MAX_PATHS {
        return Err(precondition(
            format!("{} changed paths is more than {MAX_PATHS}", changed.len()),
            Some("commit part of the work first"),
        ));
    }

    let filemode = repo.config_bool("core.fileMode", true)?;
    let mut modes = vec![None; changed.len()];
    let mut stats = vec![None; changed.len()];
    let mut regular = Vec::new();
    let mut links = Vec::new();
    for (i, c) in changed.iter().enumerate() {
        if c.deleted {
            continue;
        }
        let fs_path = repo.path_of(&c.path);
        let meta =
            fs::symlink_metadata(&fs_path).map_err(io(format!("read {}", fs_path.display())))?;
        stats[i] = Some(Stat {
            size: meta.len(),
            mtime_ns: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos() as i128),
        });
        let kind = meta.file_type();
        if kind.is_symlink() {
            let target =
                fs::read_link(&fs_path).map_err(io(format!("read link {}", fs_path.display())))?;
            links.push((i, target.into_os_string().into_vec()));
            modes[i] = Some(Mode::LINK);
        } else if kind.is_file() {
            let mode = match c.tracked_mode {
                Some(mode) if mode.is_regular() => mode,
                Some(_) => {
                    return Err(Error::Concurrent(format!(
                        "{} changed while carve was reading it; re-run",
                        show(&c.path)
                    )));
                }
                None if filemode && meta.permissions().mode() & 0o111 != 0 => Mode::EXEC,
                None => Mode::FILE,
            };
            regular.push(i);
            modes[i] = Some(mode);
        } else {
            return Err(precondition(
                format!("{} is not a regular file or symlink", show(&c.path)),
                None,
            ));
        }
    }

    let mut oids: Vec<Option<String>> = vec![None; changed.len()];
    let paths: Vec<&[u8]> = regular
        .iter()
        .map(|&i| changed[i].path.as_slice())
        .collect();
    for (&i, oid) in regular.iter().zip(repo.hash_worktree_files(&paths)?) {
        oids[i] = Some(oid);
    }
    let targets: Vec<&[u8]> = links.iter().map(|(_, t)| t.as_slice()).collect();
    for ((i, _), oid) in links.iter().zip(repo.write_blobs(&targets)?) {
        oids[*i] = Some(oid);
    }

    let attrs = attributes(repo, &changed)?;
    let mut files: Vec<FileSnap> = changed
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            let (filtered, binary_attr) = attrs.get(&c.path).copied().unwrap_or_default();
            let new = match (modes[i], oids[i].take()) {
                (Some(mode), Some(oid)) => Some(Entry { mode, oid }),
                _ => None,
            };
            FileSnap {
                path: c.path,
                base: c.base,
                new,
                stat: stats[i],
                filtered,
                binary_attr,
            }
        })
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Snapshot { head, files })
}

/// `Some(reason)` if HEAD, the branch, or any changed path differs from `before` (§10.3).
pub fn changed_since(repo: &Repo, before: &Snapshot) -> Result<Option<String>> {
    let after = match take(repo) {
        Ok(after) => after,
        Err(Error::Precondition(reason)) => return Ok(Some(reason)),
        Err(err) => return Err(err),
    };
    if after.head != before.head {
        return Ok(Some("HEAD moved".into()));
    }
    if after.files.len() != before.files.len() {
        return Ok(Some("the set of changed files changed".into()));
    }
    for (a, b) in after.files.iter().zip(&before.files) {
        if a != b {
            return Ok(Some(format!("{} changed", show(&b.path))));
        }
    }
    Ok(None)
}

fn read_head(repo: &Repo) -> Result<Head> {
    let commit = repo
        .git
        .try_run_str(["rev-parse", "--verify", "-q", "HEAD^{commit}"])?
        .ok_or_else(|| precondition("HEAD has no commits yet", None))?;
    let tree = repo
        .git
        .run_str(["rev-parse", &format!("{commit}^{{tree}}")])?;
    let branch = repo.git.try_run_str(["symbolic-ref", "-q", "HEAD"])?;
    Ok(Head {
        commit,
        tree,
        branch,
    })
}

fn parse_status(raw: &[u8]) -> Result<Vec<Changed>> {
    let mut changed = Vec::new();
    for record in raw.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        match record[0] {
            b'1' => {
                let fields: Vec<&[u8]> = record.splitn(9, |&b| b == b' ').collect();
                let [_, xy, sub, m_head, _, m_work, h_head, _, path] = fields[..] else {
                    return Err(Error::Internal(format!(
                        "unexpected status record: {}",
                        show(record)
                    )));
                };
                if sub != b"N..." {
                    return Err(precondition(
                        format!(
                            "submodule {} changed; submodules are not supported",
                            show(path)
                        ),
                        None,
                    ));
                }
                match (xy[0], xy[1]) {
                    (_, b'A') => {
                        return Err(precondition(
                            format!("{} is marked intent-to-add", show(path)),
                            Some("`git reset` clears it and keeps the file in your working tree"),
                        ));
                    }
                    (b'.', _) => {}
                    _ => return Err(staged_changes()),
                }
                let bad_mode = || Error::Internal(format!("bad mode in status: {}", show(record)));
                let base = Entry {
                    mode: Mode::parse(m_head).ok_or_else(bad_mode)?,
                    oid: String::from_utf8_lossy(h_head).into_owned(),
                };
                let deleted = xy[1] == b'D';
                if !matches!(xy[1], b'M' | b'T' | b'D') {
                    return Err(Error::Internal(format!(
                        "unexpected status code: {}",
                        show(record)
                    )));
                }
                changed.push(Changed {
                    path: path.to_vec(),
                    base: Some(base),
                    deleted,
                    tracked_mode: if deleted {
                        None
                    } else {
                        Some(Mode::parse(m_work).ok_or_else(bad_mode)?)
                    },
                });
            }
            b'2' => return Err(staged_changes()),
            b'u' => {
                return Err(precondition(
                    "the index has unmerged entries",
                    Some("resolve the conflicts first"),
                ));
            }
            b'?' => {
                let path = &record[2..];
                if path.ends_with(b"/") {
                    return Err(precondition(
                        format!(
                            "{} is a nested git repository; submodules are not supported",
                            show(path)
                        ),
                        None,
                    ));
                }
                changed.push(Changed {
                    path: path.to_vec(),
                    base: None,
                    deleted: false,
                    tracked_mode: None,
                });
            }
            b'#' | b'!' => {}
            _ => {
                return Err(Error::Internal(format!(
                    "unexpected status record: {}",
                    show(record)
                )));
            }
        }
    }
    Ok(changed)
}

/// `(filtered, binary)` per path, from `.gitattributes`.
fn attributes(repo: &Repo, changed: &[Changed]) -> Result<HashMap<Vec<u8>, (bool, bool)>> {
    let mut input = Vec::new();
    for c in changed {
        input.extend_from_slice(&c.path);
        input.push(0);
    }
    let out = repo.git.run_with_stdin(
        ["check-attr", "-z", "--stdin", "filter", "diff", "binary"],
        &input,
    )?;
    let mut attrs: HashMap<Vec<u8>, (bool, bool)> = HashMap::new();
    let parts: Vec<&[u8]> = out.split(|&b| b == 0).collect();
    for triple in parts.chunks_exact(3) {
        let [path, attr, info] = triple else { continue };
        let entry = attrs.entry(path.to_vec()).or_default();
        let specified = *info != b"unspecified" && *info != b"unset";
        match *attr {
            b"filter" if specified => entry.0 = true,
            b"diff" if *info == b"unset" => entry.1 = true,
            b"binary" if *info == b"set" => entry.1 = true,
            _ => {}
        }
    }
    Ok(attrs)
}
