//! Types shared across the pipeline: tree entries and modes.

use std::fmt;

/// A git file mode as stored in trees.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Mode(pub u32);

impl Mode {
    pub const FILE: Mode = Mode(0o100644);
    pub const EXEC: Mode = Mode(0o100755);
    pub const LINK: Mode = Mode(0o120000);

    pub fn parse(octal: &[u8]) -> Option<Mode> {
        std::str::from_utf8(octal)
            .ok()
            .and_then(|s| u32::from_str_radix(s, 8).ok())
            .map(Mode)
    }

    pub fn is_regular(self) -> bool {
        self == Self::FILE || self == Self::EXEC
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:06o}", self.0)
    }
}

/// One tree entry: a mode and an object id.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Entry {
    pub mode: Mode,
    pub oid: String,
}

/// A repository-relative path, for display. Paths are bytes in git and may not be UTF-8.
pub fn show(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}
