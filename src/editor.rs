//! Opening the user's editor on the plan buffer (SPEC §9.1): `git var GIT_EDITOR`, run
//! through the shell with the file as an argument, exactly as git runs it.

use std::path::Path;
use std::process::Command;

use crate::error::{Error, Result, io};
use crate::repo::Repo;

pub fn edit(repo: &Repo, path: &Path) -> Result<()> {
    let editor = repo.git.run_str(["var", "GIT_EDITOR"])?;
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$@\""))
        .arg(&editor)
        .arg(path)
        .current_dir(&repo.toplevel)
        .status()
        .map_err(io(format!("run the editor `{editor}`")))?;
    if !status.success() {
        return Err(Error::Aborted(format!(
            "the editor `{editor}` exited with {status}; nothing was committed"
        )));
    }
    Ok(())
}
