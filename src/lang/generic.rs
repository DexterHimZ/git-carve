//! The generic adapter (DECISIONS.md D45): grammar-free facts for languages without a
//! tree-sitter adapter (Python, Go, Rust, Java, Kotlin, C, C++, C#, Ruby, PHP, Swift, Scala,
//! Elixir, Lua, Dart). Definitions come from keyword patterns, their extents from indentation,
//! references from identifiers, imports from the common statement forms. It is rougher than a
//! parser and far better than nothing: the code graph can link a new function to its uses
//! across files, name a hunk's enclosing function, and keep a drive-by fix apart.

use std::ops::Range;

use super::{
    DebugKind, DebugStmt, Def, DefKind, DefaultCheck, FileFacts, Import, Imported, LanguageAdapter,
    Ref,
};

pub struct Generic;

const EXTENSIONS: &[&str] = &[
    "py", "go", "rs", "java", "kt", "kts", "scala", "rb", "php", "cs", "swift", "c", "h", "cc",
    "cpp", "cxx", "hpp", "hh", "m", "mm", "lua", "ex", "exs", "dart",
];

/// Words that introduce a definition, and what they define.
const DEF_KEYWORDS: &[(&str, DefKind)] = &[
    ("def", DefKind::Function),
    ("defp", DefKind::Function),
    ("defmacro", DefKind::Function),
    ("fn", DefKind::Function),
    ("func", DefKind::Function),
    ("fun", DefKind::Function),
    ("function", DefKind::Function),
    ("class", DefKind::Class),
    ("struct", DefKind::Class),
    ("record", DefKind::Class),
    ("object", DefKind::Class),
    ("impl", DefKind::Class),
    ("extension", DefKind::Class),
    ("interface", DefKind::Interface),
    ("trait", DefKind::Interface),
    ("protocol", DefKind::Interface),
    ("enum", DefKind::Enum),
    ("type", DefKind::Type),
    ("module", DefKind::Namespace),
    ("mod", DefKind::Namespace),
    ("defmodule", DefKind::Namespace),
    ("macro_rules!", DefKind::Function),
];

/// Words that may precede a definition keyword.
const MODIFIERS: &[&str] = &[
    "pub",
    "pub(crate)",
    "pub(super)",
    "pub(self)",
    "public",
    "private",
    "protected",
    "internal",
    "static",
    "final",
    "abstract",
    "async",
    "export",
    "default",
    "override",
    "open",
    "inline",
    "virtual",
    "extern",
    "unsafe",
    "sealed",
    "data",
    "suspend",
    "operator",
    "infix",
    "tailrec",
    "synchronized",
    "native",
    "partial",
    "readonly",
    "local",
    "const",
    "mutating",
    "fileprivate",
    "required",
    "convenience",
    "lazy",
    "case",
    "abstract",
    "annotation",
    "inner",
    "value",
];

/// C-family statements that look like `word name(` but define nothing.
const NOT_FUNCTIONS: &[&str] = &[
    "if", "for", "while", "switch", "catch", "return", "else", "do", "try", "new", "throw",
    "sizeof", "typeof", "using", "lock", "foreach", "when", "match", "elif", "until", "unless",
];

const KEYWORDS: &[&str] = &[
    "if",
    "else",
    "elif",
    "for",
    "while",
    "do",
    "return",
    "break",
    "continue",
    "def",
    "class",
    "import",
    "from",
    "as",
    "in",
    "is",
    "not",
    "and",
    "or",
    "None",
    "True",
    "False",
    "null",
    "nil",
    "true",
    "false",
    "self",
    "this",
    "super",
    "fn",
    "let",
    "mut",
    "pub",
    "use",
    "mod",
    "impl",
    "struct",
    "enum",
    "trait",
    "type",
    "where",
    "match",
    "func",
    "go",
    "defer",
    "var",
    "const",
    "package",
    "interface",
    "map",
    "chan",
    "select",
    "case",
    "default",
    "switch",
    "try",
    "catch",
    "finally",
    "throw",
    "throws",
    "new",
    "delete",
    "public",
    "private",
    "protected",
    "static",
    "final",
    "abstract",
    "void",
    "int",
    "long",
    "float",
    "double",
    "char",
    "bool",
    "boolean",
    "string",
    "str",
    "byte",
    "short",
    "unsigned",
    "signed",
    "auto",
    "async",
    "await",
    "yield",
    "lambda",
    "with",
    "pass",
    "raise",
    "except",
    "global",
    "nonlocal",
    "assert",
    "end",
    "begin",
    "then",
    "unless",
    "until",
    "module",
    "require",
    "include",
    "extends",
    "implements",
    "instanceof",
    "typeof",
    "sizeof",
    "namespace",
    "using",
    "template",
    "typename",
    "virtual",
    "override",
    "operator",
    "inline",
    "extern",
    "goto",
    "volatile",
    "val",
    "object",
    "companion",
    "data",
    "sealed",
    "open",
    "internal",
    "suspend",
    "fun",
    "when",
    "it",
    "elsif",
    "local",
    "function",
    "then",
    "defp",
    "defmodule",
    "Self",
    "crate",
    "dyn",
    "ref",
    "loop",
    "unsafe",
    "usize",
    "isize",
    "u8",
    "u16",
    "u32",
    "u64",
    "i8",
    "i16",
    "i32",
    "i64",
    "f32",
    "f64",
];

/// Names too common to link anything on their own.
const STOPLIST: &[&str] = &[
    "self",
    "this",
    "new",
    "init",
    "main",
    "get",
    "set",
    "run",
    "data",
    "value",
    "values",
    "result",
    "args",
    "kwargs",
    "err",
    "error",
    "ctx",
    "context",
    "config",
    "name",
    "type",
    "id",
    "key",
    "item",
    "items",
    "list",
    "len",
    "str",
    "int",
    "test",
    "setup",
    "path",
    "file",
    "line",
    "text",
    "index",
    "count",
    "size",
    "start",
    "end",
    "node",
    "state",
    "options",
    "opts",
    "params",
    "request",
    "response",
    "res",
    "req",
    "client",
    "server",
    "app",
    "log",
    "logger",
    "info",
    "debug",
    "util",
    "utils",
    "helper",
    "helpers",
    "handler",
    "callback",
    "obj",
    "val",
    "tmp",
    "temp",
    "out",
    "input",
    "output",
    "buf",
    "fmt",
    "println",
    "print",
    "string",
    "format",
    "append",
    "push",
    "pop",
    "update",
    "create",
    "delete",
    "load",
    "save",
    "read",
    "write",
    "open",
    "close",
    "call",
    "apply",
    "parse",
    "build",
    "make",
    "add",
    "remove",
    "Ok",
    "Err",
    "Some",
    "Option",
    "Result",
    "Vec",
    "String",
    "Box",
    "nil",
    "len",
    "cap",
    "unwrap",
    "expect",
    "clone",
    "into",
    "from",
    "iter",
    "map",
    "filter",
    "collect",
    "to_string",
];

/// (manifest, lockfile) pairs that must commit together (SPEC §6.3 B2).
pub const LOCKS: &[(&str, &str)] = &[
    ("Cargo.toml", "Cargo.lock"),
    ("go.mod", "go.sum"),
    ("pyproject.toml", "poetry.lock"),
    ("pyproject.toml", "uv.lock"),
    ("pyproject.toml", "pdm.lock"),
    ("Pipfile", "Pipfile.lock"),
    ("Gemfile", "Gemfile.lock"),
    ("composer.json", "composer.lock"),
    ("mix.exs", "mix.lock"),
    ("Package.swift", "Package.resolved"),
    ("pubspec.yaml", "pubspec.lock"),
];

fn extension(path: &[u8]) -> &[u8] {
    let name = path.rsplit(|&b| b == b'/').next().unwrap_or(path);
    match name.iter().rposition(|&b| b == b'.') {
        Some(i) if i > 0 => &name[i + 1..],
        _ => b"",
    }
}

fn comment_markers(ext: &[u8]) -> &'static [&'static str] {
    match ext {
        b"py" | b"rb" | b"ex" | b"exs" => &["#"],
        b"lua" => &["--"],
        b"php" => &["//", "#"],
        _ => &["//"],
    }
}

/// The code part of a line: comments and string contents blanked out (approximately).
fn code(line: &str, markers: &[&str]) -> String {
    let mut out = String::with_capacity(line.len());
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut rest = line;
    while let Some(c) = rest.chars().next() {
        if quote.is_none() && markers.iter().any(|m| rest.starts_with(m)) {
            break;
        }
        match quote {
            Some(q) => {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == q {
                    quote = None;
                    out.push(c);
                    rest = &rest[c.len_utf8()..];
                    continue;
                }
                out.push(' ');
            }
            None => {
                if c == '"' || c == '\'' || c == '`' {
                    quote = Some(c);
                }
                out.push(c);
            }
        }
        rest = &rest[c.len_utf8()..];
    }
    out
}

fn indent(line: &str) -> usize {
    line.chars()
        .take_while(|c| c.is_whitespace())
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum()
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// The identifier at the start of `s`, if any.
fn leading_ident(s: &str) -> Option<&str> {
    let s = s.trim_start_matches(['&', '*']);
    let end = s.find(|c: char| !is_ident(c)).unwrap_or(s.len());
    (end > 0 && s.starts_with(is_ident_start)).then(|| &s[..end])
}

/// Identifiers in `code`, with the identifier before a `.` as their qualifier.
fn identifiers(code: &str) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    let bytes: Vec<char> = code.chars().collect();
    let mut i = 0;
    let mut prev: Option<String> = None;
    while i < bytes.len() {
        if is_ident_start(bytes[i]) && (i == 0 || !is_ident(bytes[i - 1])) {
            let start = i;
            while i < bytes.len() && is_ident(bytes[i]) {
                i += 1;
            }
            let word: String = bytes[start..i].iter().collect();
            let qualifier = (start > 0 && bytes[start - 1] == '.')
                .then(|| prev.clone())
                .flatten();
            prev = Some(word.clone());
            out.push((word, qualifier));
        } else {
            i += 1;
        }
    }
    out
}

/// A definition on this line: its kind and name.
fn definition(code: &str, ext: &[u8]) -> Option<(DefKind, String)> {
    let trimmed = code.trim();
    let mut words = trimmed.split_whitespace().peekable();
    while let Some(&w) = words.peek() {
        if MODIFIERS.contains(&w) || w.starts_with("pub(") || w == "extern" || w.starts_with('"') {
            words.next();
        } else {
            break;
        }
    }
    let first = words.next()?;
    // `local function name`, `async def name`
    if let Some((_, kind)) = DEF_KEYWORDS.iter().find(|(k, _)| *k == first) {
        let rest: String = words.collect::<Vec<_>>().join(" ");
        let rest = rest.trim_start();
        // Go method: `func (r *Recv) Name(`
        let rest = if first == "func" && rest.starts_with('(') {
            rest.split_once(')').map_or("", |(_, r)| r.trim_start())
        } else {
            rest
        };
        // Rust `impl Trait for Type` names the type; `impl Type` names the type.
        let rest = if first == "impl" {
            let r = rest.trim_start_matches(|c: char| c == '<' || c.is_whitespace());
            let r = r.split_once('>').map_or(r, |(generics, after)| {
                if generics.contains(' ') || generics.contains(':') {
                    after.trim_start()
                } else {
                    r
                }
            });
            r.split_once(" for ").map_or(r, |(_, t)| t)
        } else {
            rest
        };
        // Lua `function M.name(` / Ruby `def self.name`: the last segment of the leading
        // qualified name (a trailing `:`, as in Python's `class Name:`, is not a segment).
        let qualified_end = rest
            .find(|c: char| !(is_ident(c) || c == '.' || c == ':'))
            .unwrap_or(rest.len());
        let rest = rest[..qualified_end]
            .split(['.', ':'])
            .rfind(|seg| !seg.is_empty())
            .unwrap_or("");
        let name = leading_ident(rest)?;
        return (!KEYWORDS.contains(&name)).then(|| (*kind, name.to_owned()));
    }
    // C-family functions: `[modifiers] Type name(params) {` or ending in `)` (brace next line).
    if matches!(
        ext,
        b"java"
            | b"cs"
            | b"c"
            | b"h"
            | b"cc"
            | b"cpp"
            | b"cxx"
            | b"hpp"
            | b"hh"
            | b"m"
            | b"mm"
            | b"dart"
    ) {
        let open = trimmed.find('(')?;
        let before = trimmed[..open].trim_end();
        let name_start = before.rfind(|c: char| !is_ident(c)).map_or(0, |i| i + 1);
        let name = &before[name_start..];
        let head = before[..name_start].trim();
        let ends = trimmed.ends_with('{') || trimmed.ends_with(')');
        if !name.is_empty()
            && name.starts_with(is_ident_start)
            && !head.is_empty()
            && ends
            && !trimmed.ends_with(';')
            && !NOT_FUNCTIONS.contains(&name)
            && !NOT_FUNCTIONS
                .iter()
                .any(|w| head.split_whitespace().any(|h| h == *w))
            && !head.contains('=')
            && !head.contains('.')
            && !KEYWORDS.contains(&name)
        {
            return Some((DefKind::Function, name.to_owned()));
        }
    }
    // Python and Ruby module-level constants `NAME = ...` are left to references.
    None
}

/// The import statement starting at `line`, as (bound names, module, last line).
fn import(lines: &[String], i: usize, ext: &[u8]) -> Option<(Vec<String>, String, usize)> {
    let t = lines[i].trim();
    let until = |closer: char| -> usize {
        (i..lines.len())
            .find(|&k| lines[k].contains(closer))
            .unwrap_or(i)
    };
    match ext {
        b"py" => {
            if let Some(rest) = t.strip_prefix("from ") {
                let (module, names) = rest.split_once(" import ")?;
                let last = if names.contains('(') { until(')') } else { i };
                let joined: String = lines[i..=last].join(" ");
                let names = joined.split_once(" import ")?.1;
                let bound = names
                    .split(',')
                    .filter_map(|n| {
                        let n = n.trim().trim_matches(['(', ')', ' ']);
                        let local = n.rsplit_once(" as ").map_or(n, |(_, a)| a).trim();
                        leading_ident(local).map(str::to_owned)
                    })
                    .collect();
                return Some((bound, module.trim().to_owned(), last));
            }
            let rest = t.strip_prefix("import ")?;
            let bound = rest
                .split(',')
                .filter_map(|n| {
                    let n = n.trim();
                    let local = n
                        .rsplit_once(" as ")
                        .map_or(n.split('.').next()?, |(_, a)| a);
                    leading_ident(local.trim()).map(str::to_owned)
                })
                .collect();
            Some((bound, rest.trim().to_owned(), i))
        }
        b"rs" => {
            let rest = t.strip_prefix("pub ").unwrap_or(t);
            let rest = rest.strip_prefix("use ")?;
            let last = until(';');
            let joined: String = lines[i..=last].join(" ");
            let body = joined.split_once("use ")?.1.trim_end_matches(';');
            let names = body
                .split(['{', '}', ','])
                .filter_map(|p| {
                    let p = p.trim();
                    let p = p.rsplit_once(" as ").map_or(p, |(_, a)| a);
                    let last = p.rsplit("::").next()?;
                    leading_ident(last)
                        .filter(|n| *n != "self")
                        .map(str::to_owned)
                })
                .collect();
            Some((names, rest.trim_end_matches(';').to_owned(), last))
        }
        b"go" => {
            let rest = t.strip_prefix("import")?;
            let rest = rest.trim();
            let last = if rest.starts_with('(') { until(')') } else { i };
            let joined: String = lines[i..=last].join("\n");
            let names = joined
                .lines()
                .filter_map(|l| {
                    let l = l
                        .trim()
                        .trim_start_matches("import")
                        .trim()
                        .trim_matches(['(', ')']);
                    let (alias, path) = match l.split_once('"') {
                        Some((alias, p)) => (alias.trim(), p.trim_end_matches('"')),
                        None => return None,
                    };
                    let local = if alias.is_empty() {
                        path.rsplit('/').next().unwrap_or(path)
                    } else {
                        alias
                    };
                    leading_ident(local).map(str::to_owned)
                })
                .collect();
            Some((names, rest.to_owned(), last))
        }
        b"java" | b"kt" | b"kts" | b"scala" | b"php" | b"cs" | b"swift" => {
            let rest = t
                .strip_prefix("import ")
                .or_else(|| t.strip_prefix("use "))
                .or_else(|| t.strip_prefix("using "))?;
            let rest = rest.trim().trim_end_matches(';');
            let last = rest.rsplit(['.', '\\']).next().unwrap_or(rest);
            Some((
                leading_ident(last).map(str::to_owned).into_iter().collect(),
                rest.to_owned(),
                i,
            ))
        }
        b"c" | b"h" | b"cc" | b"cpp" | b"cxx" | b"hpp" | b"hh" | b"m" | b"mm" => {
            let rest = t.strip_prefix("#include")?;
            Some((Vec::new(), rest.trim().to_owned(), i))
        }
        _ => None,
    }
}

/// Unambiguous debug statements: `breakpoint()`, `pdb.set_trace()`, `binding.pry`, `dbg!(`.
fn debug_statement(code: &str, ext: &[u8]) -> bool {
    let t = code.trim();
    match ext {
        b"py" => {
            t == "breakpoint()"
                || t.ends_with("pdb.set_trace()")
                || t == "import pdb; pdb.set_trace()"
                || t == "import ipdb; ipdb.set_trace()"
        }
        b"rb" => matches!(t, "binding.pry" | "byebug" | "binding.irb" | "debugger"),
        b"rs" => t.starts_with("dbg!(") && t.ends_with(");"),
        _ => false,
    }
}

impl LanguageAdapter for Generic {
    fn matches(&self, path: &[u8]) -> bool {
        let ext = extension(path);
        EXTENSIONS.iter().any(|e| e.as_bytes() == ext)
    }

    fn analyze(&self, path: &[u8], src: &[u8]) -> Option<FileFacts> {
        let text = std::str::from_utf8(src).ok()?;
        let ext = extension(path);
        let markers = comment_markers(ext);
        let raw: Vec<&str> = text.lines().collect();
        let lines: Vec<String> = raw.iter().map(|l| code(l, markers)).collect();
        let raw_lines: Vec<String> = raw.iter().map(|l| (*l).to_owned()).collect();
        let mut facts = FileFacts::default();

        // Definitions, with extents from indentation.
        let mut found: Vec<(usize, DefKind, String)> = Vec::new();
        for (i, l) in lines.iter().enumerate() {
            if let Some((kind, name)) = definition(l, ext) {
                found.push((i, kind, name));
            }
        }
        let blank = |k: usize| lines[k].trim().is_empty();
        let closer = |k: usize| {
            let t = lines[k].trim();
            t.starts_with('}')
                || t.starts_with(')')
                || t.starts_with(']')
                || t == "end"
                || t.starts_with("end ")
                || t.starts_with("end)")
        };
        for (i, kind, name) in &found {
            let d = indent(raw[*i]);
            let mut end = *i + 1;
            let mut k = *i + 1;
            while k < lines.len() {
                if blank(k) {
                    k += 1;
                    continue;
                }
                let ik = indent(raw[k]);
                if ik > d {
                    end = k + 1;
                } else if ik == d && closer(k) {
                    end = k + 1;
                    break;
                } else {
                    break;
                }
                k += 1;
            }
            // Decorators, annotations, and attributes directly above belong to the definition.
            let mut start = *i;
            while start > 0 {
                let above = lines[start - 1].trim();
                if indent(raw[start - 1]) == d
                    && (above.starts_with('@')
                        || above.starts_with("#[")
                        || above.starts_with("[["))
                {
                    start -= 1;
                } else {
                    break;
                }
            }
            facts.defs.push(Def {
                name: name.clone(),
                kind: *kind,
                name_line: *i as u32,
                lines: start as u32..end as u32,
                top_level: true,
                parent: None,
            });
        }
        // Parents: the innermost earlier definition whose extent contains this one.
        for j in 0..facts.defs.len() {
            let line = facts.defs[j].name_line;
            let indent_j = indent(raw[line as usize]);
            let parent = (0..j).rev().find(|&p| {
                let pd = &facts.defs[p];
                pd.lines.start <= line
                    && line < pd.lines.end
                    && indent(raw[pd.name_line as usize]) < indent_j
            });
            if let Some(p) = parent {
                facts.defs[j].parent = Some(p);
                facts.defs[j].top_level = false;
                if facts.defs[j].kind == DefKind::Function
                    && matches!(facts.defs[p].kind, DefKind::Class | DefKind::Interface)
                {
                    facts.defs[j].kind = DefKind::Method;
                }
            }
        }

        // Imports.
        let mut i = 0;
        while i < lines.len() {
            if let Some((names, module, last)) = import(&raw_lines, i, ext) {
                let range: Range<u32> = i as u32..last as u32 + 1;
                facts.import_lines.push(range.clone());
                for local in names {
                    facts.imports.push(Import {
                        imported: Imported::Named(local.clone()),
                        local,
                        source: module.clone(),
                        lines: range.clone(),
                    });
                }
                i = last + 1;
            } else {
                i += 1;
            }
        }

        // References: every identifier outside definitions' own names and imports.
        let import_line = |k: u32| facts.import_lines.iter().any(|r| r.contains(&k));
        for (k, l) in lines.iter().enumerate() {
            let k32 = k as u32;
            if import_line(k32) {
                continue;
            }
            let own: Option<&str> = facts
                .defs
                .iter()
                .find(|d| d.name_line == k32)
                .map(|d| d.name.as_str());
            let mut skipped_own = false;
            for (name, qualifier) in identifiers(l) {
                if KEYWORDS.contains(&name.as_str()) || name.len() < 2 {
                    continue;
                }
                if !skipped_own && own == Some(name.as_str()) {
                    skipped_own = true;
                    continue;
                }
                facts.refs.push(Ref {
                    name,
                    line: k32,
                    qualifier,
                });
            }
            if debug_statement(l, ext) {
                facts.debug.push(DebugStmt {
                    lines: k32..k32 + 1,
                    kind: DebugKind::Debugger,
                    error_path: false,
                });
            }
        }
        Some(facts)
    }

    fn whitespace_significant(&self) -> bool {
        // Python is the common case among these languages; whitespace edits there can change
        // meaning, so they are never treated as style-only.
        true
    }

    fn manifest_lock_pairs(&self) -> &'static [(&'static str, &'static str)] {
        LOCKS
    }

    fn stoplist(&self) -> &'static [&'static str] {
        STOPLIST
    }

    fn corrupt(&self, src: &[u8]) -> Vec<u8> {
        [src, b"\n)(\n"].concat()
    }

    fn default_check(
        &self,
        root: &std::path::Path,
        _configs: &[String],
        changed: &[String],
    ) -> Result<DefaultCheck, String> {
        ecosystem_check(root, changed).ok_or_else(|| {
            "no default check for this repository (no tsconfig.json, Cargo.toml, go.mod, or \
             Python project file at the top level)"
                .to_owned()
        })
    }

    fn name_only_resolution(&self) -> bool {
        true
    }
}

/// The default check for the ecosystem at `root`, and the file extensions it reads (the probe
/// targets only those, DECISIONS.md D1 and D45).
pub fn ecosystem_check(root: &std::path::Path, changed: &[String]) -> Option<DefaultCheck> {
    let has = |name: &str| root.join(name).is_file();
    let python_changed = changed.iter().any(|p| p.ends_with(".py"));
    if has("Cargo.toml") {
        return Some(DefaultCheck {
            command: "cargo check --all-targets --quiet".to_owned(),
            display: "cargo check --all-targets".to_owned(),
            links: Vec::new(),
            reads: vec!["rs".to_owned()],
        });
    }
    if has("go.mod") {
        return Some(DefaultCheck {
            command: "go build ./... && go vet ./...".to_owned(),
            display: "go build ./... && go vet ./...".to_owned(),
            links: Vec::new(),
            reads: vec!["go".to_owned()],
        });
    }
    let python_project = [
        "pyproject.toml",
        "setup.py",
        "setup.cfg",
        "requirements.txt",
    ]
    .iter()
    .any(|f| has(f));
    if python_project || python_changed {
        // Syntax only; a project with a type checker should say so with carve.check. `-f`
        // because compileall otherwise trusts a cached .pyc whose mtime second matches, and
        // carve rewrites files within the same second (found by the D1 probe).
        return Some(DefaultCheck {
            command: "python3 -m compileall -q -f -x '(^|/)(\\.git|\\.venv|venv|node_modules|build|dist)(/|$)' ."
                .to_owned(),
            display: "python3 -m compileall (syntax only)".to_owned(),
            links: Vec::new(),
            reads: vec!["py".to_owned()],
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(path: &str, src: &str) -> FileFacts {
        Generic
            .analyze(path.as_bytes(), src.as_bytes())
            .expect("utf-8")
    }

    fn names(f: &FileFacts) -> Vec<(&str, DefKind, bool)> {
        f.defs
            .iter()
            .map(|d| (d.name.as_str(), d.kind, d.top_level))
            .collect()
    }

    #[test]
    fn python_definitions_extents_and_imports() {
        let f = facts(
            "app/core.py",
            "import json\nfrom app.util import helper, other as alias\n\n\nclass Store:\n    def load(self, path):\n        return json.load(path)\n\n\n@cache\ndef load_json(path):\n    # a comment mentioning ignored_name\n    return helper(path)\n",
        );
        assert_eq!(
            names(&f),
            vec![
                ("Store", DefKind::Class, true),
                ("load", DefKind::Method, false),
                ("load_json", DefKind::Function, true)
            ]
        );
        assert_eq!(f.defs[0].lines, 4..7);
        assert_eq!(
            f.defs[2].lines,
            9..13,
            "the decorator belongs to the definition"
        );
        let locals: Vec<&str> = f.imports.iter().map(|i| i.local.as_str()).collect();
        assert_eq!(locals, vec!["json", "helper", "alias"]);
        assert!(f.refs.iter().any(|r| r.name == "helper" && r.line == 12));
        assert!(
            !f.refs.iter().any(|r| r.name == "ignored_name"),
            "comments are not code"
        );
    }

    #[test]
    fn go_rust_and_c_family_definitions() {
        let go = facts(
            "main.go",
            "package main\n\nimport (\n\t\"fmt\"\n\tstr \"strings\"\n)\n\ntype Server struct {\n\tport int\n}\n\nfunc (s *Server) Start() error {\n\treturn nil\n}\n\nfunc main() {\n\tfmt.Println(str.ToUpper(\"x\"))\n}\n",
        );
        assert_eq!(
            names(&go),
            vec![
                ("Server", DefKind::Type, true),
                ("Start", DefKind::Function, true),
                ("main", DefKind::Function, true)
            ]
        );
        let imports: Vec<&str> = go.imports.iter().map(|i| i.local.as_str()).collect();
        assert_eq!(imports, vec!["fmt", "str"]);

        let rs = facts(
            "src/lib.rs",
            "use crate::model::{Entry, Mode};\n\npub struct Tree;\n\nimpl Display for Tree {\n    fn fmt(&self) {}\n}\n\npub(crate) async fn build() {}\n",
        );
        assert_eq!(
            names(&rs),
            vec![
                ("Tree", DefKind::Class, true),
                ("Tree", DefKind::Class, true),
                ("fmt", DefKind::Method, false),
                ("build", DefKind::Function, true)
            ]
        );
        let java = facts(
            "A.java",
            "import java.util.List;\n\npublic class A {\n    public static int add(int a, int b) {\n        if (a > 0) {\n            return a + b;\n        }\n        return b;\n    }\n}\n",
        );
        assert_eq!(
            names(&java),
            vec![("A", DefKind::Class, true), ("add", DefKind::Method, false)]
        );
    }

    #[test]
    fn debug_statements_and_matching() {
        let f = facts("x.py", "def f():\n    breakpoint()\n    return 1\n");
        assert_eq!(f.debug.len(), 1);
        assert!(Generic.matches(b"pkg/a.go") && Generic.matches(b"lib.rs"));
        assert!(
            !Generic.matches(b"a.ts")
                && !Generic.matches(b"README.md")
                && !Generic.matches(b"Makefile")
        );
    }
}
