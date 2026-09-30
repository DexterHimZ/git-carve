//! Tier B dependencies (DECISIONS.md D23): each window's historical `node_modules`, installed
//! once per lockfile into the eval cache and linked into the *cache clone* only. The user's
//! own repositories are never touched. Install scripts never run, and every package-manager
//! cache and store is redirected into the eval cache.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::{Result, io};
use crate::git::Git;

/// Lockfile versions installed per source (D23), the most common ones by window count.
pub const MAX_LOCKFILES_PER_SOURCE: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Manager {
    Npm,
    YarnClassic,
    Pnpm(&'static str),
}

/// The lockfile at `base` and how to install it, or why tier B cannot.
pub fn lockfile(
    git: &Git,
    base: &str,
) -> Result<std::result::Result<(String, String, Manager), String>> {
    let names = git.run_str(["ls-tree", "--name-only", base])?;
    let has = |n: &str| names.lines().any(|l| l == n);
    let (file, manager) = if has("package-lock.json") {
        ("package-lock.json", Manager::Npm)
    } else if has("pnpm-lock.yaml") {
        let head = git.run(["cat-file", "blob", &format!("{base}:pnpm-lock.yaml")])?;
        let text = String::from_utf8_lossy(&head[..head.len().min(200)]).into_owned();
        let version = text
            .lines()
            .find_map(|l| l.strip_prefix("lockfileVersion:"))
            .unwrap_or("")
            .trim()
            .trim_matches('\'')
            .to_owned();
        let pnpm = match version.chars().next() {
            Some('5') => "pnpm@7.33.7",
            Some('6') => "pnpm@8.15.9",
            Some('9') => "pnpm@9.15.9",
            _ => return Ok(Err(format!("unknown pnpm lockfile version {version:?}"))),
        };
        ("pnpm-lock.yaml", Manager::Pnpm(pnpm))
    } else if has("yarn.lock") {
        let head = git.run(["cat-file", "blob", &format!("{base}:yarn.lock")])?;
        if !String::from_utf8_lossy(&head[..head.len().min(400)]).contains("yarn lockfile v1") {
            return Ok(Err("yarn berry lockfile".into()));
        }
        ("yarn.lock", Manager::YarnClassic)
    } else if has("bun.lockb") || has("bun.lock") {
        return Ok(Err("bun lockfile (bun is not installed)".into()));
    } else {
        return Ok(Err("no lockfile".into()));
    };
    let oid = git.run_str(["rev-parse", &format!("{base}:{file}")])?;
    Ok(Ok((file.to_owned(), oid, manager)))
}

/// Installs (once) the dependencies for a lockfile; returns the `node_modules` directory.
pub fn install(
    git: &Git,
    base: &str,
    file: &str,
    oid: &str,
    manager: &Manager,
    cache: &Path,
) -> Result<std::result::Result<PathBuf, String>> {
    let dir = cache.join("deps").join(oid);
    let modules = dir.join("node_modules");
    let failed = dir.join("FAILED");
    if failed.exists() {
        return Ok(Err(
            std::fs::read_to_string(&failed).unwrap_or_else(|_| "install failed".into())
        ));
    }
    if dir.join("OK").exists() {
        return Ok(Ok(modules));
    }
    std::fs::create_dir_all(&dir).map_err(io(format!("create {}", dir.display())))?;
    for name in ["package.json", file] {
        let content = git.run(["cat-file", "blob", &format!("{base}:{name}")])?;
        std::fs::write(dir.join(name), content).map_err(io(format!("write {name}")))?;
    }
    let stores = cache.join("stores");
    let (program, args): (&str, Vec<String>) = match manager {
        Manager::Npm => (
            "npm",
            vec![
                "ci".into(),
                "--ignore-scripts".into(),
                "--no-audit".into(),
                "--no-fund".into(),
                "--loglevel=error".into(),
                format!("--cache={}", stores.join("npm").display()),
            ],
        ),
        Manager::YarnClassic => (
            "corepack",
            vec![
                "yarn@1.22.22".into(),
                "install".into(),
                "--frozen-lockfile".into(),
                "--ignore-scripts".into(),
                "--non-interactive".into(),
                "--silent".into(),
                format!("--cache-folder={}", stores.join("yarn").display()),
            ],
        ),
        Manager::Pnpm(version) => (
            "corepack",
            vec![
                (*version).into(),
                "install".into(),
                "--frozen-lockfile".into(),
                "--ignore-scripts".into(),
                format!("--store-dir={}", stores.join("pnpm").display()),
            ],
        ),
    };
    let out = Command::new(program)
        .args(&args)
        .current_dir(&dir)
        .env("COREPACK_HOME", stores.join("corepack"))
        .env("COREPACK_ENABLE_DOWNLOAD_PROMPT", "0")
        .env("COREPACK_ENABLE_STRICT", "0")
        .env("CI", "1")
        .stdin(Stdio::null())
        .output()
        .map_err(io(format!("run {program}")))?;
    if !out.status.success() {
        let tail: String = String::from_utf8_lossy(&out.stderr)
            .lines()
            .rev()
            .take(3)
            .collect::<Vec<_>>()
            .join(" | ");
        let reason = format!("install failed: {tail}");
        let _ = std::fs::write(&failed, &reason);
        return Ok(Err(reason));
    }
    std::fs::write(dir.join("OK"), b"").map_err(io("mark install"))?;
    Ok(Ok(modules))
}

/// Links `node_modules` into the cache clone and hides it from git there.
pub fn link(clone: &Path, modules: &Path) -> Result<()> {
    let target = clone.join("node_modules");
    if std::fs::symlink_metadata(&target).is_ok() {
        let _ = std::fs::remove_file(&target);
    }
    std::os::unix::fs::symlink(modules, &target)
        .map_err(io("link node_modules into the cache clone"))?;
    let exclude = clone.join(".git/info/exclude");
    let current = std::fs::read_to_string(&exclude).unwrap_or_default();
    if !current.lines().any(|l| l == "/node_modules") {
        std::fs::create_dir_all(clone.join(".git/info")).map_err(io("create .git/info"))?;
        std::fs::write(&exclude, format!("{current}/node_modules\n"))
            .map_err(io("write .git/info/exclude"))?;
    }
    Ok(())
}

/// For each source, the lockfile ids tier B installs: the most common by window count.
pub fn allowed(
    counts: &BTreeMap<String, BTreeMap<String, usize>>,
) -> BTreeMap<String, Vec<String>> {
    counts
        .iter()
        .map(|(source, by_oid)| {
            let mut v: Vec<(&String, &usize)> = by_oid.iter().collect();
            v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            (
                source.clone(),
                v.into_iter()
                    .take(MAX_LOCKFILES_PER_SOURCE)
                    .map(|(o, _)| o.clone())
                    .collect(),
            )
        })
        .collect()
}
