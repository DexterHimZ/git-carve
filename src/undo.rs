//! `git carve --undo` (SPEC §10.5): one level, exact, never touches the working tree.

use std::fs;

use crate::commit::{UndoRecord, undo_path};
use crate::error::{Error, Result, io, precondition};
use crate::repo::Repo;

pub fn undo(repo: &Repo) -> Result<UndoRecord> {
    let path = undo_path(repo);
    let json = match fs::read(&path) {
        Ok(json) => json,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(precondition("nothing to undo", None));
        }
        Err(err) => return Err(io(format!("read {}", path.display()))(err)),
    };
    let record: UndoRecord = serde_json::from_slice(&json)
        .map_err(|e| Error::Internal(format!("unreadable {}: {e}", path.display())))?;

    let manual = format!("to undo by hand: git reset {}", record.old);
    let branch = repo.git.try_run_str(["symbolic-ref", "-q", "HEAD"])?;
    let on_same_ref = match (&branch, record.detached) {
        (Some(b), false) => *b == record.ref_name,
        (None, true) => true,
        _ => false,
    };
    if !on_same_ref {
        return Err(precondition(
            format!(
                "HEAD is no longer on {}, where carve committed",
                record.ref_name
            ),
            Some(&manual),
        ));
    }
    let tip = repo
        .git
        .run_str(["rev-parse", "--verify", "HEAD^{commit}"])?;
    if tip != record.new {
        return Err(precondition(
            "HEAD moved since the carve (new commits, amend, or reset)",
            Some(&manual),
        ));
    }
    // read-tree below replaces the index; staged work would be lost from it.
    if !repo.git.test(["diff", "--cached", "--quiet"])? {
        return Err(precondition(
            "you have staged changes; undo would discard them from the index",
            Some("`git reset` unstages them and keeps your working tree, then undo again"),
        ));
    }

    let mut args = vec!["update-ref"];
    if record.detached {
        args.push("--no-deref");
    }
    args.extend([
        "-m",
        "carve: undo",
        &record.ref_name,
        &record.old,
        &record.new,
    ]);
    if repo.git.run(args).is_err() {
        return Err(Error::Concurrent(format!(
            "{} moved during undo; nothing was changed",
            record.ref_name
        )));
    }
    repo.git.run(["read-tree", &record.old])?;
    repo.git.run(["update-index", "-q", "--refresh"])?;
    // One level only.
    fs::rename(&path, path.with_extension("json.done"))
        .map_err(io(format!("retire {}", path.display())))?;
    Ok(record)
}
