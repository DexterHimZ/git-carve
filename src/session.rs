//! One `git carve` run end to end: plan, verify, edit, validate, re-verify, commit
//! (SPEC §9–§10). Messages come only from the user's buffer.

use std::fs;
use std::path::PathBuf;

use crate::atom::{AtomId, Changes, atomize};
use crate::check::{self, Checker};
use crate::commit::{self, Group, Hooks, UndoRecord};
use crate::editor;
use crate::error::{Error, Result, io};
use crate::facts::{self, Analysis};
use crate::graph::{self, Plan};
use crate::plan::{self, Parsed, RenderOptions, Saved};
use crate::precondition;
use crate::repo::Repo;
use crate::snapshot::{self, Snapshot};
use crate::verify::{self, Verification};

const ATTEMPTS: usize = 3;

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub check: Option<String>,
    pub no_check: bool,
    /// `--plan-file`: a filled buffer instead of the editor (DECISIONS.md D6).
    pub plan_file: Option<PathBuf>,
    /// `--edit` (D49): the editor opens on the plan file, and the human writes the messages.
    pub edit: bool,
    /// The learned judge deciding soft merges (DECISIONS.md D28); code-only when `None`.
    pub judge: Option<crate::judge::LearnedJudge>,
    /// `--no-new-errors` (D43): when HEAD fails the check, no commit may add a new error.
    pub no_new_errors: bool,
    /// `git carve <base>` (D46 phase 1): re-carve the commits since this base too.
    pub base: Option<crate::precondition::ResolvedBase>,
    /// `--stack` (D48): one branch per commit but the last, on top of the range's base.
    pub stack: bool,
    /// `--layers` (D59): with `--stack`, at most this many commits; `None` for no limit.
    pub layers: Option<usize>,
    /// `layers` is the default, not the user's: an adopted stack's shape wins over it.
    pub layers_default: bool,
    /// Parallel check workers, overriding `carve.checkJobs` (D51; the speed benchmark).
    pub jobs: Option<usize>,
}

/// Everything the summary (§10.4) needs.
#[derive(Clone, Debug)]
pub struct Summary {
    pub record: UndoRecord,
    pub subjects: Vec<String>,
    /// `None` with `--no-check`.
    pub verification: Option<Verification>,
    pub merges: usize,
    /// Atoms missing from the buffer, kept in the working tree.
    pub missing: Vec<String>,
    /// The re-carved range, when there was one.
    pub range: Option<crate::precondition::ResolvedBase>,
    /// `--stack`: the current branch and its new layer branches, bottom first.
    pub stack: Option<StackSummary>,
}

#[derive(Clone, Debug)]
pub struct StackSummary {
    /// The top layer: the branch carve committed on.
    pub branch: String,
    /// `(branch, commit, subject)` for layers 1..N-1.
    pub layers: Vec<(String, String, String)>,
    /// D52: how many of `layers`, from the bottom, are an earlier stack's branches, moved.
    pub adopted: usize,
    /// D52: an earlier stack's layers above the new ones, left where they were.
    pub stale: Vec<String>,
}

/// Plans and verifies (the `--plan-only` part of a run).
pub struct Prepared<'r> {
    pub snap: Snapshot,
    pub changes: Changes,
    pub analysis: Analysis,
    pub plan: Plan,
    pub verification: Option<Verification>,
    pub checker: Option<Checker<'r>>,
    /// Range carves only: where each atom's lines came from (display only).
    pub sources: crate::attribution::Sources,
}

/// `carve.requireHumanMessages` (D58, reversing D49's default): message lines come only from
/// the editor. A plan file with filled messages is refused without `--edit`, and with
/// `--edit` its messages open as `# proposed:` comments. Off by default: whoever fills a
/// plan file (a human or an agent) supplies its messages, and `plan_source` records which.
fn require_human_messages(repo: &Repo) -> Result<bool> {
    Ok(repo
        .git
        .try_run_str(["config", "--type=bool", "carve.requireHumanMessages"])?
        .is_some_and(|v| v.trim() == "true"))
}

pub fn prepare<'r>(repo: &'r Repo, opts: &Options) -> Result<Prepared<'r>> {
    precondition::check_state(repo)?;
    let base = opts
        .base
        .as_ref()
        .filter(|b| !b.commits.is_empty())
        .map(|b| b.oid.as_str());
    let snap = snapshot::take_since(repo, base)?;
    let changes = atomize(repo, &snap)?;
    precondition::check_size(repo, &changes)?;
    let sources = crate::attribution::attribute(repo, &snap, &changes)?;
    let analysis = facts::analyze(repo, &changes)?;
    let mut plan = graph::plan_with(&changes, &analysis, opts.judge.as_ref());
    if opts.no_check {
        if let Some(max) = opts.layers {
            cap_layers(&changes, &mut plan, max);
        }
        return Ok(Prepared {
            snap,
            changes,
            analysis,
            plan,
            verification: None,
            checker: None,
            sources,
        });
    }
    let carved: Vec<&[u8]> = changes.files.iter().map(|f| f.path.as_slice()).collect();
    let spec = check::resolve(repo, opts.check.as_deref(), &carved)?;
    let mut checker = Checker::new(repo, spec, &carved)?;
    checker.no_new_errors = opts.no_new_errors;
    if let Some(jobs) = opts.jobs {
        checker.jobs = jobs.max(1);
    }
    let (mut plan, mut verification) =
        verify::verify(repo, &snap, &changes, &analysis, &plan, &mut checker)?;
    if let Some(max) = opts.layers {
        let from = plan.groups.len();
        if cap_layers(&changes, &mut plan, max) {
            verification.dropped.push(verify::Dropped::Layers {
                from,
                to: plan.groups.len(),
            });
        }
    }
    Ok(Prepared {
        snap,
        changes,
        analysis,
        plan,
        verification: Some(verification),
        checker: Some(checker),
        sources,
    })
}

/// D59: merges the adjacent pair of groups with the fewest changed lines (the earlier pair on a
/// tie) until at most `max` remain. Each merge only removes a boundary, so every remaining
/// prefix is the same tree as before: a verified plan stays verified without a check run.
/// Returns whether anything merged.
fn cap_layers(changes: &Changes, plan: &mut Plan, max: usize) -> bool {
    let size = |id: &AtomId| -> usize {
        match &changes.atom(*id).kind {
            crate::atom::AtomKind::Region(r) => {
                (r.new.end - r.new.start + r.base.end - r.base.start) as usize
            }
            _ => 1,
        }
    };
    let mut sizes: Vec<usize> = plan
        .groups
        .iter()
        .map(|g| g.iter().map(size).sum())
        .collect();
    let mut merged = false;
    while max > 0 && plan.groups.len() > max {
        let i = (0..plan.groups.len() - 1)
            .min_by_key(|&i| (sizes[i] + sizes[i + 1], i))
            .expect("at least two groups");
        let next = plan.groups.remove(i + 1);
        plan.groups[i].extend(next);
        plan.groups[i].sort_unstable();
        sizes[i] += sizes.remove(i + 1);
        plan.style = match plan.style {
            Some(s) if s == i || s == i + 1 => None,
            Some(s) if s > i + 1 => Some(s - 1),
            s => s,
        };
        merged = true;
    }
    merged
}

/// `opts` with the default `--layers` cap raised to keep an adopted stack's `layers` branches
/// plus its top (D59 review: the existing shape wins over the default, never over an
/// explicit `--layers`).
fn with_room_for(opts: &Options, layers: usize) -> Options {
    let mut out = opts.clone();
    if out.layers_default {
        out.layers = out.layers.map(|cap| cap.max(layers + 1));
    }
    out
}

/// The options `--plan-only` plans with: as a run would, with room for the layers an earlier
/// `--stack` left that a re-carve would adopt. Adoption problems are left to the run to report.
pub fn plan_only_options(repo: &Repo, opts: &Options) -> Result<Options> {
    if !opts.stack || !opts.layers_default {
        return Ok(opts.clone());
    }
    let Some(branch) = repo
        .git
        .try_run_str(["symbolic-ref", "-q", "--short", "HEAD"])?
    else {
        return Ok(opts.clone());
    };
    let head = repo.git.run_str(["rev-parse", "HEAD"])?;
    let base = opts
        .base
        .as_ref()
        .filter(|b| !b.commits.is_empty())
        .map_or(head.clone(), |b| b.oid.clone());
    let adopted = crate::stack::adopt(repo, &branch, &base, &head)?.unwrap_or_default();
    Ok(with_room_for(opts, adopted.len()))
}

/// A full run: the plan buffer, then commits.
pub fn run(repo: &Repo, opts: &Options) -> Result<Summary> {
    let stack_branch = if opts.stack {
        let head = repo
            .git
            .try_run_str(["symbolic-ref", "-q", "--short", "HEAD"])?;
        Some(head.ok_or_else(|| {
            crate::error::precondition(
                "a stack needs a branch, and HEAD is detached",
                Some("`git switch -c <name>` names it"),
            )
        })?)
    } else {
        None
    };
    // D52: an earlier stack's layers are adopted (moved, so their PRs survive) when they
    // still form this branch's stack, and refused before the editor otherwise. Known before
    // planning, so the default `--layers` cap can make room for them (D59 review).
    let recovered = commit::recover_layers(repo)?;
    if !recovered.is_empty() {
        return Err(Error::Concurrent(format!(
            "an interrupted carve moved {} and they moved again since; check them, then re-run",
            recovered.join(", ")
        )));
    }
    let head = repo.git.run_str(["rev-parse", "HEAD"])?;
    let base = opts
        .base
        .as_ref()
        .filter(|b| !b.commits.is_empty())
        .map_or(head.clone(), |b| b.oid.clone());
    let adopted = match &stack_branch {
        Some(branch) => match crate::stack::adopt(repo, branch, &base, &head)? {
            Ok(adopted) => adopted,
            Err(why) => {
                return Err(crate::error::precondition(
                    format!("branches from an earlier --stack cannot be adopted: {why}"),
                    Some(
                        "a re-carve adopts layers `<branch>-1`, `<branch>-2`, ... that are commits of this branch, in order; rename a stray one with `git branch -m` (a layer branch may carry a PR)",
                    ),
                ));
            }
        },
        None => Vec::new(),
    };
    let owned = with_room_for(opts, adopted.len());
    let opts = &owned;
    let mut prepared = prepare(repo, opts)?;
    if prepared.plan.groups.is_empty() {
        return Err(Error::Aborted(
            "every change is left out (large or binary new files, debug lines); nothing to commit"
                .into(),
        ));
    }
    let saved_path = repo.carve_dir.join("PLAN.saved");
    let saved: Saved = fs::read(&saved_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let (messages, mut notes) = plan::restore(&saved, &prepared.changes, &prepared.plan.groups);
    if !adopted.is_empty() {
        // D52: positional, so the order of the commit blocks below decides which PR shows what.
        // Only as many layers move as the plan has commits below the top (D59 review).
        let moving = adopted
            .len()
            .min(prepared.plan.groups.len().saturating_sub(1));
        let names = |range: &[crate::stack::Adopted]| -> String {
            range
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut note = if moving > 0 {
            format!(
                "# stack: commits 1-{moving} move {} (adopted: their PRs stay)",
                names(&adopted[..moving])
            )
        } else {
            "# stack: no adopted layer moves".to_owned()
        };
        if moving < adopted.len() {
            note.push_str(&format!(
                "; {} stay where they are (close their PRs)",
                names(&adopted[moving..])
            ));
        }
        note.push_str("; the last commit is the branch itself");
        notes.push(note);
    }
    let diff_lines = repo.config_u64("carve.planDiff", plan::DIFF_TOTAL_DEFAULT as u64)? as usize;
    let mut text = plan::render_range(
        &prepared.changes,
        &prepared.analysis,
        &prepared.plan,
        &RenderOptions {
            verification: prepared.verification.as_ref(),
            messages: &messages,
            notes: &notes,
            diff_lines,
            unsure: true,
        },
        opts.base
            .as_ref()
            .map(|base| plan::RangeView {
                base,
                sources: &prepared.sources,
            })
            .as_ref(),
    );
    let verified_structure = (prepared.plan.groups.clone(), prepared.plan.leave.clone());

    let buffer = repo.carve_dir.join("PLAN");
    // D49: a plan file seeds the editor with --edit; without it, the file is the plan, and
    // its messages are an agent's unless the human runs the editor.
    let from_file = opts.plan_file.as_ref().filter(|_| !opts.edit);
    if let (Some(path), true) = (&opts.plan_file, opts.edit) {
        text = fs::read_to_string(path).map_err(io(format!("read {}", path.display())))?;
        // Under carve.requireHumanMessages, messages already in the file are shown as
        // proposals, so saving untouched (or an editor like `true`) cannot turn them into
        // commits (D49 review).
        if require_human_messages(repo)? {
            text = plan::messages_as_proposals(&text);
        }
    }
    let plan_source = match (&opts.plan_file, opts.edit) {
        (None, _) => "editor",
        (Some(_), true) => "plan-file+edit",
        (Some(_), false) => "plan-file",
    };
    let mut attempt = 0;
    let edited = loop {
        attempt += 1;
        text = match from_file {
            Some(path) => {
                fs::read_to_string(path).map_err(io(format!("read {}", path.display())))?
            }
            None => {
                fs::write(&buffer, &text).map_err(io("write the plan buffer"))?;
                if let Err(err) = editor::edit(repo, &buffer) {
                    return Err(keep_messages(repo, &text, &prepared.changes, err));
                }
                fs::read_to_string(&buffer).map_err(io("read the plan buffer"))?
            }
        };
        let notes: Vec<String> = match plan::parse(&text, &prepared.changes, &prepared.plan.hard) {
            Parsed::Abort(reason) => {
                return Err(keep_messages(
                    repo,
                    &text,
                    &prepared.changes,
                    Error::Aborted(reason),
                ));
            }
            Parsed::Errors(errors) => errors.iter().map(|e| format!("# ERROR: {e}")).collect(),
            Parsed::Plan(edited) => {
                if from_file.is_some()
                    && edited.groups.iter().any(|g| !g.message.trim().is_empty())
                    && require_human_messages(repo)?
                {
                    return Err(crate::error::precondition(
                        "the plan file has commit messages in it, and this repository takes \
                         messages only from carve's editor (carve.requireHumanMessages)",
                        Some("review it and write them yourself: add --edit"),
                    ));
                }
                let (groups, _) = edited.structure();
                let regrouped = edited.structure() != verified_structure;
                let failure = match (&mut prepared.checker, regrouped) {
                    (Some(checker), true) => verify::check_plan(
                        repo,
                        &prepared.snap,
                        &prepared.changes,
                        &prepared.analysis,
                        &groups,
                        checker,
                    )?,
                    _ => None,
                };
                match failure {
                    None => break edited,
                    Some((k, tail)) => {
                        if from_file.is_some() {
                            return Err(Error::CheckFailed(format!(
                                "commit {k} of the plan file does not pass the check; nothing was committed\n{tail}"
                            )));
                        }
                        let mut notes = vec![format!(
                            "# CHECK FAILED at commit {k}; nothing was committed yet:"
                        )];
                        notes.extend(
                            tail.lines()
                                .rev()
                                .take(20)
                                .collect::<Vec<_>>()
                                .into_iter()
                                .rev()
                                .map(|l| format!("#   | {l}")),
                        );
                        notes.push(format!(
                            "# hint: delete the \"commit\" line after commit {k} to merge them"
                        ));
                        notes
                    }
                }
            }
        };
        if from_file.is_some() {
            return Err(Error::Invalid(
                notes.join("\n").replace("# ERROR: ", "plan file: "),
            ));
        }
        if attempt == ATTEMPTS {
            return Err(keep_messages(
                repo,
                &text,
                &prepared.changes,
                Error::Aborted(format!(
                    "the plan still has problems after {ATTEMPTS} attempts; nothing was committed"
                )),
            ));
        }
        text = plan::with_notes(&text, &notes);
    };

    let groups: Vec<Group> = edited
        .groups
        .iter()
        .map(|g| Group {
            atoms: g.atoms.clone(),
            message: g.message.clone(),
        })
        .collect();
    let subjects: Vec<String> = edited
        .groups
        .iter()
        .map(|g| g.message.lines().next().unwrap_or("").to_owned())
        .collect();
    let layers = match &stack_branch {
        Some(branch) => {
            let names = crate::stack::layer_names(branch, &subjects);
            for name in names.iter().skip(adopted.len()) {
                let valid = repo.git.test(["check-ref-format", "--branch", name])?;
                let taken =
                    repo.git
                        .test(["show-ref", "--verify", "-q", &format!("refs/heads/{name}")])?;
                if !valid || taken {
                    let why = if taken {
                        "already exists"
                    } else {
                        "is not a valid branch name"
                    };
                    return Err(keep_messages(
                        repo,
                        &text,
                        &prepared.changes,
                        Error::Invalid(format!(
                            "stack branch {name} {why}; nothing was committed (your messages are kept)"
                        )),
                    ));
                }
            }
            names
                .into_iter()
                .enumerate()
                .map(|(k, name)| match adopted.get(k) {
                    Some(a) => commit::Layer {
                        name: a.name.clone(),
                        was: Some(a.oid.clone()),
                    },
                    None => commit::Layer { name, was: None },
                })
                .collect()
        }
        None => Vec::<commit::Layer>::new(),
    };
    let record = match commit::commit_stacked(
        repo,
        &prepared.snap,
        &prepared.changes,
        &groups,
        &Hooks::default(),
        &layers,
        Some(plan_source),
    ) {
        Ok(record) => record,
        Err(err) => return Err(keep_messages(repo, &text, &prepared.changes, err)),
    };
    let _ = fs::remove_file(&saved_path);
    let _ = fs::remove_file(&buffer);
    let stack = stack_branch.map(|branch| StackSummary {
        branch,
        layers: layers
            .iter()
            .zip(&record.commits)
            .zip(&subjects)
            .map(|((layer, oid), subject)| (layer.name.clone(), oid.clone(), subject.clone()))
            .collect(),
        adopted: adopted.len().min(layers.len()),
        stale: adopted
            .iter()
            .skip(layers.len())
            .map(|a| a.name.clone())
            .collect(),
    });
    Ok(Summary {
        record,
        stack,
        subjects,
        merges: prepared
            .verification
            .as_ref()
            // Boundaries the check took away; `--layers` merges are a choice, not a failure.
            .map_or(0, |v| {
                v.dropped
                    .iter()
                    .filter(|d| !matches!(d, verify::Dropped::Layers { .. }))
                    .count()
            }),
        verification: prepared.verification,
        missing: edited
            .missing
            .iter()
            .map(|&id: &AtomId| prepared.changes.position(id))
            .collect(),
        range: opts.base.clone().filter(|b| !b.commits.is_empty()),
    })
}

/// D17: whatever went wrong after the user typed, keep their messages for the next run.
fn keep_messages(repo: &Repo, text: &str, changes: &Changes, err: Error) -> Error {
    let saved = plan::save(text, changes);
    if !saved.groups.is_empty() {
        if let Ok(json) = serde_json::to_vec(&saved) {
            let _ = fs::write(repo.carve_dir.join("PLAN.saved"), json);
        }
    }
    err
}
