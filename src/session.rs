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
    /// The learned judge deciding soft merges (DECISIONS.md D28); code-only when `None`.
    pub judge: Option<crate::judge::LearnedJudge>,
    /// `--no-new-errors` (D43): when HEAD fails the check, no commit may add a new error.
    pub no_new_errors: bool,
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
}

/// Plans and verifies (the `--plan-only` part of a run).
pub struct Prepared<'r> {
    pub snap: Snapshot,
    pub changes: Changes,
    pub analysis: Analysis,
    pub plan: Plan,
    pub verification: Option<Verification>,
    pub checker: Option<Checker<'r>>,
}

pub fn prepare<'r>(repo: &'r Repo, opts: &Options) -> Result<Prepared<'r>> {
    precondition::check_state(repo)?;
    let snap = snapshot::take(repo)?;
    let changes = atomize(repo, &snap)?;
    precondition::check_size(repo, &changes)?;
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
        });
    }
    let carved: Vec<&[u8]> = changes.files.iter().map(|f| f.path.as_slice()).collect();
    let spec = check::resolve(repo, opts.check.as_deref(), &carved)?;
    let mut checker = Checker::new(repo, spec, &carved)?;
    checker.no_new_errors = opts.no_new_errors;
    let (plan, verification) =
        verify::verify(repo, &snap, &changes, &analysis, &plan, &mut checker)?;
    Ok(Prepared {
        snap,
        changes,
        analysis,
        plan,
        verification: Some(verification),
        checker: Some(checker),
    })
}

/// A full run: the plan buffer, then commits.
pub fn run(repo: &Repo, opts: &Options) -> Result<Summary> {
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
    let (messages, notes) = plan::restore(&saved, &prepared.changes, &prepared.plan.groups);
    let diff_lines = repo.config_u64("carve.planDiff", plan::DIFF_TOTAL_DEFAULT as u64)? as usize;
    let mut text = plan::render(
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
    );
    let verified_structure = (prepared.plan.groups.clone(), prepared.plan.leave.clone());

    let buffer = repo.carve_dir.join("PLAN");
    let mut attempt = 0;
    let edited = loop {
        attempt += 1;
        text = match &opts.plan_file {
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
                        if opts.plan_file.is_some() {
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
        if opts.plan_file.is_some() {
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
    let record = match commit::commit(
        repo,
        &prepared.snap,
        &prepared.changes,
        &groups,
        &Hooks::default(),
    ) {
        Ok(record) => record,
        Err(err) => return Err(keep_messages(repo, &text, &prepared.changes, err)),
    };
    let _ = fs::remove_file(&saved_path);
    let _ = fs::remove_file(&buffer);
    Ok(Summary {
        record,
        subjects: edited
            .groups
            .iter()
            .map(|g| g.message.lines().next().unwrap_or("").to_owned())
            .collect(),
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
