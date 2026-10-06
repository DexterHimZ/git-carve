//! Throwaway repositories for integration tests. All git access goes through `git_carve::git`.
#![allow(dead_code)] // each test crate uses a different subset

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::UNIX_EPOCH;

use git_carve::atom::{AtomId, Changes, atomize};
use git_carve::commit::{Group, Hooks, UndoRecord, commit};
use git_carve::facts::{self, Analysis};
use git_carve::git::Git;
use git_carve::graph::{self, Plan};
use git_carve::repo::Repo;
use git_carve::verify::{self, Verification};
use git_carve::{precondition, snapshot};
use tempfile::TempDir;

/// Isolates tests from the machine's git config and pins identities and dates, so object
/// ids are reproducible.
pub const ISOLATION: &[(&str, &str)] = &[
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_AUTHOR_NAME", "Carve Test"),
    ("GIT_AUTHOR_EMAIL", "carve@example.com"),
    ("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z"),
    ("GIT_COMMITTER_NAME", "Carve Test"),
    ("GIT_COMMITTER_EMAIL", "carve@example.com"),
    ("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z"),
];

pub fn isolated(git: Git) -> Git {
    ISOLATION.iter().fold(git, |git, (k, v)| git.env(k, v))
}

pub struct TestRepo {
    dir: TempDir,
    git: Git,
}

impl TestRepo {
    /// An empty SHA-1 repository on branch `main`.
    pub fn new() -> Self {
        Self::init("sha1")
    }

    /// An empty SHA-256 repository on branch `main`.
    pub fn sha256() -> Self {
        Self::init("sha256")
    }

    fn init(object_format: &str) -> Self {
        let dir = tempfile::tempdir().expect("create temp dir");
        let git = isolated(Git::new(dir.path()));
        let format = format!("--object-format={object_format}");
        git.run(["init", "-q", "-b", "main", &format])
            .expect("git init");
        git.run(["config", "core.autocrlf", "false"])
            .expect("git config");
        Self { dir, git }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn git(&self) -> &Git {
        &self.git
    }

    /// Runs git and returns trimmed stdout, panicking with stderr on failure.
    pub fn run(&self, args: &[&str]) -> String {
        self.git
            .run_str(args)
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"))
    }

    /// Writes a file relative to the repository root, creating parent directories.
    pub fn write(&self, rel: &str, contents: impl AsRef<[u8]>) {
        let path = self.path().join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(path, contents).expect("write file");
    }

    pub fn remove(&self, rel: &str) {
        fs::remove_file(self.path().join(rel)).expect("remove file");
    }

    pub fn symlink(&self, target: &str, rel: &str) {
        let path = self.path().join(rel);
        let _ = fs::remove_file(&path);
        std::os::unix::fs::symlink(target, path).expect("symlink");
    }

    pub fn set_exec(&self, rel: &str, exec: bool) {
        let path = self.path().join(rel);
        let mode = if exec { 0o755 } else { 0o644 };
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
    }

    /// Stages everything and commits it. Returns the new commit id.
    pub fn commit_all(&self, message: &str) -> String {
        self.run(&["add", "-A"]);
        self.run(&["commit", "-q", "-m", message]);
        self.run(&["rev-parse", "HEAD"])
    }

    pub fn head_tree(&self) -> String {
        self.run(&["rev-parse", "HEAD^{tree}"])
    }

    /// A `git-carve` command that runs inside this repository with the same isolation.
    pub fn carve(&self) -> Command {
        carve_in(self.path())
    }

    pub fn open(&self) -> Repo {
        Repo::discover(self.git.clone()).expect("open repo")
    }

    pub fn changes(&self) -> Changes {
        let repo = self.open();
        let snap = snapshot::take(&repo).expect("snapshot");
        atomize(&repo, &snap).expect("atomize")
    }

    /// The code graph's plan for the current working tree. Asserts R4: every atom is in
    /// exactly one group or in leave.
    pub fn plan(&self) -> (Changes, Analysis, Plan) {
        let repo = self.open();
        let snap = snapshot::take(&repo).expect("snapshot");
        let changes = atomize(&repo, &snap).expect("atomize");
        let analysis = facts::analyze(&repo, &changes).expect("analyze");
        let plan = graph::plan(&changes, &analysis);
        let mut seen: Vec<AtomId> = plan
            .groups
            .iter()
            .flatten()
            .chain(&plan.leave)
            .copied()
            .collect();
        seen.sort_unstable();
        let all: Vec<AtomId> = changes.atoms.iter().map(|a| a.id).collect();
        assert_eq!(seen, all, "R4: every atom exactly once");
        (changes, analysis, plan)
    }

    /// The plan after verification with `check` (`None`: the default check).
    pub fn verified(
        &self,
        check: Option<&str>,
    ) -> git_carve::Result<(Changes, Plan, Verification)> {
        let (changes, analysis, plan) = self.plan();
        let repo = self.open();
        let snap = snapshot::take(&repo)?;
        let carved: Vec<&[u8]> = changes.files.iter().map(|f| f.path.as_slice()).collect();
        let spec = git_carve::check::resolve(&repo, check, &carved)?;
        let mut checker = git_carve::check::Checker::new(&repo, spec, &carved)?;
        let (plan, verification) =
            verify::verify(&repo, &snap, &changes, &analysis, &plan, &mut checker)?;
        Ok((changes, plan, verification))
    }

    /// Verifies a hand-made grouping instead of the graph's (for rules the graph avoids).
    pub fn verified_groups(
        &self,
        check: &str,
        groups: impl FnOnce(&Changes) -> Vec<Vec<AtomId>>,
    ) -> git_carve::Result<(Changes, Plan, Verification)> {
        let (changes, analysis, mut plan) = self.plan();
        plan.groups = groups(&changes);
        plan.leave.clear();
        plan.style = None;
        let repo = self.open();
        let snap = snapshot::take(&repo)?;
        let carved: Vec<&[u8]> = changes.files.iter().map(|f| f.path.as_slice()).collect();
        let spec = git_carve::check::resolve(&repo, Some(check), &carved)?;
        let mut checker = git_carve::check::Checker::new(&repo, spec, &carved)?;
        let (plan, verification) =
            verify::verify(&repo, &snap, &changes, &analysis, &plan, &mut checker)?;
        Ok((changes, plan, verification))
    }

    /// Links a real TypeScript into `node_modules`, as an install would. `false` when no
    /// TypeScript is available (the caller skips): set GIT_CARVE_TEST_TYPESCRIPT, or
    /// `npm install -g typescript`.
    pub fn link_typescript(&self) -> bool {
        let Some(pkg) = typescript_package() else {
            eprintln!("skipping: no TypeScript found (npm install -g typescript)");
            return false;
        };
        let bin = self.path().join("node_modules/.bin");
        fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(&pkg, self.path().join("node_modules/typescript")).unwrap();
        std::os::unix::fs::symlink("../typescript/bin/tsc", bin.join("tsc")).unwrap();
        true
    }

    /// A repository built by `scripts/make-demo-repo.sh` (SPEC §14.1).
    pub fn demo() -> Self {
        let dir = tempfile::tempdir().expect("create temp dir");
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/make-demo-repo.sh");
        let out = Command::new("bash")
            .arg(script)
            .arg(dir.path())
            .envs(ISOLATION.iter().copied())
            .output()
            .expect("run demo script");
        assert!(out.status.success(), "{}", stderr(&out));
        let git = isolated(Git::new(dir.path()));
        Self { dir, git }
    }

    /// Carves through the library, as the CLI will: `choose` picks the groups, and each
    /// group gets a message the way a user would write one.
    pub fn carve_lib(
        &self,
        choose: impl FnOnce(&Changes) -> Vec<Vec<AtomId>>,
        hooks: &Hooks,
    ) -> git_carve::Result<UndoRecord> {
        let repo = self.open();
        precondition::check_state(&repo)?;
        let snap = snapshot::take(&repo)?;
        let changes = atomize(&repo, &snap)?;
        let groups: Vec<Group> = choose(&changes)
            .into_iter()
            .enumerate()
            .map(|(k, atoms)| Group {
                atoms,
                message: format!("Commit {}\n", k + 1),
            })
            .collect();
        commit(&repo, &snap, &changes, &groups, hooks)
    }

    /// Every path under the work tree except `.git`, with its kind, mode bits, size,
    /// modification time, and bytes. carve must never change any of it.
    pub fn worktree_digest(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut digest = BTreeMap::new();
        walk(self.path(), self.path(), &mut digest);
        digest
    }

    /// What a user can observe: HEAD, the branch, status, unstaged and staged diffs.
    pub fn observable(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for args in [
            &["rev-parse", "HEAD"][..],
            &["symbolic-ref", "-q", "HEAD"],
            &["status", "--porcelain=v2", "-z", "--untracked-files=all"],
            &["diff", "--binary"],
            &["diff", "--cached", "--binary"],
        ] {
            out.extend(self.git.run(args).unwrap_or_default());
            out.extend(b"\0--\0");
        }
        out
    }

    /// The tree `git add -A` would commit right now, built in a throwaway index. An
    /// oracle that shares no code with carve.
    pub fn oracle_tree(&self) -> String {
        let dir = tempfile::tempdir().expect("temp dir");
        let git = self
            .git
            .clone()
            .env("GIT_INDEX_FILE", dir.path().join("index"));
        git.run(["read-tree", "HEAD"]).expect("read-tree");
        git.run(["add", "-A"]).expect("add -A");
        git.run_str(["write-tree"]).expect("write-tree")
    }
}

pub fn carve_in(dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_git-carve"));
    cmd.current_dir(dir).envs(ISOLATION.iter().copied());
    cmd
}

pub fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Every atom, in one group.
pub fn one_group(changes: &Changes) -> Vec<Vec<AtomId>> {
    vec![changes.atoms.iter().map(|a| a.id).collect()]
}

/// The M1 grouping: one group per file, joined by hard binds, with default leaves left out.
pub fn per_file(changes: &Changes) -> Vec<Vec<AtomId>> {
    git_carve::group::per_file(changes).groups
}

/// Atom ids in the file at `path`.
pub fn atoms_in(changes: &Changes, path: &str) -> Vec<AtomId> {
    changes
        .atoms
        .iter()
        .filter(|a| changes.files[a.file].path == path.as_bytes())
        .map(|a| a.id)
        .collect()
}

/// Index of the group holding `id`, or `None` if it is left out.
pub fn group_of(plan: &Plan, id: AtomId) -> Option<usize> {
    plan.groups.iter().position(|g| g.contains(&id))
}

fn walk(root: &Path, dir: &Path, digest: &mut BTreeMap<PathBuf, Vec<u8>>) {
    for entry in fs::read_dir(dir).expect("read dir") {
        let path = entry.expect("dir entry").path();
        let rel = path.strip_prefix(root).unwrap().to_path_buf();
        if rel == Path::new(".git") {
            continue;
        }
        let meta = fs::symlink_metadata(&path).expect("stat");
        let mtime = meta
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut record =
            format!("{:o} {} {mtime}\n", meta.permissions().mode(), meta.len()).into_bytes();
        if meta.file_type().is_symlink() {
            record.extend(b"link:");
            record.extend(fs::read_link(&path).unwrap().as_os_str().as_encoded_bytes());
        } else if meta.is_dir() {
            record.extend(b"dir");
            walk(root, &path, digest);
        } else {
            record.extend(fs::read(&path).unwrap());
        }
        digest.insert(rel, record);
    }
}

fn typescript_package() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("GIT_CARVE_TEST_TYPESCRIPT") {
        return Some(PathBuf::from(path));
    }
    let out = Command::new("npm").args(["root", "-g"]).output().ok()?;
    let pkg = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()).join("typescript");
    pkg.join("bin/tsc").exists().then_some(pkg)
}

/// An editor stand-in: a shell script run as `$GIT_EDITOR <file>`. `body` sees the buffer
/// path as `$1`; `$STATE` is a scratch directory that persists across invocations.
pub fn editor(body: &str) -> (TempDir, String) {
    let dir = tempfile::tempdir().expect("editor dir");
    let script = dir.path().join("editor.sh");
    fs::write(
        &script,
        format!("#!/bin/sh\nSTATE={}\n{body}\n", dir.path().display()),
    )
    .expect("write editor");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod editor");
    let path = script.display().to_string();
    (dir, path)
}

/// Types "Commit <n> subject" under every "commit" line that has no message yet.
pub const TYPE_MESSAGES: &str = r#"awk '
  pending && /^[[:space:]]*@/ { printf "Commit %d subject\n", n; pending = 0 }
  pending && /^[^#[:space:]]/ { pending = 0 }
  /^commit$/ { n++; pending = 1 }
  { print }' "$1" > "$1.tmp" && mv "$1.tmp" "$1""#;
