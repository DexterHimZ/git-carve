//! The library's error type. Each variant maps to one exit code (SPEC §11).

use crate::exit::Exit;
use crate::git::GitError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A refused repository state (§5.1). The message already includes any hint.
    #[error("{0}")]
    Precondition(String),
    /// The ref or the working tree changed while carve ran (§10.2, §10.3).
    #[error("{0}")]
    Concurrent(String),
    /// The check fails where it must pass (HEAD, the working tree) or cannot be trusted
    /// (the probe, DECISIONS.md D1). SPEC §8.3, exit 3.
    #[error("{0}")]
    CheckFailed(String),
    /// The user stopped the run (empty plan, empty message, editor failure). Exit 1.
    #[error("{0}")]
    Aborted(String),
    /// The plan handed to carve is not valid (unknown atom, split hard bind, empty message).
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[error("internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn exit(&self) -> Exit {
        match self {
            Self::Precondition(_) => Exit::Precondition,
            Self::Concurrent(_) => Exit::Concurrent,
            Self::CheckFailed(_) => Exit::CheckFailed,
            Self::Invalid(_) | Self::Aborted(_) => Exit::UserAbort,
            Self::Git(_) | Self::Io { .. } | Self::Internal(_) => Exit::Internal,
        }
    }
}

pub fn precondition(reason: impl Into<String>, hint: Option<&str>) -> Error {
    let mut message = reason.into();
    if let Some(hint) = hint {
        message.push_str("\nhint: ");
        message.push_str(hint);
    }
    Error::Precondition(message)
}

/// `map_err` adapter that names what was being done when an I/O call failed.
pub fn io(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> Error {
    let context = context.into();
    move |source| Error::Io { context, source }
}
