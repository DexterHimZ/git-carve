//! Verification (SPEC §8.3–§8.4): the check runs on the final tree, on HEAD, on a probe
//! that must fail (DECISIONS.md D1), then on each prefix. A failing prefix drops its
//! boundary; that never creates a new tree to check (D2). Prefixes that would break a
//! file's syntax are dropped without a run (D14). A working tree that does not pass is
//! rescued when leaving some groups out lets the rest pass (D40).

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::atom::{AtomId, AtomKind, Changes};
use crate::check::Checker;
use crate::error::{Error, Result};
use crate::facts::Analysis;
use crate::graph::Plan;
use crate::lang::adapter_for;
use crate::model::show;
use crate::reconstruct::Target;
use crate::repo::Repo;
use crate::snapshot::Snapshot;
use crate::tree::{TreeBuilder, prefix_trees};

/// Probes per run: one per (nearest config directory, first folder below it), D1 and D27.
const MAX_PROBES: usize = 4;
/// D40: rounds of compiler-guided leaving, and the most groups the one-at-a-time search tries.
const RESCUE_ROUNDS: usize = 3;
/// Error-guided repair (EG): rounds per failing boundary, and the most groups one pull moves.
const EG_ROUNDS: usize = 3;
const EG_MAX_PULL: usize = 4;
const RESCUE_SEARCH_MAX: usize = 12;
/// D40: check-output lines quoted in the buffer to say why atoms were left.
const RESCUE_QUOTE: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Dropped {
    /// Commits `after` and `after + 1` (1-based, in the unverified plan) failed separately.
    CheckFailed { after: usize, tail: String },
    /// Commit `after` would leave `path` unparseable (D14).
    Syntax { after: usize, path: String },
    /// Error-guided repair (dev experiment, DECISIONS.md D57): planned commits `groups` (1-based)
    /// moved into commit `into`, because its check errors pointed at them.
    Pulled { into: usize, groups: Vec<usize> },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Probe {
    pub path: String,
    /// The check failed on the broken file, as it must.
    pub caught: bool,
    pub remembered: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Verification {
    pub check: String,
    pub dropped: Vec<Dropped>,
    /// Leftovers the working tree needed, put back into their groups (§6.5).
    pub unleft: Vec<AtomId>,
    pub probes: Vec<Probe>,
    /// Files the check did not read (an explicit check whose probe passed).
    pub unverified: Vec<String>,
    /// D40: the working tree did not pass; these atoms were left out so the rest could.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failing: Option<Failing>,
    /// D43 (`--no-new-errors`): HEAD's error count. Every commit adds no error beyond these.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_errors: Option<usize>,
    pub runs: usize,
    pub cached: usize,
    /// Summed run time of every check run (more than the wall time when runs overlap).
    pub millis: u64,
    /// Wall time of the whole verification, and the parallel workers it could use (D51).
    pub wall_ms: u64,
    pub jobs: usize,
}

/// D40: what a failing working tree cost, for the buffer and the summary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Failing {
    /// Atoms left in the working tree because the tree does not pass with them.
    pub left: Vec<AtomId>,
    /// The check's own error lines that implicated them (up to a few).
    pub errors: Vec<String>,
}

pub fn verify(
    repo: &Repo,
    snap: &Snapshot,
    changes: &Changes,
    analysis: &Analysis,
    plan: &Plan,
    checker: &mut Checker,
) -> Result<(Plan, Verification)> {
    let started = std::time::Instant::now();
    let mut plan = plan.clone();
    let mut report = Verification {
        check: checker.spec.display.clone(),
        jobs: checker.jobs,
        ..Verification::default()
    };
    if plan.groups.is_empty() {
        return Ok((plan, report));
    }
    let mut trees = prefix_trees(repo, &snap.base.tree, changes, &plan.groups)?;

    // 0. Base and final together (D51); the base's errors, if any, may become the baseline
    // (D43), applied to the final outcome afterwards.
    // Spare workers in that first round check prefixes ahead of need (step 4 then finds them
    // cached); no extra round runs before the final's verdict is known.
    let mut first = vec![
        snap.base.tree.clone(),
        trees.last().expect("groups are not empty").clone(),
    ];
    first.extend(
        (0..plan.groups.len() - 1)
            .filter(|&k| syntax_break(changes, analysis, &plan.groups[..=k]).is_none())
            .take(checker.jobs.saturating_sub(2))
            .map(|k| trees[k].clone()),
    );
    let mut first = checker.check_many(&first, false)?.into_iter();
    let base = first.next().flatten().expect("checked");
    let last_raw = first.next().flatten().expect("checked");
    if !base.passed {
        if let Some(n) = checker.set_baseline(&base) {
            report.baseline_errors = Some(n);
        } else {
            let what = if snap.base.commit == snap.head.commit {
                "HEAD"
            } else {
                "the base of the range"
            };
            return Err(head_fails(changes, checker, &base, what));
        }
    }

    // 1. Final: base plus every non-leave atom.
    let last = checker.against_baseline(last_raw);
    if !last.passed {
        let display = checker.spec.display.clone();
        let fail = |tail: &str, why: &str| {
            Error::CheckFailed(format!(
                "your working tree does not pass the check, so there is nothing to prove{why}\n\
                 check: {display}\n{tail}"
            ))
        };
        let mut rescued = false;
        let mut failure = last.tail.clone();
        if !plan.leave.is_empty() {
            let mut all = plan.groups.clone();
            all.push(plan.leave.clone());
            let whole = prefix_trees(repo, &snap.base.tree, changes, &all)?;
            let outcome = checker.check(whole.last().expect("non-empty"))?;
            if outcome.passed {
                // The leftovers were load-bearing (§6.5): put them back.
                for id in std::mem::take(&mut plan.leave) {
                    let g =
                        nearest_group(changes, &plan.groups, id).unwrap_or(plan.groups.len() - 1);
                    plan.groups[g].push(id);
                    plan.groups[g].sort_unstable();
                    report.unleft.push(id);
                }
                rescued = true;
            } else {
                failure = outcome.tail;
            }
        }
        if !rescued {
            // D40: commit what passes, leave what does not (HEAD passed, step 0).
            let Some(left) = rescue(repo, snap, changes, &plan, &last.tail, checker)? else {
                return Err(fail(
                    &failure,
                    "; leaving out any one group of changes does not help either",
                ));
            };
            let mut atoms: Vec<AtomId> =
                left.iter().flat_map(|&g| plan.groups[g].clone()).collect();
            atoms.sort_unstable();
            for &id in &atoms {
                plan.tags
                    .entry(id)
                    .or_default()
                    .push(crate::graph::Tag::FailsCheck);
            }
            report.failing = Some(Failing {
                left: atoms.clone(),
                errors: error_lines(&last.tail),
            });
            let kept: Vec<Vec<AtomId>> = plan
                .groups
                .iter()
                .enumerate()
                .filter(|(g, _)| !left.contains(g))
                .map(|(_, atoms)| atoms.clone())
                .collect();
            plan.style = plan.style.and_then(|s| {
                (!left.contains(&s)).then(|| s - left.iter().filter(|&&g| g < s).count())
            });
            plan.groups = kept;
            plan.leave.extend(atoms);
            plan.leave.sort_unstable();
            if plan.groups.is_empty() {
                return Err(fail(&failure, "; no group of changes passes on its own"));
            }
        }
        trees = prefix_trees(repo, &snap.base.tree, changes, &plan.groups)?;
    }

    // 3. Probe: the check must fail on a deliberately broken changed file (D1). Its trees
    // run in the same rounds as the prefixes (D51), which step 4 then finds cached.
    let final_tree = trees.last().expect("non-empty").clone();
    let n = plan.groups.len();
    let breaks: Vec<Option<String>> = (0..n.saturating_sub(1))
        .map(|k| syntax_break(changes, analysis, &plan.groups[..=k]))
        .collect();
    let ahead: Vec<String> = (0..breaks.len())
        .filter(|&k| breaks[k].is_none())
        .map(|k| trees[k].clone())
        .collect();
    probe(
        repo,
        changes,
        &plan,
        &final_tree,
        checker,
        &mut report,
        &ahead,
    )?;

    // 4. Prefixes, in order; drop every failing boundary (D2).
    let mut keep = vec![true; n.saturating_sub(1)];
    // D51: every prefix that parses, up to `jobs` at once.
    let wanted: Vec<usize> = (0..breaks.len()).filter(|&k| breaks[k].is_none()).collect();
    let checked = checker.check_many(
        &wanted.iter().map(|&k| trees[k].clone()).collect::<Vec<_>>(),
        false,
    )?;
    let mut outcomes: Vec<Option<crate::check::Outcome>> = vec![None; breaks.len()];
    for (k, outcome) in wanted.into_iter().zip(checked) {
        outcomes[k] = outcome;
    }
    for (k, (broken, outcome)) in breaks.into_iter().zip(outcomes).enumerate() {
        if let Some(path) = broken {
            keep[k] = false;
            report.dropped.push(Dropped::Syntax { after: k + 1, path });
            continue;
        }
        let outcome = outcome.expect("checked");
        if !outcome.passed {
            keep[k] = false;
            report.dropped.push(Dropped::CheckFailed {
                after: k + 1,
                tail: outcome.tail,
            });
        }
    }

    // Dev experiment (DECISIONS.md D57): repair failing boundaries from the check's own errors before
    // merging forward. Off by default; the default plan is today's, byte for byte.
    if keep.iter().any(|k| !k) && std::env::var_os("GIT_CARVE_DEV_ERROR_GUIDED").is_some() {
        let (groups, style, dropped) = error_guided(repo, snap, changes, analysis, &plan, checker)?;
        plan.groups = groups;
        plan.style = style;
        report.dropped = dropped;
        report.runs = checker.runs;
        report.cached = checker.hits;
        report.millis = checker.millis;
        report.wall_ms = started.elapsed().as_millis() as u64;
        return Ok((plan, report));
    }

    // Merge across dropped boundaries.
    let mut groups: Vec<Vec<AtomId>> = Vec::new();
    let mut style = None;
    for (k, group) in plan.groups.iter().enumerate() {
        if k == 0 || keep[k - 1] {
            groups.push(Vec::new());
            if plan.style == Some(k) {
                style = Some(groups.len() - 1);
            }
        }
        let current = groups.last_mut().expect("pushed above");
        current.extend(group);
        current.sort_unstable();
    }
    plan.groups = groups;
    plan.style = style;
    report.runs = checker.runs;
    report.cached = checker.hits;
    report.millis = checker.millis;
    report.wall_ms = started.elapsed().as_millis() as u64;
    Ok((plan, report))
}

/// EG's result: the groups, the style group, and what moved or merged.
type Regrouped = (Vec<Vec<AtomId>>, Option<usize>, Vec<Dropped>);

/// EG: walks boundaries in order. A failing prefix pulls in the later groups its errors point
/// at (`eg_candidates`), up to `EG_ROUNDS` times; if that does not make it pass, the boundary
/// is merged forward as in D2. Every check request, cached or not, spends the budget, so a
/// warm cache cannot change the plan. Returns the groups, the style group and what moved.
fn error_guided(
    repo: &Repo,
    snap: &Snapshot,
    changes: &Changes,
    analysis: &Analysis,
    plan: &Plan,
    checker: &mut Checker,
) -> Result<Regrouped> {
    let mut groups = plan.groups.clone();
    let mut style = plan.style;
    // 1-based planned numbers of each current group, for the report.
    let mut origin: Vec<Vec<usize>> = (1..=groups.len()).map(|g| vec![g]).collect();
    let mut dropped = Vec::new();
    let mut budget = 2 * groups.len() + 4;
    let check_prefix = |groups: &[Vec<AtomId>],
                        k: usize,
                        budget: &mut usize,
                        checker: &mut Checker|
     -> Result<std::result::Result<crate::check::Outcome, String>> {
        *budget = budget.saturating_sub(1);
        if let Some(path) = syntax_break(changes, analysis, &groups[..=k]) {
            return Ok(Err(path));
        }
        let trees = prefix_trees(repo, &snap.base.tree, changes, &groups[..=k])?;
        Ok(Ok(checker.check(trees.last().expect("non-empty prefix"))?))
    };
    let merge_forward = |groups: &mut Vec<Vec<AtomId>>,
                         origin: &mut Vec<Vec<usize>>,
                         style: &mut Option<usize>,
                         k: usize| {
        let next = groups.remove(k + 1);
        groups[k].extend(next);
        groups[k].sort_unstable();
        let o = origin.remove(k + 1);
        origin[k].extend(o);
        *style = match *style {
            Some(s) if s == k + 1 => None,
            Some(s) if s > k + 1 => Some(s - 1),
            s => s,
        };
    };
    let mut k = 0;
    while k + 1 < groups.len() {
        if budget == 0 {
            // Out of budget: the rest becomes one commit, whose prefix is the passing final tree.
            while k + 1 < groups.len() {
                let after = origin[k + 1][0] - 1;
                dropped.push(Dropped::CheckFailed {
                    after,
                    tail: "(error-guided repair: out of budget)".into(),
                });
                merge_forward(&mut groups, &mut origin, &mut style, k);
            }
            break;
        }
        let outcome = match check_prefix(&groups, k, &mut budget, checker)? {
            Err(path) => {
                dropped.push(Dropped::Syntax {
                    after: origin[k + 1][0] - 1,
                    path,
                });
                merge_forward(&mut groups, &mut origin, &mut style, k);
                continue;
            }
            Ok(o) if o.passed => {
                k += 1;
                continue;
            }
            Ok(o) => o,
        };
        let mut trial = (groups.clone(), origin.clone(), style);
        let mut tail = outcome.tail.clone();
        let mut moved: Vec<usize> = Vec::new();
        let mut fixed = false;
        let rounds = std::env::var("GIT_CARVE_DEV_EG_ROUNDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(EG_ROUNDS);
        for _ in 0..rounds {
            if budget == 0 {
                break;
            }
            let cands = eg_candidates(changes, analysis, &trial.0, k, &tail, trial.2);
            if std::env::var_os("GIT_CARVE_DEV_EG_TRACE").is_some() {
                eprintln!(
                    "eg: boundary {k} of {}: candidates {cands:?}; errors at {:?}",
                    trial.0.len(),
                    error_locations(&tail)
                );
            }
            let after: usize = trial.0[k + 1..].iter().map(Vec::len).sum();
            let pulled: usize = cands.iter().map(|&j| trial.0[j].len()).sum();
            // Never pull everything that is left: then there is no split to repair.
            if cands.is_empty() || cands.len() > EG_MAX_PULL || pulled >= after {
                break;
            }
            for &j in cands.iter().rev() {
                let atoms = trial.0.remove(j);
                trial.0[k].extend(atoms);
                let o = trial.1.remove(j);
                moved.extend(&o);
                trial.1[k].extend(o);
                trial.2 = trial.2.map(|s| if s > j { s - 1 } else { s });
            }
            trial.0[k].sort_unstable();
            let checked = check_prefix(&trial.0, k, &mut budget, checker)?;
            if std::env::var_os("GIT_CARVE_DEV_EG_TRACE").is_some() {
                eprintln!(
                    "eg:   trial: {}",
                    match &checked {
                        Err(path) => format!("syntax break in {path}"),
                        Ok(o) if o.passed => "passes".to_owned(),
                        Ok(_) => "fails".to_owned(),
                    }
                );
            }
            match checked {
                Err(_) => break,
                Ok(o) if o.passed => {
                    fixed = true;
                    break;
                }
                Ok(o) => tail = o.tail,
            }
        }
        if fixed {
            moved.sort_unstable();
            dropped.push(Dropped::Pulled {
                into: k + 1,
                groups: moved,
            });
            (groups, origin, style) = trial;
            k += 1;
        } else {
            dropped.push(Dropped::CheckFailed {
                after: origin[k + 1][0] - 1,
                tail: outcome.tail,
            });
            merge_forward(&mut groups, &mut origin, &mut style, k);
        }
    }
    Ok((groups, style, dropped))
}

/// EG candidates: later groups (index > `k`, never `style`) the errors in the tree of
/// `groups[..=k]` point at. First tier that finds any: (a1) an error line inside an atom that
/// is not applied yet; (a2) an error line, outside every atom, that references a same-file
/// definition an unapplied atom changes (tsc reports at the use site, not the declaration).
fn eg_candidates(
    changes: &Changes,
    analysis: &Analysis,
    groups: &[Vec<AtomId>],
    k: usize,
    tail: &str,
    style: Option<usize>,
) -> BTreeSet<usize> {
    let group_of: BTreeMap<AtomId, usize> = groups
        .iter()
        .enumerate()
        .flat_map(|(g, atoms)| atoms.iter().map(move |&id| (id, g)))
        .collect();
    let later = |id: AtomId| {
        group_of
            .get(&id)
            .copied()
            .filter(|&g| g > k && Some(g) != style)
    };
    let (mut exact, mut by_ref) = (BTreeSet::new(), BTreeSet::new());
    for (path, line) in error_locations(tail) {
        let at = line.saturating_sub(1);
        let Some(f) = changes.files.iter().position(|file| {
            let p = show(&file.path);
            p == path || p.ends_with(&format!("/{path}")) || path.ends_with(&format!("/{p}"))
        }) else {
            continue;
        };
        let mut atoms: Vec<AtomId> = changes.file_atoms[f]
            .iter()
            .map(|&i| changes.atoms[i].id)
            .collect();
        atoms.sort_by_key(|&id| match &changes.atom(id).kind {
            AtomKind::Region(r) => r.base.start,
            _ => 0,
        });
        // Walk the file as it is in the prefix tree: applied atoms at their new length,
        // the others (later groups, leave) at their base length.
        let mut offset: i64 = 0;
        let mut inside = false;
        for &id in &atoms {
            let applied = group_of.get(&id).is_some_and(|&g| g <= k);
            match &changes.atom(id).kind {
                AtomKind::Region(r) => {
                    let start = (i64::from(r.base.start) + offset).max(0) as u32;
                    if at < start {
                        break;
                    }
                    let (new_len, base_len) = (r.new.end - r.new.start, r.base.end - r.base.start);
                    let len = if applied { new_len } else { base_len };
                    if at < start + len.max(1) {
                        inside = true;
                        if !applied {
                            exact.extend(later(id));
                        }
                        break;
                    }
                    if applied {
                        offset += i64::from(new_len) - i64::from(base_len);
                    }
                }
                _ => {
                    inside = true;
                    if !applied {
                        exact.extend(later(id));
                    }
                    break;
                }
            }
        }
        if inside {
            continue;
        }
        // A context line: in base coordinates, it references definitions of this file.
        let b = (i64::from(at) - offset).max(0) as u32;
        let Some(facts) = analysis.base.get(f).and_then(Option::as_ref) else {
            continue;
        };
        for name in facts.refs.iter().filter(|r| r.line == b).map(|r| &r.name) {
            let def = facts
                .defs
                .iter()
                .filter(|d| &d.name == name && d.name_line <= b)
                .max_by_key(|d| d.name_line)
                .or_else(|| facts.defs.iter().find(|d| &d.name == name));
            let Some(def) = def else { continue };
            for &id in &atoms {
                if let AtomKind::Region(r) = &changes.atom(id).kind {
                    let touches = if r.base.start == r.base.end {
                        def.lines.start <= r.base.start && r.base.start < def.lines.end
                    } else {
                        r.base.start < def.lines.end && def.lines.start < r.base.end
                    };
                    if touches {
                        by_ref.extend(later(id));
                    }
                }
            }
        }
    }
    if exact.is_empty() { by_ref } else { exact }
}

/// The base (HEAD, or the base of a re-carved range) fails the check. When its errors are
/// recognizable, say how many and offer D43.
fn head_fails(
    changes: &Changes,
    checker: &Checker,
    base: &crate::check::Outcome,
    what: &str,
) -> Error {
    let manifests = changes.files.iter().any(|f| {
        let name = f.path.rsplit(|&b| b == b'/').next().unwrap_or(&f.path);
        name == b"package.json"
            || name.ends_with(b"lock.json")
            || name.ends_with(b".lock")
            || name.ends_with(b"lock.yaml")
    });
    let note = if manifests {
        "\nnote: your changes touch package.json or a lockfile; the check runs against your \
         current node_modules, which matches your working tree, not the base"
    } else {
        ""
    };
    let hint = match &base.errors {
        Some(errors) if !checker.no_new_errors => format!(
            "hint: {what} already has {} error{}. To carve anyway, so that no commit adds a new \
             one: git carve --no-new-errors",
            errors.len(),
            if errors.len() == 1 { "" } else { "s" }
        ),
        _ => "hint: if the check needs untracked files (such as .env), list them in carve.link"
            .to_owned(),
    };
    Error::CheckFailed(format!(
        "the check fails on {what}, before any of your changes\ncheck: {}\n{}{note}\n{hint}",
        checker.spec.display, base.tail
    ))
}

/// D40: the smallest set of groups whose absence lets the rest of the working tree pass, or
/// `None`. First the check's own error locations pick the groups (code before model: the
/// compiler knows which lines it rejects), for a few rounds; then each group alone is tried,
/// smallest first. Every candidate is a real check run on a real tree.
fn rescue(
    repo: &Repo,
    snap: &Snapshot,
    changes: &Changes,
    plan: &Plan,
    tail: &str,
    checker: &mut Checker,
) -> Result<Option<BTreeSet<usize>>> {
    let n = plan.groups.len();
    let passes_without =
        |left: &BTreeSet<usize>, checker: &mut Checker| -> Result<(bool, String)> {
            let kept: Vec<Vec<AtomId>> = (0..n)
                .filter(|g| !left.contains(g))
                .map(|g| plan.groups[g].clone())
                .collect();
            if kept.is_empty() {
                return Ok((false, String::new()));
            }
            let trees = prefix_trees(repo, &snap.base.tree, changes, &kept)?;
            let outcome = checker.check(trees.last().expect("non-empty"))?;
            Ok((outcome.passed, outcome.tail))
        };
    let mut left = implicated(changes, &plan.groups, tail);
    for _ in 0..RESCUE_ROUNDS {
        if left.is_empty() || left.len() == n {
            break;
        }
        let (passed, tail) = passes_without(&left, checker)?;
        if passed {
            return Ok(Some(left));
        }
        let kept: Vec<Vec<AtomId>> = (0..n)
            .filter(|g| !left.contains(g))
            .map(|g| plan.groups[g].clone())
            .collect();
        let more: Vec<usize> = implicated(changes, &kept, &tail)
            .into_iter()
            .map(|k| {
                (0..n)
                    .filter(|g| !left.contains(g))
                    .nth(k)
                    .expect("kept index")
            })
            .collect();
        if more.is_empty() {
            break;
        }
        left.extend(more);
    }
    if n <= RESCUE_SEARCH_MAX && n > 1 {
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by_key(|&g| (plan.groups[g].len(), std::cmp::Reverse(g)));
        for g in order {
            let one = BTreeSet::from([g]);
            if passes_without(&one, checker)?.0 {
                return Ok(Some(one));
            }
        }
    }
    Ok(None)
}

/// D40: `path(line,col)` (tsc) or `path:line:col` (most other tools) at the start of a line
/// of check output, as a repository path and a 1-based line.
fn error_locations(tail: &str) -> Vec<(String, u32)> {
    let digits = |s: &str| -> Option<u32> {
        let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        s[..end].parse().ok()
    };
    tail.lines()
        .filter_map(|line| {
            let line = line.trim_start();
            if let Some(open) = line.find('(') {
                let (path, rest) = (&line[..open], &line[open + 1..]);
                if !path.is_empty() && !path.contains(' ') {
                    if let Some(l) = digits(rest) {
                        if rest[l.to_string().len()..].starts_with(',') {
                            return Some((path.trim_start_matches("./").to_owned(), l));
                        }
                    }
                }
            }
            let (path, rest) = line.split_once(':')?;
            (!path.is_empty() && !path.contains(' '))
                .then(|| digits(rest))
                .flatten()
                .map(|l| (path.trim_start_matches("./").to_owned(), l))
        })
        .collect()
}

/// The lines of check output that name a location, to quote in the buffer.
fn error_lines(tail: &str) -> Vec<String> {
    let located: Vec<String> = tail
        .lines()
        .filter(|l| !error_locations(l).is_empty())
        .take(RESCUE_QUOTE)
        .map(|l| l.trim().to_owned())
        .collect();
    if located.is_empty() {
        tail.lines()
            .filter(|l| !l.trim().is_empty())
            .take(RESCUE_QUOTE)
            .map(|l| l.trim().to_owned())
            .collect()
    } else {
        located
    }
}

/// D40: the groups (indices into `groups`) holding a changed line the check's output points
/// at. Lines are counted in the tree that was checked: base plus `groups`, without `leave`,
/// so each atom's position is recomputed from the atoms before it in its file.
fn implicated(changes: &Changes, groups: &[Vec<AtomId>], tail: &str) -> BTreeSet<usize> {
    let group_of: BTreeMap<AtomId, usize> = groups
        .iter()
        .enumerate()
        .flat_map(|(g, atoms)| atoms.iter().map(move |&id| (id, g)))
        .collect();
    let mut out = BTreeSet::new();
    for (path, line) in error_locations(tail) {
        let at = line.saturating_sub(1);
        let Some(f) = changes.files.iter().position(|file| {
            let p = show(&file.path);
            p == path || p.ends_with(&format!("/{path}")) || path.ends_with(&format!("/{p}"))
        }) else {
            continue;
        };
        // Selected atoms of this file, in base order, with their offset in the checked tree.
        let mut offset: i64 = 0;
        let mut atoms: Vec<AtomId> = changes.file_atoms[f]
            .iter()
            .map(|&i| changes.atoms[i].id)
            .filter(|id| group_of.contains_key(id))
            .collect();
        atoms.sort_by_key(|&id| match &changes.atom(id).kind {
            AtomKind::Region(r) => r.base.start,
            _ => 0,
        });
        for id in atoms {
            let hit = match &changes.atom(id).kind {
                AtomKind::Region(r) => {
                    let start = (i64::from(r.base.start) + offset) as u32;
                    let len = r.new.end - r.new.start;
                    offset += i64::from(len) - i64::from(r.base.end - r.base.start);
                    at >= start && at < start + len.max(1)
                }
                // A whole-file change (added, deleted, binary, mode): any line of it.
                _ => true,
            };
            if hit {
                out.insert(group_of[&id]);
            }
        }
    }
    out
}

/// The group holding the atom nearest to `id` in the same file, by line.
fn nearest_group(changes: &Changes, groups: &[Vec<AtomId>], id: AtomId) -> Option<usize> {
    let line = |id: AtomId| match &changes.atom(id).kind {
        AtomKind::Region(r) => r.new.start.max(r.base.start),
        _ => 0,
    };
    let file = changes.atom(id).file;
    groups
        .iter()
        .enumerate()
        .filter_map(|(g, atoms)| {
            atoms
                .iter()
                .filter(|&&a| changes.atom(a).file == file)
                .map(|&a| line(a).abs_diff(line(id)))
                .min()
                .map(|d| (d, g))
        })
        .min()
        .map(|(_, g)| g)
}

/// D14: a file that parses in HEAD and in the working tree but not after `prefix`.
fn syntax_break(changes: &Changes, analysis: &Analysis, prefix: &[Vec<AtomId>]) -> Option<String> {
    let selected: BTreeSet<AtomId> = prefix.iter().flatten().copied().collect();
    let files: BTreeSet<usize> = selected.iter().map(|&id| changes.atom(id).file).collect();
    for f in files {
        let clean =
            |facts: &Option<crate::lang::FileFacts>| facts.as_ref().is_some_and(|x| !x.has_error);
        if !clean(&analysis.base[f]) || !clean(&analysis.fin[f]) {
            continue;
        }
        let path = &changes.files[f].path;
        let Some(adapter) = adapter_for(path) else {
            continue;
        };
        if let Target::Blob { bytes, .. } = changes.target(f, &|id| selected.contains(&id)) {
            if adapter
                .analyze(path, &bytes)
                .is_none_or(|facts| facts.has_error)
            {
                return Some(show(path));
            }
        }
    }
    None
}

fn probe(
    repo: &Repo,
    changes: &Changes,
    plan: &Plan,
    final_tree: &str,
    checker: &mut Checker,
    report: &mut Verification,
    ahead: &[String],
) -> Result<()> {
    // Config files decide what a check reads; their ids key the remembered verdict.
    let listing = repo.git.run(["ls-tree", "-r", "-z", final_tree])?;
    let mut config_dirs: BTreeSet<Vec<u8>> = BTreeSet::new();
    let mut config_ids = Vec::new();
    for entry in listing.split(|&b| b == 0).filter(|e| !e.is_empty()) {
        let Some(tab) = entry.iter().position(|&b| b == b'\t') else {
            continue;
        };
        let path = &entry[tab + 1..];
        let name = path.rsplit(|&b| b == b'/').next().unwrap_or(path);
        let is_tsconfig = name.starts_with(b"tsconfig") && name.ends_with(b".json");
        if is_tsconfig || name == b"package.json" {
            config_ids.push(show(entry));
        }
        if is_tsconfig {
            config_dirs.insert(
                path[..path.len() - name.len()]
                    .strip_suffix(b"/")
                    .unwrap_or(b"")
                    .to_vec(),
            );
        }
    }
    let configs = config_ids.join(";");

    // One changed, committed, parseable file per nearest-config directory.
    let grouped: BTreeSet<AtomId> = plan.groups.iter().flatten().copied().collect();
    let mut per_dir: BTreeMap<Vec<u8>, (usize, usize)> = BTreeMap::new();
    let mut outside: Vec<String> = Vec::new();
    for (f, file) in changes.files.iter().enumerate() {
        let atoms = changes.file_atoms[f]
            .iter()
            .filter(|&&i| grouped.contains(&changes.atoms[i].id))
            .count();
        let source = atoms > 0
            && adapter_for(&file.path).is_some()
            && file.new.as_ref().is_some_and(|e| e.mode.is_regular());
        if !source {
            continue;
        }
        // D45: a source file the check does not claim to read is unverified, not probed.
        if !checker.spec.probes(&file.path) {
            outside.push(show(&file.path));
            continue;
        }
        let mut dir = file.path.rsplit_once_dir();
        while !config_dirs.contains(&dir) && !dir.is_empty() {
            dir = dir.rsplit_once_dir();
        }
        // Files under one config can still differ in coverage (`include: ["src"]` leaves
        // `scripts/` out), so the first folder below the config is part of the key.
        let below = if dir.is_empty() {
            &file.path[..]
        } else {
            &file.path[dir.len() + 1..]
        };
        let folder: Vec<u8> = below
            .split(|&b| b == b'/')
            .next()
            .filter(|_| below.contains(&b'/'))
            .unwrap_or(b"")
            .to_vec();
        // D54: a file the check only may read (JavaScript under tsc) gets its own slot and its
        // own remembered verdict, so its miss is never replayed for, or taken from, a hard file.
        let soft = checker
            .spec
            .soft_reads
            .contains(&crate::lang::extension(&file.path));
        let class: &[u8] = if soft { b"\0soft" } else { b"" };
        let dir = [dir, b"\0".to_vec(), folder, class.to_vec()].concat();
        let entry = per_dir.entry(dir).or_insert((f, atoms));
        if atoms > entry.1 {
            *entry = (f, atoms);
        }
    }
    // (path, key, remembered verdict, or the broken tree to check)
    let mut found: Vec<(String, String, Option<bool>, Option<String>)> = Vec::new();
    // Hard slots first, so soft ones never crowd them out of the probe limit.
    let (hard, soft): (Vec<_>, Vec<_>) = per_dir
        .into_iter()
        .partition(|(dir, _)| !dir.ends_with(b"\0soft"));
    for (dir, (f, _)) in hard.into_iter().chain(soft).take(MAX_PROBES) {
        let file = &changes.files[f];
        let path = show(&file.path);
        let key = format!("{configs}|{}", show(&dir).replace('\0', "|"));
        match checker.probe_known(&key) {
            Some(caught) => found.push((path, key, Some(caught), None)),
            None => {
                let new = file
                    .new
                    .as_ref()
                    .expect("usable files exist in the working tree");
                let content = repo.read_blobs(&[new.oid.as_str()])?.remove(0);
                let adapter = adapter_for(&file.path).expect("usable files have an adapter");
                let mut builder = TreeBuilder::new(repo, "probe", final_tree)?;
                builder.set(vec![(
                    file.path.clone(),
                    Target::Blob {
                        mode: new.mode,
                        bytes: adapter.corrupt(&content),
                    },
                )])?;
                found.push((path, key, None, Some(builder.write()?)));
            }
        }
    }
    // D51: the broken trees, then the prefixes ahead, in shared rounds.
    let mut trees: Vec<String> = found.iter().filter_map(|f| f.3.clone()).collect();
    if !trees.is_empty() {
        trees.extend(ahead.iter().cloned());
    }
    let mut outcomes = checker.check_many(&trees, false)?.into_iter();
    for (path, key, known, _) in found {
        let (caught, remembered) = match known {
            Some(caught) => (caught, true),
            None => {
                let caught = !outcomes.next().flatten().expect("checked").passed;
                checker.remember_probe(&key, caught)?;
                (caught, false)
            }
        };
        report.probes.push(Probe {
            path,
            caught,
            remembered,
        });
    }
    // D27: a check that reads none of the probed files is vacuous, so the default check is
    // refused (D1). A check that reads some of them proves those; the rest are reported as
    // unverified, whoever chose the check.
    // D54: a missed probe on a file the check only may read (JavaScript under tsc) is never
    // evidence of a vacuous check; the file is reported as unverified.
    let soft = |path: &str| {
        checker
            .spec
            .soft_reads
            .contains(&crate::lang::extension(path.as_bytes()))
    };
    let caught_any = report.probes.iter().any(|p| p.caught);
    if let Some(first) = report.probes.iter().find(|p| !p.caught && !soft(&p.path)) {
        if !caught_any && !checker.spec.explicit {
            return Err(Error::CheckFailed(format!(
                "the default check passed on a deliberately broken {}, so it does not \
                 read that file and cannot prove anything about it\ncheck: {}\n\
                 hint: pass --check, or set git config carve.check, with a command that checks it",
                first.path, checker.spec.display
            )));
        }
    }
    report.unverified = report
        .probes
        .iter()
        .filter(|p| !p.caught)
        .map(|p| p.path.clone())
        .chain(outside)
        .collect();
    Ok(())
}

trait DirOf {
    fn rsplit_once_dir(&self) -> Vec<u8>;
}

impl DirOf for Vec<u8> {
    /// The parent directory of a repository path (`b""` at the top).
    fn rsplit_once_dir(&self) -> Vec<u8> {
        match self.iter().rposition(|&b| b == b'/') {
            Some(i) => self[..i].to_vec(),
            None => Vec::new(),
        }
    }
}

/// Re-verifies a grouping the user edited (§9.5). Never merges the user's explicit
/// choices: returns the first failing commit (1-based) and why, or `None`. HEAD and the
/// probe were already checked for this state; the final tree is re-checked because moving
/// atoms to or from `leave` changes it.
pub fn check_plan(
    repo: &Repo,
    snap: &Snapshot,
    changes: &Changes,
    analysis: &Analysis,
    groups: &[Vec<AtomId>],
    checker: &mut Checker,
) -> Result<Option<(usize, String)>> {
    if groups.is_empty() {
        return Ok(None);
    }
    let trees = prefix_trees(repo, &snap.base.tree, changes, groups)?;
    let n = groups.len();
    // The final tree, then each prefix up to the first one that would not parse, in that
    // order and up to `jobs` at once (D51): the first failure in this order is the answer.
    let parses = (0..n - 1)
        .find(|&k| syntax_break(changes, analysis, &groups[..=k]).is_some())
        .unwrap_or(n - 1);
    let mut order = vec![trees[n - 1].clone()];
    order.extend(trees[..parses].iter().cloned());
    let outcomes = checker.check_many(&order, true)?;
    for (i, outcome) in outcomes.into_iter().enumerate() {
        match outcome {
            Some(o) if !o.passed => return Ok(Some((if i == 0 { n } else { i }, o.tail))),
            Some(_) => {}
            None => break,
        }
    }
    if parses < n - 1 {
        let path = syntax_break(changes, analysis, &groups[..=parses]).expect("found above");
        return Ok(Some((
            parses + 1,
            format!("{path} would not parse after this commit"),
        )));
    }
    Ok(None)
}
