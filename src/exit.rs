//! Process exit codes (SPEC §11). Scripts and the eval harness depend on these values.

use std::process::ExitCode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// 0: success.
    Success,
    /// 1: user abort (empty message, empty plan, editor error).
    UserAbort,
    /// 2: a precondition failed (§5.1).
    Precondition,
    /// 3: the check fails on HEAD or on the final tree.
    CheckFailed,
    /// 4: concurrent change (CAS failure or the worktree changed).
    Concurrent,
    /// 5: internal error.
    Internal,
    /// 6: `--stack --submit` committed and created every layer branch, but submitting the
    /// stack failed (DECISIONS.md D48). Retrying the printed commands is safe.
    SubmitFailed,
    /// 129: command-line usage error, as with git's own commands. Kept apart from 2 so a
    /// typo is never mistaken for a refused repository state.
    Usage,
}

impl Exit {
    pub fn code(self) -> u8 {
        match self {
            Self::Success => 0,
            Self::UserAbort => 1,
            Self::Precondition => 2,
            Self::CheckFailed => 3,
            Self::Concurrent => 4,
            Self::Internal => 5,
            Self::SubmitFailed => 6,
            Self::Usage => 129,
        }
    }
}

impl From<Exit> for ExitCode {
    fn from(exit: Exit) -> Self {
        ExitCode::from(exit.code())
    }
}
