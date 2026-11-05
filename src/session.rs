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

/// `carve.agentMessages` (D49): an agent may write the message lines of a plan file.
fn agent_messages(repo: &Repo) -> Result<bool> {
    Ok(repo
        .git
        .try_run_str(["config", "--type=bool", "carve.agentMessages"])?
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
    let plan = graph::plan_with(&changes, &analysis, opts.judge.as_ref());
    if opts.no_check {
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
    let (plan, verification) =
        verify::verify(repo, &snap, &changes, &analysis, &plan, &mut checker)?;
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
    let mut prepared = prepare(repo, opts)?;
    if prepared.plan.groups.is_empty() {
        return Err(Error::Aborted(
            "every change is left out (large or binary new files, debug lines); nothing to commit"
                .into(),
        ));
    }
    // D52: an earlier stack's layers are adopted (moved, so their PRs survive) when they
    // still form this branch's stack, and refused before the editor otherwise.
    let recovered = commit::recover_layers(repo)?;
    if !recovered.is_empty() {
        return Err(Error::Concurrent(format!(
            "an interrupted carve moved {} and they moved again since; check them, then re-run",
            recovered.join(", ")
        )));
    }
    let adopted = match &stack_branch {
        Some(branch) => match crate::stack::adopt(
            repo,
            branch,
            &prepared.snap.base.commit,
            &prepared.snap.head.commit,
        )? {
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
    let saved_path = repo.carve_dir.join("PLAN.saved");
    let saved: Saved = fs::read(&saved_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let (messages, mut notes) = plan::restore(&saved, &prepared.changes, &prepared.plan.groups);
    if !adopted.is_empty() {
        // D52: positional, so the order of the commit blocks below decides which PR shows what.
        let names: Vec<&str> = adopted.iter().map(|a| a.name.as_str()).collect();
        notes.push(format!(
            "# stack: commits 1-{} move {} (adopted: their PRs stay); the last commit is the branch itself",
            names.len(),
            names.join(", ")
        ));
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
        // Messages already in the file are shown as proposals, so saving untouched (or an
        // editor like `true`) cannot turn them into commits (D49 review).
        if !agent_messages(repo)? {
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
                    && !agent_messages(repo)?
                {
                    return Err(crate::error::precondition(
                        "the plan file has commit messages in it, and carve takes messages only from \
                         its editor (DECISIONS.md D49)",
                        Some(
                            "review it and write them yourself: add --edit. To let agents write \
                             messages in this repository: git config carve.agentMessages true",
                        ),
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
            .map_or(0, |v| v.dropped.len()),
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
