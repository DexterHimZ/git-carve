//! `git carve verify` (DECISIONS.md D44): checks that every commit in a range passes the check,
//! oldest first, in the scratch worktree. The working tree, the index, and refs are never
//! touched, so it is safe as a pre-push hook or a CI step. A probe (D1) first proves the check
//! actually reads the changed files; `--no-new-errors` (D43) measures each commit against the
//! errors the range's base already had.

use std::collections::BTreeSet;

use serde::Serialize;

use crate::check::{self, Checker};
use crate::error::{Error, Result, precondition};
use crate::lang::adapter_for;
use crate::model::{Entry, Mode, show};
use crate::reconstruct::Target;
use crate::repo::Repo;
use crate::tree::TreeBuilder;

/// Probes per run: one per top-level directory holding a changed file the adapter knows.
const MAX_PROBES: usize = 3;
/// Error lines quoted per failing commit.
const QUOTED: usize = 3;

pub struct VerifyOptions {
    /// `git rev-list` arguments, one list per range (e.g. `["main..HEAD"]` or
    /// `["<sha>", "--not", "--remotes"]`). Commits from all ranges are verified once each.
    pub ranges: Vec<Vec<String>>,
    pub check: Option<String>,
    pub no_new_errors: bool,
    /// The tolerance came from the default (D53), not from a flag or `carve.noNewErrors`:
    /// when the base fails without errors carve can tell apart, stay strict instead of
    /// refusing.
    pub by_default: bool,
    /// Stop at the first failing commit, for a gate that needs a verdict, not a report
    /// (`--fail-fast`, implied by `--pre-push`). The D1 probe then runs only when every commit
    /// passed: it guards against a false pass, and a failure needs no such proof.
    pub fail_fast: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct CommitResult {
    pub oid: String,
    pub subject: String,
    pub passed: bool,
    pub cached: bool,
    pub millis: u64,
    /// The check's error lines (only the new ones with `--no-new-errors`).
    pub errors: Vec<String>,
    /// A failing commit whose every error already appeared in an earlier failing commit of
    /// the range: that commit introduced them, this one only inherits them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inherits_from: Option<String>,
}

/// The `verify --json` contract version: fields are only ever added within a version.
pub const JSON_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize)]
pub struct BranchReport {
    pub version: u32,
    pub check: String,
    pub commits: Vec<CommitResult>,
    /// `--no-new-errors`: the error count of the range's base.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_errors: Option<usize>,
    /// Changed files the check did not read (an explicit check whose probe passed).
    pub unverified: Vec<String>,
    /// The base fails the check with recognizable errors, so `--no-new-errors` would help.
    pub base_fails: Option<usize>,
    pub runs: usize,
    pub cached: usize,
    pub millis: u64,
    /// `fail_fast`: commits after the first failure, left unchecked.
    pub skipped: usize,
}

impl BranchReport {
    pub fn failed(&self) -> usize {
        self.commits.iter().filter(|c| !c.passed).count()
    }
}

/// `carve.noNewErrors` (D50), or `None` when unset.
pub fn no_new_errors_config(repo: &Repo) -> Result<Option<bool>> {
    Ok(repo
        .git
        .try_run_str(["config", "--type=bool", "carve.noNewErrors"])?
        .map(|v| v.trim() == "true"))
}

/// D53: whether errors the base already has are tolerated, and whether that is only the
/// default. `--strict` or `carve.noNewErrors false` turn it off; `--no-new-errors` or
/// `carve.noNewErrors true` ask for it explicitly; otherwise it is on by default.
pub fn tolerance(repo: &Repo, strict: bool, explicit: bool) -> Result<(bool, bool)> {
    let config = no_new_errors_config(repo)?;
    if strict || (config == Some(false) && !explicit) {
        return Ok((false, false));
    }
    let asked = explicit || config == Some(true);
    Ok((true, !asked))
}

/// The range a plain `git carve verify` checks: commits not on the upstream, or, without one,
/// commits on no remote. With no remote at all there is nothing safe to default to.
pub fn default_ranges(repo: &Repo) -> Result<Vec<Vec<String>>> {
    if repo
        .git
        // `--verify -q` exits 1, quietly, when there is no upstream (plain rev-parse exits 128).
        .try_run_str(["rev-parse", "--verify", "-q", "@{upstream}"])?
        .is_some()
    {
        return Ok(vec![vec!["@{upstream}..HEAD".to_owned()]]);
    }
    if !repo.git.run_str(["remote"])?.trim().is_empty() {
        return Ok(vec![vec![
            "HEAD".to_owned(),
            "--not".to_owned(),
            "--remotes".to_owned(),
        ]]);
    }
    Err(precondition(
        "this branch has no upstream and the repository has no remote, so there is no default range",
        Some("name one, for example: git carve verify main..HEAD"),
    ))
}

/// Ranges from a pre-push hook's standard input: `<local ref> <local oid> <remote ref> <remote oid>`
/// per line. A deleted ref is skipped; a new branch, or a remote commit this repository does
/// not have (a teammate's push, not fetched), verifies what no remote has yet.
pub fn pre_push_ranges(stdin: &str, zero: &str, known: impl Fn(&str) -> bool) -> Vec<Vec<String>> {
    stdin
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let [_, local, _, remote] = fields[..] else {
                return None;
            };
            if local == zero {
                None
            } else if remote == zero || !known(remote) {
                Some(vec![
                    local.to_owned(),
                    "--not".to_owned(),
                    "--remotes".to_owned(),
                ])
            } else {
                Some(vec![format!("{remote}..{local}")])
            }
        })
        .collect()
}

/// Commits of the ranges, oldest first, each once: `(oid, subject)`.
fn commits(repo: &Repo, ranges: &[Vec<String>]) -> Result<Vec<(String, String)>> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for range in ranges {
        let mut args = vec![
            "log".to_owned(),
            "--reverse".to_owned(),
            "--format=%H%x00%s".to_owned(),
        ];
        args.extend(range.iter().cloned());
        args.push("--".to_owned());
        let listing = repo.git.run_str(&args)?;
        for line in listing.lines().filter(|l| !l.is_empty()) {
            let (oid, subject) = line.split_once('\0').unwrap_or((line, ""));
            if seen.insert(oid.to_owned()) {
                out.push((oid.to_owned(), subject.to_owned()));
            }
        }
    }
    Ok(out)
}

/// Every path any commit of the range changes, against its first parent.
fn changed_paths(repo: &Repo, commits: &[(String, String)]) -> Result<Vec<Vec<u8>>> {
    let mut paths = BTreeSet::new();
    for (oid, _) in commits {
        let listing = repo.git.run([
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            "-z",
            "--root",
            "-m",
            "--first-parent",
            oid.as_str(),
        ])?;
        paths.extend(
            listing
                .split(|&b| b == 0)
                .filter(|p| !p.is_empty())
                .map(<[u8]>::to_vec),
        );
    }
    Ok(paths.into_iter().collect())
}

fn quote(tail: &str) -> Vec<String> {
    let located: Vec<String> = tail
        .lines()
        .filter(|l| l.contains("error"))
        .take(QUOTED)
        .map(|l| l.trim().to_owned())
        .collect();
    if located.is_empty() {
        tail.lines()
            .filter(|l| !l.trim().is_empty())
            .take(QUOTED)
            .map(|l| l.trim().to_owned())
            .collect()
    } else {
        located
    }
}

pub fn verify_branch(repo: &Repo, opts: &VerifyOptions) -> Result<BranchReport> {
    let commits = commits(repo, &opts.ranges)?;
    let changed = changed_paths(repo, &commits)?;
    let carved: Vec<&[u8]> = changed.iter().map(Vec::as_slice).collect();
    let spec = check::resolve(repo, opts.check.as_deref(), &carved)?;
    let mut checker = Checker::new(repo, spec, &carved)?;
    checker.no_new_errors = opts.no_new_errors;
    let mut report = BranchReport {
        version: JSON_VERSION,
        check: checker.spec.display.clone(),
        commits: Vec::new(),
        baseline_errors: None,
        unverified: Vec::new(),
        base_fails: None,
        runs: 0,
        cached: 0,
        millis: 0,
        skipped: 0,
    };
    let Some((first, _)) = commits.first() else {
        return Ok(report);
    };
    let tree = |rev: &str| repo.git.run_str(["rev-parse", &format!("{rev}^{{tree}}")]);
    let base_tree =
        repo.git
            .try_run_str(["rev-parse", "--verify", "-q", &format!("{first}^^{{tree}}")])?;

    // D43: the base's errors become the baseline.
    if opts.no_new_errors {
        if let Some(base) = &base_tree {
            let outcome = checker.check(base)?;
            if !outcome.passed {
                match checker.set_baseline(&outcome) {
                    Some(n) => report.baseline_errors = Some(n),
                    // D53: the default tolerates only errors carve can tell apart; with a
                    // check that prints none (cargo, go, `make test`), verify stays strict.
                    None if opts.by_default => checker.no_new_errors = false,
                    None => {
                        return Err(Error::CheckFailed(format!(
                            "the range's base fails the check without errors carve recognizes, \
                             so there is no baseline to compare with\ncheck: {}\n{}",
                            checker.spec.display, outcome.tail
                        )));
                    }
                }
            }
        }
    }

    // D1: the check must fail on a deliberately broken changed file.
    let last_tree = tree(&commits.last().expect("non-empty").0)?;
    if !opts.fail_fast {
        probe(repo, &mut checker, &last_tree, &changed, &mut report)?;
    }

    // Error identities (D43's position-free keys) already seen, and which commit introduced each.
    let mut seen: Vec<(String, String)> = Vec::new();
    // D51: up to `jobs` commits at once; with `fail_fast`, nothing after the first failure.
    let trees = commits
        .iter()
        .map(|(oid, _)| tree(oid))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let outcomes = checker.check_many(&trees, opts.fail_fast)?;
    for (i, ((oid, subject), outcome)) in commits.iter().zip(outcomes).enumerate() {
        let Some(outcome) = outcome else {
            report.skipped = commits.len() - i;
            break;
        };
        let keys: Vec<String> = outcome
            .errors
            .iter()
            .flatten()
            .map(|e| check::error_key(e))
            .collect();
        let inherits_from = if outcome.passed || keys.is_empty() {
            None
        } else {
            let origin = |k: &String| {
                seen.iter()
                    .find(|(key, _)| key == k)
                    .map(|(_, o)| o.clone())
            };
            keys.iter()
                .map(origin)
                .collect::<Option<Vec<String>>>()
                .and_then(|origins| origins.first().cloned())
        };
        for k in keys {
            if !seen.iter().any(|(key, _)| *key == k) {
                seen.push((k, oid.clone()));
            }
        }
        report.commits.push(CommitResult {
            oid: oid.clone(),
            subject: subject.clone(),
            passed: outcome.passed,
            cached: outcome.cached,
            millis: outcome.millis,
            errors: if outcome.passed {
                Vec::new()
            } else {
                quote(&outcome.tail)
            },
            inherits_from,
        });
    }
    if opts.fail_fast && report.failed() == 0 {
        probe(repo, &mut checker, &last_tree, &changed, &mut report)?;
    }

    // A failure that the base shares is worth a hint (D43).
    if report.failed() > 0 && !checker.no_new_errors {
        if let Some(base) = &base_tree {
            let outcome = checker.check(base)?;
            if !outcome.passed {
                report.base_fails = outcome.errors.as_ref().map(Vec::len);
            }
        }
    }
    report.runs = checker.runs;
    report.cached = checker.hits;
    report.millis = checker.millis;
    Ok(report)
}

/// D1 for a range: corrupt one changed, adapter-known file per top-level directory in the
/// last commit's tree; the check must fail on each. The corrupted trees are cached by oid
/// like any other, so a re-run costs nothing.
fn probe(
    repo: &Repo,
    checker: &mut Checker,
    last_tree: &str,
    changed: &[Vec<u8>],
    report: &mut BranchReport,
) -> Result<()> {
    let mut by_dir: std::collections::BTreeMap<Vec<u8>, (Vec<u8>, Entry)> = Default::default();
    let mut outside = Vec::new();
    for path in changed {
        if !checker.spec.probes(path) {
            // D45: a source file the check does not claim to read is unverified, not probed.
            if adapter_for(path).is_some() {
                outside.push(show(path));
            }
            continue;
        }
        let listing = repo
            .git
            .run(["ls-tree", "-z", last_tree, "--", &show(path)])?;
        let Some(line) = listing.split(|&b| b == 0).find(|l| !l.is_empty()) else {
            continue; // deleted by the end of the range
        };
        let text = String::from_utf8_lossy(line);
        let mut parts = text.split_whitespace();
        let (Some(mode), Some(kind), Some(oid)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let Ok(mode) = u32::from_str_radix(mode, 8) else {
            continue;
        };
        let mode = Mode(mode);
        if kind != "blob" || !mode.is_regular() {
            continue;
        }
        let dir = path.split(|&b| b == b'/').next().unwrap_or(path).to_vec();
        by_dir.entry(dir).or_insert_with(|| {
            (
                path.clone(),
                Entry {
                    mode,
                    oid: oid.to_owned(),
                },
            )
        });
    }
    let mut caught_any = false;
    let mut missed = Vec::new();
    for (path, entry) in by_dir.into_values().take(MAX_PROBES) {
        let content = repo.read_blobs(&[entry.oid.as_str()])?.remove(0);
        let adapter = adapter_for(&path).expect("filtered above");
        let mut builder = TreeBuilder::new(repo, "verify-probe", last_tree)?;
        builder.set(vec![(
            path.clone(),
            Target::Blob {
                mode: entry.mode,
                bytes: adapter.corrupt(&content),
            },
        )])?;
        let broken = builder.write()?;
        if checker.check(&broken)?.passed {
            missed.push(show(&path));
        } else {
            caught_any = true;
        }
    }
    if let Some(first) = missed.first() {
        if !caught_any && !checker.spec.explicit {
            return Err(Error::CheckFailed(format!(
                "the default check passed on a deliberately broken {first}, so it does not read \
                 that file and cannot prove anything about it\ncheck: {}\n\
                 hint: pass --check, or set git config carve.check, with a command that checks it",
                checker.spec.display
            )));
        }
    }
    report.unverified = missed.into_iter().chain(outside).collect();
    Ok(())
}
