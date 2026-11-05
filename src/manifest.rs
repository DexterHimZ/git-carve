//! B5 (DECISIONS.md D48): a change that starts using a dependency the same carve adds to a
//! manifest travels with that manifest line, and a change that stops using a dependency the
//! manifest removes travels with the removal. Without it, a stack layer could import a
//! package that only a later layer adds: it passes locally, against the final install, and
//! fails in CI. Sound, not complete: npm (`package.json`), Cargo (`Cargo.toml`) and Go
//! (`go.mod`); Python package and module names differ, so it only gets the existing note.
//! A manifest that is itself new is not read (its text is not diffed); a brand-new project
//! is new files anyway.

use crate::atom::{AtomKind, Changes};
use crate::facts::Analysis;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eco {
    Npm,
    Cargo,
    Go,
}

/// The ecosystem a manifest file name belongs to.
pub fn manifest_eco(path: &[u8]) -> Option<Eco> {
    let name = path.rsplit(|&b| b == b'/').next().unwrap_or(path);
    match name {
        b"package.json" => Some(Eco::Npm),
        b"Cargo.toml" => Some(Eco::Cargo),
        b"go.mod" => Some(Eco::Go),
        _ => None,
    }
}

/// Whether a source file's imports belong to the ecosystem.
fn code_of(eco: Eco, path: &[u8]) -> bool {
    let ext = path.rsplit(|&b| b == b'.').next().unwrap_or(b"");
    match eco {
        Eco::Npm => matches!(
            ext,
            b"ts" | b"tsx" | b"mts" | b"cts" | b"js" | b"jsx" | b"mjs" | b"cjs"
        ),
        Eco::Cargo => ext == b"rs",
        Eco::Go => ext == b"go",
    }
}

/// `(name, 0-based line)` of every dependency the manifest declares.
pub fn deps(eco: Eco, text: &str) -> Vec<(String, u32)> {
    let lines: Vec<&str> = text.lines().collect();
    match eco {
        Eco::Npm => {
            let Ok(doc) = serde_json::from_str::<serde_json::Value>(text) else {
                return Vec::new();
            };
            let mut out = Vec::new();
            for section in [
                "dependencies",
                "devDependencies",
                "peerDependencies",
                "optionalDependencies",
            ] {
                if let Some(map) = doc.get(section).and_then(|v| v.as_object()) {
                    // Look only inside the section's own braces: `"scripts": {"vitest": …}`
                    // above it must not claim the dependency's line.
                    let Some((from, to)) = npm_section(&lines, section) else {
                        continue;
                    };
                    for name in map.keys() {
                        let key = format!("\"{name}\"");
                        if let Some(i) =
                            (from..to).find(|&i| lines[i].trim_start().starts_with(&key))
                        {
                            out.push((name.clone(), i as u32));
                        }
                    }
                }
            }
            out
        }
        Eco::Cargo => {
            let mut out = Vec::new();
            let mut in_deps = false;
            for (i, raw) in lines.iter().enumerate() {
                let line = raw.split('#').next().unwrap_or("").trim();
                if let Some(section) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                    let section = section.trim();
                    in_deps = section.ends_with("dependencies");
                    // `[dependencies.serde]` table form.
                    if let Some((head, name)) = section.rsplit_once('.') {
                        if head.ends_with("dependencies") {
                            out.push((name.trim_matches('"').replace('-', "_"), i as u32));
                        }
                    }
                    continue;
                }
                if in_deps {
                    if let Some((name, _)) = line.split_once('=') {
                        let name = name
                            .trim()
                            .split('.')
                            .next()
                            .unwrap_or("")
                            .trim_matches('"');
                        if !name.is_empty() {
                            out.push((name.replace('-', "_"), i as u32));
                        }
                    }
                }
            }
            out
        }
        Eco::Go => {
            let mut out = Vec::new();
            let mut in_block = false;
            for (i, raw) in lines.iter().enumerate() {
                let line = raw.split("//").next().unwrap_or("").trim();
                let module = if in_block {
                    if line.starts_with(')') {
                        in_block = false;
                        continue;
                    }
                    line.split_whitespace().next()
                } else if let Some(rest) = line.strip_prefix("require") {
                    let rest = rest.trim();
                    if rest.starts_with('(') {
                        in_block = true;
                        continue;
                    }
                    rest.split_whitespace().next()
                } else {
                    None
                };
                if let Some(m) = module.filter(|m| !m.is_empty()) {
                    out.push((m.to_owned(), i as u32));
                }
            }
            out
        }
    }
}

/// The line span `[from, to)` of a top-level `"section": { … }` in a package.json, found by
/// brace depth (strings are skipped, so a `}` inside a value does not close it).
fn npm_section(lines: &[&str], section: &str) -> Option<(usize, usize)> {
    let key = format!("\"{section}\"");
    let start = lines
        .iter()
        .position(|l| l.trim_start().starts_with(&key))?;
    let mut depth = 0i32;
    let mut opened = false;
    for (i, line) in lines.iter().enumerate().skip(start) {
        let mut in_str = false;
        let mut escaped = false;
        for c in line.chars() {
            match c {
                _ if escaped => escaped = false,
                '\\' if in_str => escaped = true,
                '"' => in_str = !in_str,
                '{' if !in_str => {
                    depth += 1;
                    opened = true;
                }
                '}' if !in_str => depth -= 1,
                _ => {}
            }
        }
        if opened && depth <= 0 {
            return Some((start, i + 1));
        }
    }
    None
}

/// Whether one line of source uses `dep`. Comments never count, and in JavaScript only
/// import syntax does (`import`/`export … from`, a `} from` closing a multi-line import,
/// `require(`, `import(`), so `// moved away from "axios"` binds nothing.
pub fn uses(eco: Eco, line: &str, dep: &str) -> bool {
    let code = line.trim_start();
    if code.starts_with("//") || code.starts_with("/*") || code.starts_with('*') {
        return false;
    }
    match eco {
        Eco::Npm => {
            let import_syntax = code.starts_with("import")
                || code.starts_with("export")
                || (code.starts_with('}') && code.contains("from"))
                || code.contains("require(")
                || code.contains("import(");
            if !import_syntax {
                return false;
            }
            ['"', '\'', '`'].iter().any(|q| {
                let open = format!("{q}{dep}");
                line.match_indices(&open).any(|(at, _)| {
                    let rest = &line[at + open.len()..];
                    rest.starts_with(*q) || rest.starts_with('/')
                })
            })
        }
        Eco::Cargo => {
            let path = format!("{dep}::");
            let bounded = |needle: &str| {
                line.match_indices(needle).any(|(at, _)| {
                    !line[..at]
                        .chars()
                        .next_back()
                        .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
                })
            };
            bounded(&path)
                || bounded(&format!("use {dep};"))
                || line.contains(&format!("extern crate {dep}"))
        }
        Eco::Go => line.contains(&format!("\"{dep}\"")) || line.contains(&format!("\"{dep}/")),
    }
}

/// The B5 hard binds, as atom-index pairs with their reason.
pub fn binds(changes: &Changes, analysis: &Analysis) -> Vec<(usize, usize, &'static str)> {
    let mut out = Vec::new();
    for (mf, manifest) in changes.files.iter().enumerate() {
        let (Some(eco), Some(text)) = (manifest_eco(&manifest.path), &manifest.text) else {
            continue;
        };
        let base = deps(eco, &String::from_utf8_lossy(&text.base));
        let new = deps(eco, &String::from_utf8_lossy(&text.new));
        let added: Vec<&(String, u32)> = new
            .iter()
            .filter(|(n, _)| !base.iter().any(|(b, _)| b == n))
            .collect();
        let removed: Vec<&(String, u32)> = base
            .iter()
            .filter(|(n, _)| !new.iter().any(|(b, _)| b == n))
            .collect();
        if added.is_empty() && removed.is_empty() {
            continue;
        }
        // The manifest atom holding a line, on the new side (added) or the base side (removed).
        let holder = |line: u32, new_side: bool| {
            changes.file_atoms[mf]
                .iter()
                .copied()
                .find(|&i| match &changes.atoms[i].kind {
                    AtomKind::Region(r) if new_side => r.new.contains(&line),
                    AtomKind::Region(r) => r.base.contains(&line),
                    _ => true,
                })
        };
        for (cf, code) in changes.files.iter().enumerate() {
            if !code_of(eco, &code.path) {
                continue;
            }
            for &a in &changes.file_atoms[cf] {
                let (new_lines, base_lines): (Vec<String>, Vec<String>) =
                    match (&changes.atoms[a].kind, &code.text) {
                        (AtomKind::Region(r), Some(t)) => (
                            t.new_lines(r.new.clone())
                                .iter()
                                .map(|(_, l)| String::from_utf8_lossy(l).into_owned())
                                .collect(),
                            t.base_lines(r.base.clone())
                                .iter()
                                .map(|(_, l)| String::from_utf8_lossy(l).into_owned())
                                .collect(),
                        ),
                        // A new file: every import it has is new.
                        (AtomKind::Whole(_), None) => (
                            analysis.fin[cf]
                                .iter()
                                .flat_map(|f| &f.imports)
                                .map(|i| format!("import \"{}\"", import_path(eco, &i.source)))
                                .collect(),
                            Vec::new(),
                        ),
                        _ => continue,
                    };
                for (dep, line) in &added {
                    if new_lines.iter().any(|l| uses(eco, l, dep)) {
                        if let Some(m) = holder(*line, true) {
                            out.push((a, m, "uses a dependency this change adds to the manifest"));
                        }
                    }
                }
                for (dep, line) in &removed {
                    if base_lines.iter().any(|l| uses(eco, l, dep)) {
                        if let Some(m) = holder(*line, false) {
                            out.push((
                                a,
                                m,
                                "stops using a dependency this change removes from the manifest",
                            ));
                        }
                    }
                }
            }
        }
    }
    out
}

/// An import's source as a quoted specifier `uses` can match: Rust's `serde::de::X` is the
/// path `serde::de::X` (a `crate::` path never names a dependency).
fn import_path(eco: Eco, source: &str) -> String {
    match eco {
        Eco::Cargo => format!("{source};"),
        _ => source.trim_matches(['"', '\'', '`']).to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npm_deps_and_uses() {
        let pkg = "{\n  \"name\": \"x\",\n  \"dependencies\": {\n    \"axios\": \"^1.0.0\",\n    \"@scope/lib\": \"1\"\n  }\n}\n";
        let d = deps(Eco::Npm, pkg);
        assert_eq!(d, [("@scope/lib".to_owned(), 4), ("axios".to_owned(), 3)]);
        assert!(uses(Eco::Npm, "import axios from \"axios\";", "axios"));
        assert!(uses(
            Eco::Npm,
            "const x = require('axios/lib/core');",
            "axios"
        ));
        assert!(uses(
            Eco::Npm,
            "import { a } from '@scope/lib';",
            "@scope/lib"
        ));
        assert!(!uses(Eco::Npm, "import x from \"axios-retry\";", "axios"));
        assert!(!uses(Eco::Npm, "const s = \"axios\";", "axios"));
        assert!(uses(Eco::Npm, "} from \"axios\";", "axios"));
        assert!(uses(
            Eco::Npm,
            "  const m = await import(\"axios\");",
            "axios"
        ));
        assert!(!uses(
            Eco::Npm,
            "  // migrated away from \"axios\" to fetch",
            "axios"
        ));
        assert!(!uses(Eco::Npm, " * import x from \"axios\";", "axios"));
        assert!(!uses(
            Eco::Npm,
            "  log(\"moved away from \\\"axios\\\"\");",
            "axios"
        ));
    }

    #[test]
    fn npm_dep_line_is_inside_its_section() {
        let pkg = "{\n  \"scripts\": {\n    \"vitest\": \"vitest run\"\n  },\n  \"devDependencies\": {\n    \"vitest\": \"^2.0.0\"\n  }\n}\n";
        assert_eq!(deps(Eco::Npm, pkg), [("vitest".to_owned(), 5)]);
    }

    #[test]
    fn cargo_deps_and_uses() {
        let toml = "[package]\nname = \"x\"\n\n[dependencies]\nserde_json = \"1\"\nfoo-bar = { version = \"2\" }\n\n[dependencies.tokio]\nversion = \"1\"\n";
        let d = deps(Eco::Cargo, toml);
        let names: Vec<&str> = d.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["serde_json", "foo_bar", "tokio"]);
        assert!(uses(Eco::Cargo, "use serde_json::Value;", "serde_json"));
        assert!(uses(
            Eco::Cargo,
            "    let v = serde_json::to_string(&x)?;",
            "serde_json"
        ));
        assert!(!uses(Eco::Cargo, "use my_serde_json::Value;", "serde_json"));
        assert!(!uses(
            Eco::Cargo,
            "use crate::serde_json::Value;",
            "serde_json"
        ));
        assert!(!uses(
            Eco::Cargo,
            "    // serde_json::to_string was slow",
            "serde_json"
        ));
    }

    #[test]
    fn go_deps_and_uses() {
        let gomod = "module example.com/x\n\ngo 1.22\n\nrequire (\n\tgithub.com/spf13/cobra v1.8.0\n\tgolang.org/x/sync v0.7.0 // indirect\n)\nrequire github.com/pkg/errors v0.9.1\n";
        let names: Vec<String> = deps(Eco::Go, gomod).into_iter().map(|(n, _)| n).collect();
        assert_eq!(
            names,
            [
                "github.com/spf13/cobra",
                "golang.org/x/sync",
                "github.com/pkg/errors"
            ]
        );
        assert!(uses(
            Eco::Go,
            "\t\"github.com/spf13/cobra\"",
            "github.com/spf13/cobra"
        ));
        assert!(uses(
            Eco::Go,
            "\t\"golang.org/x/sync/errgroup\"",
            "golang.org/x/sync"
        ));
        assert!(!uses(
            Eco::Go,
            "\t\"github.com/spf13/cobra-cli\"",
            "github.com/spf13/cobra"
        ));
    }
}
