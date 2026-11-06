//! Language adapters (SPEC §6.1). v1 has exactly one: TypeScript.

pub mod generic;
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
    /// Module specifiers of statements that bind no local name: re-exports
    /// (`export * from 'x'`, `export { a } from 'x'`) and side-effect imports (`import 'x'`),
    /// with their lines (D61: such a line belongs with the new module it names).
    pub module_refs: Vec<(String, Range<u32>)>,
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
    /// Whether a reference may resolve by name alone to the one changed file that defines it
    /// at top level (DECISIONS.md D45). Adapters with real module resolution say no.
    fn name_only_resolution(&self) -> bool {
        false
    }
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
    /// File extensions the check reads (DECISIONS.md D45). The probe (D1) targets only these;
    /// other changed source files are reported as unverified.
    pub reads: Vec<String>,
    /// Of `reads`, the ones the check may or may not read (JavaScript under tsc, D54): a probe
    /// on them that the check misses means "unverified", never "this check is vacuous".
    pub soft_reads: Vec<String>,
}

/// The adapter for `path`, if any.
pub fn adapter_for(path: &[u8]) -> Option<&'static dyn LanguageAdapter> {
    static TS: typescript::TypeScript = typescript::TypeScript;
    static GENERIC: generic::Generic = generic::Generic;
    // Dev-only ablation knob for the eval harness (D45): `GIT_CARVE_DEV_NO_GENERIC=1`.
    static NO_GENERIC: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let generic_off =
        *NO_GENERIC.get_or_init(|| std::env::var_os("GIT_CARVE_DEV_NO_GENERIC").is_some());
    if TS.matches(path) {
        Some(&TS)
    } else if !generic_off && GENERIC.matches(path) {
        Some(&GENERIC)
    } else {
        None
    }
}

/// Whether `path` is TypeScript (`.ts`, `.tsx`, `.mts`, `.cts`).
pub fn is_typescript(path: &[u8]) -> bool {
    typescript::TS_EXTENSIONS
        .iter()
        .any(|ext| path.ends_with(ext))
}

/// Whether `path` is JavaScript (`.js`, `.jsx`, `.mjs`, `.cjs`), which the TypeScript adapter
/// also reads (D54).
pub fn is_javascript(path: &[u8]) -> bool {
    typescript::JS_EXTENSIONS
        .iter()
        .any(|ext| path.ends_with(ext))
}

/// Manifest/lockfile pairs of every ecosystem carve knows (SPEC §6.3 B2).
pub fn manifest_lock_pairs() -> Vec<(&'static str, &'static str)> {
    let ts: &'static [(&'static str, &'static str)] = primary().manifest_lock_pairs();
    ts.iter().chain(generic::LOCKS).copied().collect()
}

/// Names too common to link anything on their own, across adapters.
pub fn stoplist() -> BTreeSet<&'static str> {
    static GENERIC: generic::Generic = generic::Generic;
    primary()
        .stoplist()
        .iter()
        .chain(GENERIC.stoplist())
        .copied()
        .collect()
}

/// The extension of a repository path (after the last dot of the file name).
pub fn extension(path: &[u8]) -> String {
    let name = path.rsplit(|&b| b == b'/').next().unwrap_or(path);
    match name.iter().rposition(|&b| b == b'.') {
        Some(i) if i > 0 => String::from_utf8_lossy(&name[i + 1..]).into_owned(),
        _ => String::new(),
    }
}

/// The v1 adapter, for repository-wide rules such as manifest/lockfile pairs.
pub fn primary() -> &'static dyn LanguageAdapter {
    static TS: typescript::TypeScript = typescript::TypeScript;
    &TS
}
