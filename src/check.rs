//! Running the user's check (SPEC §8.1–§8.2): resolving the command, the scratch worktree
//! kept warm between runs, dependency links, timeouts, and a result cache whose key
//! includes a toolchain fingerprint (DECISIONS.md D13).

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, io, precondition};
use crate::git::Git;
use crate::lang::primary;
use crate::repo::Repo;

const DEFAULT_LINKS: &[&str] = &["node_modules", ".venv", "venv", "next-env.d.ts"];
const DEFAULT_TIMEOUT_SECS: u64 = 600;
const TAIL_LINES: usize = 40;
const CACHE_LIMIT: usize = 5000;
/// Install markers whose modification time says "dependencies changed" (D13).
const INSTALL_MARKERS: &[&str] = &[
    ".package-lock.json",
    ".modules.yaml",
    ".yarn-state.yml",
    ".yarn-integrity",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckSpec {
    pub command: String,
    /// What the summary shows.
    pub display: String,
    /// Given by `--check` or `carve.check`, rather than built by the adapter. A probe that
    /// cannot fail is fatal only for the default check (DECISIONS.md D1).
    pub explicit: bool,
    /// Extra directories to link into the scratch worktree (a sub-project's `node_modules`).
    pub links: Vec<String>,
}

/// `--check`, then `git config carve.check`, then the adapter's default (§8.1).
pub fn resolve(repo: &Repo, flag: Option<&str>, changed: &[&[u8]]) -> Result<CheckSpec> {
    if let Some(command) = flag {
        return Ok(explicit(command));
    }
    if let Some(command) = repo.git.try_run_str(["config", "carve.check"])? {
        return Ok(explicit(&command));
    }
    let configs: Vec<String> = String::from_utf8_lossy(&repo.git.run(["ls-files", "-z"])?)
        .split('\0')
        .filter(|p| {
            let name = p.rsplit('/').next().unwrap_or(p);
            name.starts_with("tsconfig") && name.ends_with(".json")
        })
        .map(str::to_owned)
        .collect();
    let changed: Vec<String> = changed
        .iter()
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect();
    let check = primary()
        .default_check(&repo.toplevel, &configs, &changed)
        .map_err(|reason| precondition(reason, None))?;
    Ok(CheckSpec {
        command: check.command,
        display: check.display,
        explicit: false,
        links: check.links,
    })
}

fn explicit(command: &str) -> CheckSpec {
    CheckSpec {
        command: command.to_owned(),
        display: command.to_owned(),
        explicit: true,
        links: Vec::new(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    pub passed: bool,
    /// Last lines of output (failures only). With a baseline (D43), only the new errors.
    pub tail: String,
    pub millis: u64,
    #[serde(skip)]
    pub cached: bool,
    /// Compiler diagnostics of a failed run, as printed (`path(line,col): error TS…`), in
    /// output order; `None` when none were recognized (DECISIONS.md D43).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errors: Option<Vec<String>>,
}

/// D43: `path(line,col): error TS1234: message` (or tsc's pretty `path:line:col - error …`),
/// ANSI colors removed. Global errors (`error TS5058: …`) count too.
fn diagnostics(output: &str) -> Option<Vec<String>> {
    let mut clean = String::with_capacity(output.len());
    let mut chars = output.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for d in chars.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            clean.push(c);
        }
    }
    let errors: Vec<String> = clean
        .lines()
        .map(str::trim_end)
        .filter(|l| {
            l.find("error TS").is_some_and(|at| {
                let head = &l[..at];
                head.is_empty() || head.ends_with("): ") || head.ends_with(" - ")
            })
        })
        .map(str::to_owned)
        .collect();
    (!errors.is_empty()).then_some(errors)
}

/// D43: an error's identity across trees: the line without its position, so an error that
/// merely moved is the same error.
pub fn error_key(line: &str) -> String {
    let at = line.find("error TS").unwrap_or(0);
    let (head, rest) = line.split_at(at);
    let path = head
        .trim_end_matches(": ")
        .trim_end_matches(" - ")
        .rsplit_once('(')
        .map_or_else(|| head.split(':').next().unwrap_or(head).trim(), |(p, _)| p);
    format!("{path}|{rest}")
}

#[derive(Serialize, Deserialize, Default)]
struct CacheFile {
    entries: Vec<(String, Outcome)>,
}

/// Runs the check on trees, in a scratch worktree kept between runs.
pub struct Checker<'r> {
    repo: &'r Repo,
    pub spec: CheckSpec,
    scratch: PathBuf,
    scratch_git: Git,
    fingerprint: String,
    timeout: Duration,
    cache: BTreeMap<String, Outcome>,
    order: Vec<String>,
    /// `--no-new-errors` (D43): if HEAD fails with recognizable errors, a tree passes when it
    /// adds none.
    pub no_new_errors: bool,
    /// HEAD's error keys, sorted (a multiset), once the baseline is set.
    baseline: Option<Vec<String>>,
    pub runs: usize,
    pub hits: usize,
    pub millis: u64,
}

impl<'r> Checker<'r> {
    /// `carved` lists the repository paths carve may commit: a dependency directory that
    /// holds one of them is not linked, because git would replace the link with a real
    /// directory while materializing trees.
    pub fn new(repo: &'r Repo, spec: CheckSpec, carved: &[&[u8]]) -> Result<Self> {
        let carve = repo.ensure_carve_dir()?.to_path_buf();
        let scratch = carve.join("scratch");
        if !scratch.join(".git").exists() {
            let _ = fs::remove_dir_all(&scratch);
            repo.git.run(["worktree", "prune"])?;
            let path = scratch.to_string_lossy();
            repo.git.run([
                "worktree",
                "add",
                "--detach",
                "--no-checkout",
                "-q",
                path.as_ref(),
                "HEAD",
            ])?;
        }
        fs::create_dir_all(carve.join("tsbuildinfo")).map_err(io("create tsbuildinfo dir"))?;
        let links = link(repo, &scratch, carved, &spec.links)?;
        let timeout =
            Duration::from_secs(repo.config_u64("carve.checkTimeout", DEFAULT_TIMEOUT_SECS)?);
        let fingerprint = format!(
            "{}|{}|{}",
            env!("CARGO_PKG_VERSION"),
            spec.command,
            links
                .iter()
                .map(|l| fingerprint(repo, l))
                .collect::<Vec<_>>()
                .join(";")
        );
        let (cache, order) = load_cache(&carve.join("checks.json"));
        let scratch_git = repo.git.with_dir(&scratch);
        Ok(Self {
            repo,
            spec,
            scratch,
            scratch_git,
            fingerprint,
            timeout,
            cache,
            order,
            no_new_errors: false,
            baseline: None,
            runs: 0,
            hits: 0,
            millis: 0,
        })
    }

    pub fn scratch(&self) -> &Path {
        &self.scratch
    }

    /// Checks one tree, from the cache when this exact tree, command, and toolchain passed
    /// or failed before. With a baseline (D43), a failure that adds no error beyond HEAD's
    /// passes, and its tail lists only the new errors.
    pub fn check(&mut self, tree: &str) -> Result<Outcome> {
        let key = format!("{tree}|{}", self.fingerprint);
        let raw = match self.cache.get(&key) {
            // A failure cached before errors were recorded cannot set or meet a baseline.
            Some(hit)
                if !((self.no_new_errors || self.baseline.is_some())
                    && !hit.passed
                    && hit.errors.is_none()) =>
            {
                self.hits += 1;
                Outcome {
                    cached: true,
                    ..hit.clone()
                }
            }
            _ => {
                // Updates only the files that differ, so incremental checkers stay warm.
                self.scratch_git.run(["read-tree", "-u", "--reset", tree])?;
                let outcome = self.run()?;
                self.runs += 1;
                self.millis += outcome.millis;
                self.store(key, outcome.clone())?;
                outcome
            }
        };
        Ok(self.against_baseline(raw))
    }

    /// D43: makes HEAD's failing outcome the baseline, when `--no-new-errors` is on and the
    /// failure is made of recognizable errors. Returns how many.
    pub fn set_baseline(&mut self, head: &Outcome) -> Option<usize> {
        if !self.no_new_errors || head.passed || head.tail.contains("check timed out") {
            return None;
        }
        let errors = head.errors.as_ref()?;
        let mut keys: Vec<String> = errors.iter().map(|l| error_key(l)).collect();
        keys.sort();
        let n = keys.len();
        self.baseline = Some(keys);
        Some(n)
    }

    fn against_baseline(&self, raw: Outcome) -> Outcome {
        let (Some(baseline), false) = (&self.baseline, raw.passed) else {
            return raw;
        };
        let Some(errors) = &raw.errors else {
            return raw;
        };
        if raw.tail.contains("check timed out") {
            return raw;
        }
        let mut allowance: BTreeMap<&str, usize> = BTreeMap::new();
        for k in baseline {
            *allowance.entry(k.as_str()).or_default() += 1;
        }
        let mut new = Vec::new();
        for line in errors {
            let key = error_key(line);
            match allowance.get_mut(key.as_str()) {
                Some(n) if *n > 0 => *n -= 1,
                _ => new.push(line.clone()),
            }
        }
        if new.is_empty() {
            Outcome {
                passed: true,
                tail: String::new(),
                ..raw
            }
        } else {
            Outcome {
                tail: new
                    .iter()
                    .take(TAIL_LINES)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n"),
                ..raw
            }
        }
    }

    fn run(&self) -> Result<Outcome> {
        let started = Instant::now();
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(&self.spec.command)
            .current_dir(&self.scratch)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("CARGO_TARGET_DIR", self.repo.carve_dir.join("target"))
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .process_group(0);
        let mut child = cmd
            .spawn()
            .map_err(io(format!("run check `{}`", self.spec.command)))?;
        let output = Arc::new(Mutex::new(Vec::new()));
        let readers: Vec<_> = [
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
        ]
        .into_iter()
        .flatten()
        .map(|mut stream| {
            let output = Arc::clone(&output);
            thread::spawn(move || {
                let mut buf = [0u8; 8192];
                while let Ok(n) = stream.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    output
                        .lock()
                        .expect("output lock")
                        .extend_from_slice(&buf[..n]);
                }
            })
        })
        .collect();
        let status = loop {
            if let Some(status) = child.try_wait().map_err(io("wait for check"))? {
                break Some(status);
            }
            if started.elapsed() >= self.timeout {
                kill_group(child.id());
                let _ = child.wait();
                break None;
            }
            thread::sleep(Duration::from_millis(20));
        };
        for reader in readers {
            let _ = reader.join();
        }
        let output = String::from_utf8_lossy(&output.lock().expect("output lock")).into_owned();
        let passed = status.is_some_and(|s| s.success());
        let errors = if passed { None } else { diagnostics(&output) };
        let mut tail = if passed { String::new() } else { tail(&output) };
        if status.is_none() {
            tail.push_str(&format!(
                "\n(check timed out after {}s; carve.checkTimeout)",
                self.timeout.as_secs()
            ));
        }
        Ok(Outcome {
            passed,
            tail,
            millis: started.elapsed().as_millis() as u64,
            cached: false,
            errors,
        })
    }

    /// A remembered probe verdict for this toolchain and `config` (D1: probe once per
    /// config change, not once per carve).
    pub fn probe_known(&self, config: &str) -> Option<bool> {
        self.cache
            .get(&format!("probe|{config}|{}", self.fingerprint))
            .map(|o| !o.passed)
    }

    pub fn remember_probe(&mut self, config: &str, caught: bool) -> Result<()> {
        let key = format!("probe|{config}|{}", self.fingerprint);
        self.store(
            key,
            Outcome {
                passed: !caught,
                tail: String::new(),
                millis: 0,
                cached: false,
                errors: None,
            },
        )
    }

    fn store(&mut self, key: String, outcome: Outcome) -> Result<()> {
        if self.cache.insert(key.clone(), outcome).is_none() {
            self.order.push(key);
        }
        while self.order.len() > CACHE_LIMIT {
            let oldest = self.order.remove(0);
            self.cache.remove(&oldest);
        }
        let file = CacheFile {
            entries: self
                .order
                .iter()
                .map(|k| (k.clone(), self.cache[k].clone()))
                .collect(),
        };
        let path = self.repo.carve_dir.join("checks.json");
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_vec(&file)
            .map_err(|e| Error::Internal(format!("serialize check cache: {e}")))?;
        fs::write(&tmp, json).map_err(io("write check cache"))?;
        fs::rename(&tmp, &path).map_err(io("write check cache"))
    }
}

fn load_cache(path: &Path) -> (BTreeMap<String, Outcome>, Vec<String>) {
    let file: CacheFile = fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let order = file.entries.iter().map(|(k, _)| k.clone()).collect();
    (file.entries.into_iter().collect(), order)
}

/// Symlinks dependency directories (and other untracked inputs) into the scratch worktree
/// (§8.2). Returns the names linked.
fn link(repo: &Repo, scratch: &Path, carved: &[&[u8]], extra: &[String]) -> Result<Vec<String>> {
    let configured: Vec<String> = repo
        .git
        .try_run_str(["config", "--get-all", "carve.link"])?
        .map(|v| v.lines().map(str::to_owned).collect())
        .unwrap_or_default();
    let mut names: Vec<String> = if configured.is_empty() {
        DEFAULT_LINKS.iter().map(|s| (*s).to_owned()).collect()
    } else {
        configured
    };
    for name in extra {
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    let mut linked = Vec::new();
    for name in names {
        let source = repo.toplevel.join(&name);
        let inside =
            |p: &[u8]| p == name.as_bytes() || p.starts_with(format!("{name}/").as_bytes());
        let in_trees = carved.iter().any(|p| inside(p))
            || !repo
                .git
                .run(["ls-tree", "--name-only", "HEAD", "--", &name])?
                .is_empty();
        if !source.exists() || in_trees {
            let target = scratch.join(&name);
            if fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink()) {
                let _ = fs::remove_file(&target);
            }
            continue;
        }
        let target = scratch.join(&name);
        if fs::symlink_metadata(&target).is_ok() {
            if fs::read_link(&target).ok().as_deref() == Some(source.as_path()) {
                linked.push(name);
                continue;
            }
            let _ = fs::remove_file(&target);
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(io("create link parent"))?;
        }
        std::os::unix::fs::symlink(&source, &target)
            .map_err(io(format!("link {name} into the scratch worktree")))?;
        linked.push(name);
    }
    Ok(linked)
}

/// Target, directory mtime, and install-marker mtimes of a linked path (D13).
fn fingerprint(repo: &Repo, name: &str) -> String {
    let path = repo.toplevel.join(name);
    let mtime = |p: &Path| {
        fs::metadata(p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos())
    };
    let real = fs::canonicalize(&path).unwrap_or(path.clone());
    let markers: Vec<String> = INSTALL_MARKERS
        .iter()
        .map(|m| mtime(&real.join(m)).to_string())
        .collect();
    format!(
        "{name}={}@{}[{}]",
        real.display(),
        mtime(&real),
        markers.join(",")
    )
}

fn tail(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n")
}

fn kill_group(pid: u32) {
    for signal in ["-TERM", "-KILL"] {
        let _ = Command::new("kill")
            .args([signal, "--", &format!("-{pid}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        thread::sleep(Duration::from_millis(200));
    }
}

/// `--clean`: the scratch worktree, the check cache, build info, and the build target dir.
pub fn clean(repo: &Repo) -> Result<Vec<String>> {
    let mut removed = Vec::new();
    let scratch = repo.carve_dir.join("scratch");
    if scratch.exists() {
        let path = scratch.to_string_lossy();
        let _ = repo
            .git
            .run(["worktree", "remove", "--force", path.as_ref()]);
        let _ = fs::remove_dir_all(&scratch);
        removed.push("scratch worktree".to_owned());
    }
    repo.git.run(["worktree", "prune"])?;
    for (name, label) in [
        ("checks.json", "check cache"),
        ("tsbuildinfo", "build info"),
        ("target", "build target dir"),
    ] {
        let path = repo.carve_dir.join(name);
        let gone = if path.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        if gone.is_ok() {
            removed.push(label.to_owned());
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod diagnostics_tests {
    use super::*;

    #[test]
    fn tsc_errors_are_recognized_in_both_formats() {
        let plain = "src/a.ts(3,7): error TS2304: Cannot find name 'x'.\n  Some detail.\nerror TS5058: The specified path does not exist.\nFound 2 errors.";
        assert_eq!(
            diagnostics(plain).unwrap(),
            vec![
                "src/a.ts(3,7): error TS2304: Cannot find name 'x'.",
                "error TS5058: The specified path does not exist."
            ]
        );
        let pretty = "\u{1b}[96msrc/a.ts\u{1b}[0m:\u{1b}[93m3\u{1b}[0m:\u{1b}[93m7\u{1b}[0m - \u{1b}[91merror\u{1b}[0m\u{1b}[90m TS2304: \u{1b}[0mCannot find name 'x'.";
        let found = diagnostics(pretty).unwrap();
        assert_eq!(
            found,
            vec!["src/a.ts:3:7 - error TS2304: Cannot find name 'x'."]
        );
        assert!(diagnostics("Segmentation fault").is_none());
    }

    #[test]
    fn an_error_that_moved_is_the_same_error() {
        assert_eq!(
            error_key("src/a.ts(3,7): error TS2304: Cannot find name 'x'."),
            error_key("src/a.ts(40,1): error TS2304: Cannot find name 'x'.")
        );
        assert_eq!(
            error_key("src/a.ts:3:7 - error TS2304: Cannot find name 'x'."),
            "src/a.ts|error TS2304: Cannot find name 'x'."
        );
        assert_ne!(
            error_key("src/a.ts(3,7): error TS2304: Cannot find name 'x'."),
            error_key("src/b.ts(3,7): error TS2304: Cannot find name 'x'.")
        );
    }
}
