//! Language adapters (SPEC §6.1). v1 has exactly one: TypeScript.

pub mod resolve;
pub mod typescript;

use std::collections::BTreeSet;
use std::ops::Range;

/// What one version of one file declares, imports, and references. Lines are 0-based,
/// ranges half-open.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileFacts {
    pub defs: Vec<Def>,
    pub imports: Vec<Import>,
    pub refs: Vec<Ref>,
    /// Statements that are debug output (§6.5, DECISIONS.md D8).
    pub debug: Vec<DebugStmt>,
    /// `console.<method>` calls anywhere in the file.
    pub console_methods: BTreeSet<String>,
    /// Import statements and re-exports (`export ... from`), by line range.
    pub import_lines: Vec<Range<u32>>,
    /// Names exported through `export default <name>` or `export default function <name>`.
    pub default_export: Option<String>,
    pub has_error: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Def {
    pub name: String,
    pub kind: DefKind,
    /// The line of the name itself.
    pub name_line: u32,
    /// The whole declaration.
    pub lines: Range<u32>,
    /// Declared at module level (not inside a function, class, or other definition).
    pub top_level: bool,
    /// Index of the enclosing definition in `defs`.
    pub parent: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefKind {
    Function,
    Class,
    Method,
    Field,
    Interface,
    Type,
    Enum,
    Variable,
    Namespace,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Import {
    /// The name bound in this file.
    pub local: String,
    pub imported: Imported,
    /// The module specifier, without quotes.
    pub source: String,
    /// The whole import statement.
    pub lines: Range<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Imported {
    Named(String),
    Default,
    Namespace,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ref {
    pub name: String,
    pub line: u32,
    /// For `ns.name`, the object identifier `ns` (resolves namespace imports).
    pub qualifier: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DebugStmt {
    pub lines: Range<u32>,
    pub kind: DebugKind,
    /// Error reporting, not leftover debugging: inside a `catch` clause, or right before a
    /// `throw` (DECISIONS.md D42). Never left out.
    pub error_path: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DebugKind {
    Debugger,
    Console(String),
}

pub trait LanguageAdapter {
    fn matches(&self, path: &[u8]) -> bool;
    /// `None` when the file cannot be parsed at all (no symbol facts, §6.1).
    fn analyze(&self, path: &[u8], src: &[u8]) -> Option<FileFacts>;
    /// Whitespace changes can change meaning (Python, YAML, Makefiles).
    fn whitespace_significant(&self) -> bool;
    /// (manifest file name, lockfile name) pairs that must commit together (B2).
    fn manifest_lock_pairs(&self) -> &'static [(&'static str, &'static str)];
    /// Names too common to link anything on their own (§6.1).
    fn stoplist(&self) -> &'static [&'static str];
    /// `src` with a guaranteed syntax error appended: the probe (DECISIONS.md D1).
    fn corrupt(&self, src: &[u8]) -> Vec<u8>;
    /// The default check for a repository whose top level is `root`, or why there is none.
    /// `configs` lists the tracked project config files (for the hint); `changed` lists the
    /// changed paths, whose owning projects the check must cover (DECISIONS.md D27).
    fn default_check(
        &self,
        root: &std::path::Path,
        configs: &[String],
        changed: &[String],
    ) -> Result<DefaultCheck, String>;
}

/// A check command built by an adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefaultCheck {
    pub command: String,
    /// Short form for summaries.
    pub display: String,
    /// Extra untracked directories the check needs in the scratch worktree, such as a
    /// sub-project's `node_modules`.
    pub links: Vec<String>,
}

/// The adapter for `path`, if any.
pub fn adapter_for(path: &[u8]) -> Option<&'static dyn LanguageAdapter> {
    static TS: typescript::TypeScript = typescript::TypeScript;
    TS.matches(path).then_some(&TS as &dyn LanguageAdapter)
}

/// The v1 adapter, for repository-wide rules such as manifest/lockfile pairs.
pub fn primary() -> &'static dyn LanguageAdapter {
    static TS: typescript::TypeScript = typescript::TypeScript;
    &TS
}
