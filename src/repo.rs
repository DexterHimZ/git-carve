//! The repository carve runs in: its top level, its `<git-path carve>` directory, its object
//! format, and object-level helpers shared by the other modules.

use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result, io, precondition};
use crate::git::{Git, GitError, ObjectFormat};

pub struct Repo {
    /// Runs in the top level, with the caller's extra environment.
    pub git: Git,
    pub toplevel: PathBuf,
    /// `$(git rev-parse --git-path carve)`, absolute. Everything carve writes that is not a
    /// git object, a ref, or the final index lives here.
    pub carve_dir: PathBuf,
    pub format: ObjectFormat,
}

impl Repo {
    /// Opens the repository that contains `git.dir()`, keeping `git`'s environment.
    pub fn discover(git: Git) -> Result<Self> {
        let inside = match git.run_str(["rev-parse", "--is-inside-work-tree"]) {
            Ok(answer) => answer == "true",
            Err(GitError::Failed { .. }) => {
                return Err(precondition("not inside a git repository", None));
            }
            Err(err) => return Err(err.into()),
        };
        if !inside {
            return Err(precondition(
                "not inside a work tree (a bare repository, or inside .git)",
                None,
            ));
        }
        let toplevel = PathBuf::from(git.run_str(["rev-parse", "--show-toplevel"])?);
        let git = git.with_dir(&toplevel);
        let carve_dir = toplevel.join(git.run_str(["rev-parse", "--git-path", "carve"])?);
        let format = git.object_format()?;
        Ok(Self {
            git,
            toplevel,
            carve_dir,
            format,
        })
    }

    /// The filesystem path of a repository-relative path.
    pub fn path_of(&self, rel: &[u8]) -> PathBuf {
        self.toplevel.join(OsStr::from_bytes(rel))
    }

    pub fn config_bool(&self, key: &str, default: bool) -> Result<bool> {
        Ok(
            match self.git.try_run_str(["config", "--type=bool", key])? {
                Some(value) => value == "true",
                None => default,
            },
        )
    }

    pub fn config_u64(&self, key: &str, default: u64) -> Result<u64> {
        match self.git.try_run_str(["config", "--type=int", key])? {
            Some(value) => value
                .parse()
                .map_err(|_| Error::Invalid(format!("{key} = {value} is not a count"))),
            None => Ok(default),
        }
    }

    pub fn ensure_carve_dir(&self) -> Result<&Path> {
        fs::create_dir_all(&self.carve_dir)
            .map_err(io(format!("create {}", self.carve_dir.display())))?;
        Ok(&self.carve_dir)
    }

    /// Takes the per-repository run lock. Dropping the guard releases it.
    pub fn lock(&self) -> Result<Lock> {
        let path = self.ensure_carve_dir()?.join("lock");
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                let _ = writeln!(file, "{}", std::process::id());
                Ok(Lock { path })
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Err(precondition(
                "another git carve is running in this repository",
                Some(&format!("if none is running, delete {}", path.display())),
            )),
            Err(err) => Err(io(format!("create {}", path.display()))(err)),
        }
    }

    /// Writes each blob byte for byte (no filters) and returns the object ids, in order.
    pub fn write_blobs(&self, blobs: &[&[u8]]) -> Result<Vec<String>> {
        if blobs.is_empty() {
            return Ok(Vec::new());
        }
        let dir = self.ensure_carve_dir()?.join("blobs.tmp");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).map_err(io(format!("create {}", dir.display())))?;
        let mut paths = Vec::with_capacity(blobs.len());
        for (i, blob) in blobs.iter().enumerate() {
            let path = dir.join(i.to_string());
            fs::write(&path, blob).map_err(io(format!("write {}", path.display())))?;
            paths.push(path);
        }
        let args = ["hash-object", "-w", "--no-filters", "--"]
            .into_iter()
            .map(OsStr::new)
            .chain(paths.iter().map(|p| p.as_os_str()));
        let result = self.git.run(args);
        let _ = fs::remove_dir_all(&dir);
        self.parse_oids(&result?, blobs.len())
    }

    /// Hashes working-tree files the way `git add` would (clean filters, end-of-line
    /// conversion) and writes the blobs. Never pass symlinks: hash-object follows them.
    pub fn hash_worktree_files(&self, rel_paths: &[&[u8]]) -> Result<Vec<String>> {
        if rel_paths.is_empty() {
            return Ok(Vec::new());
        }
        let args = ["hash-object", "-w", "--"]
            .into_iter()
            .map(OsStr::new)
            .chain(rel_paths.iter().map(|p| OsStr::from_bytes(p)));
        let out = self.git.run(args)?;
        self.parse_oids(&out, rel_paths.len())
    }

    /// Sizes of blobs, in order.
    pub fn blob_sizes(&self, oids: &[&str]) -> Result<Vec<u64>> {
        if oids.is_empty() {
            return Ok(Vec::new());
        }
        let out = self
            .git
            .run_with_stdin(["cat-file", "--batch-check"], oids.join("\n").as_bytes())?;
        let text = String::from_utf8_lossy(&out);
        let sizes: Vec<u64> = text
            .lines()
            .map(|line| {
                let mut fields = line.split(' ');
                match (fields.next(), fields.next(), fields.next()) {
                    (Some(_), Some("blob"), Some(size)) => size.parse().ok(),
                    _ => None,
                }
            })
            .collect::<Option<_>>()
            .ok_or_else(|| Error::Internal(format!("unexpected cat-file output: {text}")))?;
        if sizes.len() != oids.len() {
            return Err(Error::Internal("cat-file returned the wrong count".into()));
        }
        Ok(sizes)
    }

    /// Contents of blobs, in order.
    pub fn read_blobs(&self, oids: &[&str]) -> Result<Vec<Vec<u8>>> {
        if oids.is_empty() {
            return Ok(Vec::new());
        }
        let out = self
            .git
            .run_with_stdin(["cat-file", "--batch"], oids.join("\n").as_bytes())?;
        let mut rest = out.as_slice();
        let mut blobs = Vec::with_capacity(oids.len());
        while !rest.is_empty() {
            let newline = rest
                .iter()
                .position(|&b| b == b'\n')
                .ok_or_else(|| Error::Internal("truncated cat-file header".into()))?;
            let header = String::from_utf8_lossy(&rest[..newline]).into_owned();
            let size: usize = header
                .rsplit(' ')
                .next()
                .and_then(|s| s.parse().ok())
                .filter(|_| header.split(' ').nth(1) == Some("blob"))
                .ok_or_else(|| Error::Internal(format!("unexpected cat-file header: {header}")))?;
            let start = newline + 1;
            if rest.len() < start + size + 1 {
                return Err(Error::Internal("truncated cat-file body".into()));
            }
            blobs.push(rest[start..start + size].to_vec());
            rest = &rest[start + size + 1..];
        }
        if blobs.len() != oids.len() {
            return Err(Error::Internal("cat-file returned the wrong count".into()));
        }
        Ok(blobs)
    }

    fn parse_oids(&self, out: &[u8], expected: usize) -> Result<Vec<String>> {
        let oids: Vec<String> = String::from_utf8_lossy(out)
            .lines()
            .map(str::to_owned)
            .collect();
        let valid = oids.len() == expected
            && oids.iter().all(|oid| {
                oid.len() == self.format.hex_len() && oid.bytes().all(|b| b.is_ascii_hexdigit())
            });
        if !valid {
            return Err(Error::Internal(format!(
                "expected {expected} object ids, got {oids:?}"
            )));
        }
        Ok(oids)
    }
}

/// Held for the duration of a run; removes `<git-path carve>/lock` when dropped.
pub struct Lock {
    path: PathBuf,
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
