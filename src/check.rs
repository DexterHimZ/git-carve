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
/// Parallel check workers for a check that can run in parallel, before `carve.checkJobs`
/// (DECISIONS.md D51), capped by half the CPUs.
const DEFAULT_JOBS: usize = 2;
const MAX_JOBS: u64 = 8;
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
    /// The file extensions a default check reads (D45); `None` for an explicit check, whose
    /// reach only the probe can tell.
    pub reads: Option<Vec<String>>,
    /// Of `reads`, extensions whose missed probe means "unverified", not "vacuous" (D54).
    pub soft_reads: Vec<String>,
    /// Safe to run in several worktrees at once without asking (D51): the default TypeScript
    /// check, whose build info lives next to each worktree. Anything else needs
    /// `carve.checkJobs` (cargo, for one, locks its shared target directory).
    pub parallel: bool,
}

impl CheckSpec {
    /// Whether the probe should test `path`: an adapter-known file this check claims to read.
    pub fn probes(&self, path: &[u8]) -> bool {
        crate::lang::adapter_for(path).is_some()
            && self
                .reads
                .as_ref()
                .is_none_or(|exts| exts.contains(&crate::lang::extension(path)))
    }
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
    let mut parallel = true;
    let check = match primary().default_check(&repo.toplevel, &configs, &changed) {
        Ok(check) => check,
        // No TypeScript project at all: the ecosystem's own check (DECISIONS.md D45).
        Err(_) if configs.is_empty() => {
            parallel = false;
            match crate::lang::generic::ecosystem_check(&repo.toplevel, &changed) {
                Some(check) => check,
                None if changed
                    .iter()
                    .any(|p| crate::lang::is_typescript(p.as_bytes())) =>
                {
                    return Err(precondition(
                        "no tsconfig.json at the repository root, so there is no default check \
                         (no tsconfig files are tracked)",
                        Some(
                            "pass --check, or set it once: git config carve.check \"npx tsc --noEmit -p .\"",
                        ),
                    ));
                }
                None => {
                    // D54: a JavaScript project's own script is the natural check.
                    let example = if changed
                        .iter()
                        .any(|p| crate::lang::is_javascript(p.as_bytes()))
                    {
                        "npm test"
                    } else {
                        "make test"
                    };
                    return Err(precondition(
                        "carve has no default check for this repository (it found no tsconfig.json, \
                         Cargo.toml, go.mod, or Python project file at the top level)",
                        Some(&format!(
                            "pass the command that checks your code, such as --check \"{example}\", \
                              or set it once: git config carve.check \"{example}\""
                        )),
                    ));
                }
            }
        }
        Err(reason) => return Err(precondition(reason, None)),
    };
    Ok(CheckSpec {
        command: check.command,
        display: check.display,
        explicit: false,
        links: check.links,
        reads: Some(check.reads),
        soft_reads: check.soft_reads,
        parallel,
    })
}

fn explicit(command: &str) -> CheckSpec {
    CheckSpec {
        command: command.to_owned(),
        display: command.to_owned(),
        explicit: true,
        links: Vec::new(),
        reads: None,
        soft_reads: Vec::new(),
        parallel: false,
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
    /// The run looked for `errors` (every run since D43). Failures cached before that are
    /// re-run once under a baseline; later ones without recognizable errors (cargo, go, ...)
    /// stay cached.
    #[serde(default)]
    pub diagnosed: bool,
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

/// One place a check runs: the scratch worktree (worker 0) or a parallel worker's
/// `carve/w<k>/scratch` (D51). Each keeps its own warm files, and the default TypeScript
/// check its own build info (`../tsbuildinfo`, next to the worktree).
struct Worker {
    dir: PathBuf,
    git: Git,
}

/// Runs the check on trees, in scratch worktrees kept between runs.
pub struct Checker<'r> {
    repo: &'r Repo,
    pub spec: CheckSpec,
    workers: Vec<Worker>,
    /// How many trees may be checked at once (D51).
    pub jobs: usize,
    /// The paths carve may commit, for linking dependency directories into new workers.
    carved: Vec<Vec<u8>>,
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
        open_worktree(repo, &scratch)?;
        fs::create_dir_all(carve.join("tsbuildinfo")).map_err(io("create tsbuildinfo dir"))?;
        let links = link(repo, &scratch, carved, &spec.links)?;
        let jobs = jobs_for(repo, &spec)?;
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
        let worker = Worker {
            git: repo.git.with_dir(&scratch),
            dir: scratch,
        };
        Ok(Self {
            repo,
            spec,
            workers: vec![worker],
            jobs,
            carved: carved.iter().map(|p| p.to_vec()).collect(),
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
        &self.workers[0].dir
    }

    /// Checks one tree, from the cache when this exact tree, command, and toolchain passed
    /// or failed before. With a baseline (D43), a failure that adds no error beyond HEAD's
    /// passes, and its tail lists only the new errors.
    pub fn check(&mut self, tree: &str) -> Result<Outcome> {
        let mut out = self.check_many(&[tree.to_owned()], false)?;
        Ok(out.remove(0).expect("one tree, no early stop"))
    }

    /// Checks trees in order, up to `jobs` at a time in separate worktrees (D51). Cached
    /// trees cost nothing. With `stop_at_failure`, the first failing tree in order ends the
    /// run, and every tree after it comes back `None` (unchecked, or checked but not
    /// reported: a round runs in parallel). Outcomes are exactly what `check` would give one
    /// by one: each tree's result depends only on the tree.
    pub fn check_many(
        &mut self,
        trees: &[String],
        stop_at_failure: bool,
    ) -> Result<Vec<Option<Outcome>>> {
        let mut out: Vec<Option<Outcome>> = vec![None; trees.len()];
        let mut next = 0;
        while next < trees.len() {
            // The next round: up to `jobs` uncached trees; cached ones are answered here.
            let mut round: Vec<usize> = Vec::new();
            while next < trees.len() && round.len() < self.jobs {
                let i = next;
                if round.iter().any(|&r| trees[r] == trees[i]) {
                    break; // a duplicate waits for the round's result in the cache
                }
                next += 1;
                if let Some(hit) = self.hit(&trees[i]) {
                    let outcome = self.against_baseline(hit);
                    let failed = !outcome.passed;
                    out[i] = Some(outcome);
                    if failed && stop_at_failure {
                        break;
                    }
                } else {
                    round.push(i);
                }
            }
            while self.workers.len() < round.len() {
                self.add_worker()?;
            }
            let (command, timeout) = (self.spec.command.as_str(), self.timeout);
            let target = self.repo.carve_dir.join("target");
            let results: Vec<Result<Outcome>> = thread::scope(|scope| {
                let handles: Vec<_> = round
                    .iter()
                    .zip(&self.workers)
                    .map(|(&i, worker)| {
                        let (tree, target) = (&trees[i], &target);
                        scope.spawn(move || -> Result<Outcome> {
                            // Updates only the files that differ, so incremental checkers
                            // stay warm.
                            worker.git.run(["read-tree", "-u", "--reset", tree])?;
                            run_check(command, &worker.dir, timeout, target)
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().expect("check worker panicked"))
                    .collect()
            });
            for (&i, result) in round.iter().zip(results) {
                let outcome = result?;
                self.runs += 1;
                self.millis += outcome.millis;
                self.store(
                    format!("{}|{}", trees[i], self.fingerprint),
                    outcome.clone(),
                )?;
                out[i] = Some(self.against_baseline(outcome));
            }
            if stop_at_failure {
                if let Some(f) = out
                    .iter()
                    .position(|o| o.as_ref().is_some_and(|o| !o.passed))
                {
                    for o in out.iter_mut().skip(f + 1) {
                        *o = None;
                    }
                    return Ok(out);
                }
            }
        }
        Ok(out)
    }

    /// Runs `tree` once in each of `jobs` workers without caching the outcome, so every
    /// worker's incremental state starts there: the speed benchmark's steady state, where an
    /// earlier carve or verify left build info at about HEAD (D51).
    pub fn warm(&mut self, tree: &str) -> Result<()> {
        while self.workers.len() < self.jobs {
            self.add_worker()?;
        }
        let (command, timeout) = (self.spec.command.as_str(), self.timeout);
        let target = self.repo.carve_dir.join("target");
        thread::scope(|scope| {
            let handles: Vec<_> = self
                .workers
                .iter()
                .take(self.jobs)
                .map(|worker| {
                    let target = &target;
                    scope.spawn(move || -> Result<Outcome> {
                        worker.git.run(["read-tree", "-u", "--reset", tree])?;
                        run_check(command, &worker.dir, timeout, target)
                    })
                })
                .collect();
            handles
                .into_iter()
                .try_for_each(|h| h.join().expect("check worker panicked").map(drop))
        })
    }

    /// A cached outcome for this tree, command, and toolchain.
    fn hit(&mut self, tree: &str) -> Option<Outcome> {
        let hit = self.cache.get(&format!("{tree}|{}", self.fingerprint))?;
        // A failure cached before errors were recorded cannot set or meet a baseline.
        if (self.no_new_errors || self.baseline.is_some()) && !hit.passed && !hit.diagnosed {
            return None;
        }
        self.hits += 1;
        Some(Outcome {
            cached: true,
            ..hit.clone()
        })
    }

    /// Worker k: `carve/w<k>/scratch`, with its own build-info directory and the same
    /// dependency links as the scratch worktree.
    fn add_worker(&mut self) -> Result<()> {
        let root = self.repo.carve_dir.join(format!("w{}", self.workers.len()));
        let dir = root.join("scratch");
        open_worktree(self.repo, &dir)?;
        fs::create_dir_all(root.join("tsbuildinfo")).map_err(io("create tsbuildinfo dir"))?;
        let carved: Vec<&[u8]> = self.carved.iter().map(Vec::as_slice).collect();
        link(self.repo, &dir, &carved, &self.spec.links)?;
        self.workers.push(Worker {
            git: self.repo.git.with_dir(&dir),
            dir,
        });
        Ok(())
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

    /// D43: a failure that adds no error beyond the baseline passes. Identity without one.
    pub fn against_baseline(&self, raw: Outcome) -> Outcome {
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
                diagnosed: false,
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

/// Runs the check once in `dir` (the process group is killed on timeout).
fn run_check(command: &str, dir: &Path, timeout: Duration, target: &Path) -> Result<Outcome> {
    let started = Instant::now();
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("CARGO_TARGET_DIR", target)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .process_group(0);
    let mut child = cmd.spawn().map_err(io(format!("run check `{command}`")))?;
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
        if started.elapsed() >= timeout {
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
            timeout.as_secs()
        ));
    }
    Ok(Outcome {
        passed,
        tail,
        millis: started.elapsed().as_millis() as u64,
        cached: false,
        errors,
        diagnosed: true,
    })
}

fn load_cache(path: &Path) -> (BTreeMap<String, Outcome>, Vec<String>) {
    let file: CacheFile = fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let order = file.entries.iter().map(|(k, _)| k.clone()).collect();
    (file.entries.into_iter().collect(), order)
}

/// The scratch worktree at `dir`, created detached and empty when missing (§8.2).
fn open_worktree(repo: &Repo, dir: &Path) -> Result<()> {
    if dir.join(".git").exists() {
        return Ok(());
    }
    let _ = fs::remove_dir_all(dir);
    repo.git.run(["worktree", "prune"])?;
    let path = dir.to_string_lossy();
    repo.git.run([
        "worktree",
        "add",
        "--detach",
        "--no-checkout",
        "-q",
        path.as_ref(),
        "HEAD",
    ])?;
    Ok(())
}

/// D51: `carve.checkJobs` when set; else `DEFAULT_JOBS` (at most half the CPUs) for a check
/// that is safe in parallel, and 1 for anything else. `GIT_CARVE_DEV_CHECK_JOBS` overrides
/// both, for the benchmark.
fn jobs_for(repo: &Repo, spec: &CheckSpec) -> Result<usize> {
    if let Some(n) = std::env::var("GIT_CARVE_DEV_CHECK_JOBS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        return Ok(n.clamp(1, MAX_JOBS) as usize);
    }
    let cpus = thread::available_parallelism().map_or(1, |n| n.get());
    let default = if spec.parallel {
        DEFAULT_JOBS.min(cpus / 2).max(1)
    } else {
        1
    };
    Ok(repo
        .config_u64("carve.checkJobs", default as u64)?
        .clamp(1, MAX_JOBS) as usize)
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
    // D51: the parallel workers' worktrees, `carve/w<k>/`.
    let mut workers = 0;
    for entry in fs::read_dir(&repo.carve_dir)
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_worker = name
            .strip_prefix('w')
            .is_some_and(|k| !k.is_empty() && k.bytes().all(|b| b.is_ascii_digit()));
        if is_worker && entry.path().is_dir() {
            let dir = entry.path().join("scratch");
            let path = dir.to_string_lossy();
            let _ = repo
                .git
                .run(["worktree", "remove", "--force", path.as_ref()]);
            let _ = fs::remove_dir_all(entry.path());
            workers += 1;
        }
    }
    if workers > 0 {
        removed.push(format!("{workers} parallel check worktree(s)"));
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
