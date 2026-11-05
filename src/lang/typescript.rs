//! The TypeScript adapter: one tree-sitter walk per file (DECISIONS.md D7).

use std::ops::Range;

use tree_sitter::{Node, Parser};

use super::{
    DebugKind, DebugStmt, Def, DefKind, DefaultCheck, FileFacts, Import, Imported, LanguageAdapter,
    Ref,
};

pub struct TypeScript;

/// Console methods that count as debug output (D8). `error`, `warn`, and `info` never do.
pub const DEBUG_CONSOLE: &[&str] = &["log", "debug", "trace", "dir", "table"];

const STOPLIST: &[&str] = &[
    // SPEC §6.1
    "new",
    "get",
    "set",
    "init",
    "main",
    "test",
    "self",
    "this",
    "value",
    "data",
    "result",
    "err",
    "ctx",
    // TypeScript and common framework names
    "props",
    "state",
    "error",
    "args",
    "options",
    "index",
    "item",
    "key",
    "name",
    "type",
    "console",
    "window",
    "document",
    "default",
    "string",
    "number",
    "boolean",
    "undefined",
    "Promise",
    "Array",
    "Object",
    "String",
    "Number",
    "Boolean",
    "Error",
    "Date",
    "JSON",
    "Math",
    "React",
    "children",
    "event",
    "response",
    "request",
    "then",
    "length",
    "push",
    "json",
    "async",
    "await",
    "return",
    "export",
    "import",
    "from",
    "true",
    "false",
    "null",
    "void",
    "any",
    "unknown",
    "never",
    "readonly",
    "log",
    "map",
    "filter",
    "reduce",
    "forEach",
    "keys",
    "values",
    "entries",
    "toString",
    "require",
    "module",
    "exports",
];

const LOCKS: &[(&str, &str)] = &[
    ("package.json", "package-lock.json"),
    ("package.json", "npm-shrinkwrap.json"),
    ("package.json", "pnpm-lock.yaml"),
    ("package.json", "yarn.lock"),
    ("package.json", "bun.lock"),
    ("package.json", "bun.lockb"),
];

impl LanguageAdapter for TypeScript {
    fn matches(&self, path: &[u8]) -> bool {
        [&b".ts"[..], b".tsx", b".mts", b".cts"]
            .iter()
            .any(|ext| path.ends_with(ext))
    }

    fn analyze(&self, path: &[u8], src: &[u8]) -> Option<FileFacts> {
        let language = if path.ends_with(b".tsx") {
            tree_sitter_typescript::LANGUAGE_TSX
        } else {
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT
        };
        let mut parser = Parser::new();
        parser.set_language(&language.into()).ok()?;
        let tree = parser.parse(src, None)?;
        let root = tree.root_node();
        let mut walker = Walker {
            src,
            facts: FileFacts {
                has_error: root.has_error(),
                ..FileFacts::default()
            },
            scope: Vec::new(),
        };
        walker.visit(root);
        Some(walker.facts)
    }

    fn whitespace_significant(&self) -> bool {
        false
    }

    fn manifest_lock_pairs(&self) -> &'static [(&'static str, &'static str)] {
        LOCKS
    }

    fn stoplist(&self) -> &'static [&'static str] {
        STOPLIST
    }

    fn corrupt(&self, src: &[u8]) -> Vec<u8> {
        [src, b"\n/* git carve probe: deliberately broken */ )(\n"].concat()
    }

    /// DECISIONS.md D12: each project (the root if it has inputs, plus every reference),
    /// `node_modules/.bin/tsc` when present, incremental build info redirected under
    /// `<git-path carve>` (the check runs in `<git-path carve>/scratch`), never `tsc -b`.
    fn default_check(
        &self,
        root: &std::path::Path,
        configs: &[String],
        changed: &[String],
    ) -> Result<DefaultCheck, String> {
        let root_config = std::fs::read(root.join("tsconfig.json")).ok();
        let mut projects: Vec<String> = Vec::new();
        if let Some(text) = &root_config {
            let config: serde_json::Value =
                serde_json::from_slice(&super::resolve::strip_jsonc(text))
                    .map_err(|e| format!("tsconfig.json is not valid JSON: {e}"))?;
            let has_inputs = match config.get("files").and_then(|f| f.as_array()) {
                Some(files) => !files.is_empty() || config.get("include").is_some(),
                None => true,
            };
            if has_inputs {
                projects.push(".".to_owned());
            }
            for reference in config
                .get("references")
                .and_then(|r| r.as_array())
                .into_iter()
                .flatten()
            {
                if let Some(path) = reference.get("path").and_then(|p| p.as_str()) {
                    projects.push(path.trim_start_matches("./").to_owned());
                }
            }
        }
        // D27: every project that owns a changed file (its nearest tsconfig.json below the
        // root; the root project is covered above).
        for path in changed.iter().filter(|p| self.matches(p.as_bytes())) {
            let mut dir = std::path::Path::new(path).parent();
            while let Some(d) = dir.filter(|d| !d.as_os_str().is_empty()) {
                if root.join(d).join("tsconfig.json").is_file() {
                    let project = d.to_string_lossy().into_owned();
                    if !projects.contains(&project) {
                        projects.push(project);
                    }
                    break;
                }
                dir = d.parent();
            }
        }
        if projects.is_empty() {
            if root_config.is_some() {
                return Err("tsconfig.json has no inputs and no references to check".to_owned());
            }
            let found = if configs.is_empty() {
                "no tsconfig files are tracked".to_owned()
            } else {
                format!("tracked configs: {}", configs.join(", "))
            };
            return Err(format!(
                "no tsconfig.json at the repository root, so there is no default check ({found})\n\
                 hint: git config carve.check \"cd app && npx tsc --noEmit -p .\" (or pass --check)"
            ));
        }
        let own_tsc = |p: &str| {
            let rel = if p == "." {
                "node_modules/.bin/tsc".to_owned()
            } else {
                format!("{p}/node_modules/.bin/tsc")
            };
            root.join(&rel).exists().then_some(rel)
        };
        let tsc = own_tsc(".")
            .or_else(|| projects.iter().find_map(|p| own_tsc(p)))
            .unwrap_or_else(|| "npx --no-install tsc".to_owned());
        let command = projects
            .iter()
            .map(|p| format!("{tsc} --noEmit -p {p} --incremental --tsBuildInfoFile ../tsbuildinfo/{}.tsbuildinfo", build_info_name(p)))
            .collect::<Vec<_>>()
            .join(" && ");
        let display = projects
            .iter()
            .map(|p| format!("tsc --noEmit -p {p}"))
            .collect::<Vec<_>>()
            .join(" && ");
        let links = projects
            .iter()
            .filter(|p| *p != "." && root.join(p).join("node_modules").is_dir())
            .map(|p| format!("{p}/node_modules"))
            .collect();
        Ok(DefaultCheck {
            command,
            display,
            links,
            reads: ["ts", "tsx", "mts", "cts"].map(str::to_owned).to_vec(),
        })
    }
}

struct Walker<'s> {
    src: &'s [u8],
    facts: FileFacts,
    /// Indices of the definitions enclosing the current node.
    scope: Vec<usize>,
}

/// D42: a statement in an error path: inside a `catch` clause (within its function), or
/// directly followed by a `throw`.
fn error_path(stmt: Node) -> bool {
    if stmt
        .next_named_sibling()
        .is_some_and(|n| n.kind() == "throw_statement")
    {
        return true;
    }
    let mut up = stmt.parent();
    while let Some(node) = up {
        match node.kind() {
            "catch_clause" => return true,
            k if def_kind(k).is_some() || k.contains("function") || k == "arrow_function" => {
                return false;
            }
            _ => up = node.parent(),
        }
    }
    false
}

/// Half-open line range of a node. A node ending at column 0 does not occupy that line.
pub fn lines(node: Node) -> Range<u32> {
    let (start, end) = (node.start_position(), node.end_position());
    let last = if end.column == 0 && end.row > start.row {
        end.row
    } else {
        end.row + 1
    };
    start.row as u32..last as u32
}

fn def_kind(kind: &str) -> Option<DefKind> {
    Some(match kind {
        "function_declaration" | "generator_function_declaration" | "function_signature" => {
            DefKind::Function
        }
        "class_declaration" | "abstract_class_declaration" => DefKind::Class,
        "method_definition" | "method_signature" | "abstract_method_signature" => DefKind::Method,
        "public_field_definition" => DefKind::Field,
        "interface_declaration" => DefKind::Interface,
        "type_alias_declaration" => DefKind::Type,
        "enum_declaration" => DefKind::Enum,
        "internal_module" | "module" => DefKind::Namespace,
        _ => return None,
    })
}

impl Walker<'_> {
    fn text(&self, node: Node) -> String {
        String::from_utf8_lossy(&self.src[node.byte_range()]).into_owned()
    }

    fn visit(&mut self, node: Node) {
        let kind = node.kind();
        if let Some(def) = def_kind(kind) {
            return self.definition(node, def);
        }
        match kind {
            "variable_declarator" => return self.variable(node),
            "import_statement" => return self.import(node),
            "debugger_statement" => self.facts.debug.push(DebugStmt {
                lines: lines(node),
                kind: DebugKind::Debugger,
                error_path: false,
            }),
            "expression_statement" => {
                if let Some(method) = node.named_child(0).and_then(|c| self.console_call(c)) {
                    self.facts.debug.push(DebugStmt {
                        lines: lines(node),
                        kind: DebugKind::Console(method),
                        error_path: error_path(node),
                    });
                }
            }
            "call_expression" => {
                if let Some(method) = self.console_call(node) {
                    self.facts.console_methods.insert(method);
                }
            }
            "export_statement" => {
                if node.child_by_field_name("source").is_some() {
                    self.facts.import_lines.push(lines(node));
                }
                if self.is_default_export(node) {
                    let named = node
                        .child_by_field_name("declaration")
                        .and_then(|d| d.child_by_field_name("name"))
                        .or_else(|| {
                            node.child_by_field_name("value")
                                .filter(|v| v.kind() == "identifier")
                        });
                    self.facts.default_export = named.map(|n| self.text(n));
                }
            }
            "identifier"
            | "type_identifier"
            | "property_identifier"
            | "shorthand_property_identifier"
            | "private_property_identifier" => {
                self.reference(node);
                return;
            }
            _ => {}
        }
        self.children(node, None);
    }

    fn children(&mut self, node: Node, skip: Option<usize>) {
        let mut cursor = node.walk();
        let kids: Vec<Node> = node.named_children(&mut cursor).collect();
        for child in kids {
            if Some(child.id()) != skip {
                self.visit(child);
            }
        }
    }

    fn push_def(
        &mut self,
        name: String,
        kind: DefKind,
        name_line: u32,
        lines: Range<u32>,
    ) -> usize {
        // Module level, or directly inside namespaces (reachable as `ns.name`).
        let top_level = self
            .scope
            .iter()
            .all(|&i| self.facts.defs[i].kind == DefKind::Namespace);
        self.facts.defs.push(Def {
            name,
            kind,
            name_line,
            lines,
            top_level,
            parent: self.scope.last().copied(),
        });
        self.facts.defs.len() - 1
    }

    fn definition(&mut self, node: Node, kind: DefKind) {
        let Some(name) = node.child_by_field_name("name") else {
            return self.children(node, None);
        };
        let index = self.push_def(
            self.text(name),
            kind,
            name.start_position().row as u32,
            lines(node),
        );
        self.scope.push(index);
        self.children(node, Some(name.id()));
        self.scope.pop();
    }

    /// `const a = ...`, `let { b, c } = ...`: every bound identifier is a definition.
    fn variable(&mut self, node: Node) {
        let Some(pattern) = node.child_by_field_name("name") else {
            return self.children(node, None);
        };
        let mut bound = Vec::new();
        collect_bindings(pattern, &mut bound);
        let range = lines(node);
        let first = self.facts.defs.len();
        for name in bound {
            self.push_def(
                self.text(name),
                DefKind::Variable,
                name.start_position().row as u32,
                range.clone(),
            );
        }
        // The value is scoped under the (first) declared name, so an arrow function's body
        // encloses under `const handler = () => { ... }`.
        let pushed = self.facts.defs.len() > first;
        if pushed {
            self.scope.push(first);
        }
        if let Some(value) = node.child_by_field_name("value") {
            self.visit(value);
        }
        if pushed {
            self.scope.pop();
        }
    }

    fn import(&mut self, node: Node) {
        let statement = lines(node);
        self.facts.import_lines.push(statement.clone());
        let Some(source) = node.child_by_field_name("source") else {
            return;
        };
        let source = self
            .text(source)
            .trim_matches(|c| c == '"' || c == '\'' || c == '`')
            .to_owned();
        let mut cursor = node.walk();
        let clauses: Vec<Node> = node
            .named_children(&mut cursor)
            .filter(|c| c.kind() == "import_clause")
            .collect();
        for clause in clauses {
            let mut cursor = clause.walk();
            let parts: Vec<Node> = clause.named_children(&mut cursor).collect();
            for part in parts {
                match part.kind() {
                    "identifier" => self.facts.imports.push(Import {
                        local: self.text(part),
                        imported: Imported::Default,
                        source: source.clone(),
                        lines: statement.clone(),
                    }),
                    "namespace_import" => {
                        if let Some(id) = part.named_child(0) {
                            self.facts.imports.push(Import {
                                local: self.text(id),
                                imported: Imported::Namespace,
                                source: source.clone(),
                                lines: statement.clone(),
                            });
                        }
                    }
                    "named_imports" => {
                        let mut cursor = part.walk();
                        let specs: Vec<Node> = part.named_children(&mut cursor).collect();
                        for spec in specs.into_iter().filter(|s| s.kind() == "import_specifier") {
                            let Some(name) = spec.child_by_field_name("name") else {
                                continue;
                            };
                            let local = spec.child_by_field_name("alias").unwrap_or(name);
                            self.facts.imports.push(Import {
                                local: self.text(local),
                                imported: Imported::Named(
                                    self.text(name).trim_matches('"').to_owned(),
                                ),
                                source: source.clone(),
                                lines: statement.clone(),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn reference(&mut self, node: Node) {
        let qualifier = node.parent().and_then(|p| {
            let is_property = p.kind() == "member_expression"
                && p.child_by_field_name("property").map(|n| n.id()) == Some(node.id());
            p.child_by_field_name("object")
                .filter(|o| is_property && o.kind() == "identifier")
                .map(|o| self.text(o))
        });
        self.facts.refs.push(Ref {
            name: self.text(node),
            line: node.start_position().row as u32,
            qualifier,
        });
    }

    /// `console.<method>(...)` → `Some(method)`.
    fn console_call(&self, node: Node) -> Option<String> {
        if node.kind() != "call_expression" {
            return None;
        }
        let callee = node.child_by_field_name("function")?;
        if callee.kind() != "member_expression" {
            return None;
        }
        let object = callee.child_by_field_name("object")?;
        let property = callee.child_by_field_name("property")?;
        (object.kind() == "identifier" && self.text(object) == "console")
            .then(|| self.text(property))
    }

    fn is_default_export(&self, node: Node) -> bool {
        let mut cursor = node.walk();
        node.children(&mut cursor).any(|c| c.kind() == "default")
    }
}

/// Identifiers bound by a declarator's name: `a`, `{ a, b: c }`, `[d, ...e]`.
fn collect_bindings<'t>(node: Node<'t>, out: &mut Vec<Node<'t>>) {
    match node.kind() {
        "identifier" | "shorthand_property_identifier_pattern" => out.push(node),
        _ => {
            let mut cursor = node.walk();
            let kids: Vec<Node> = node.named_children(&mut cursor).collect();
            for child in kids {
                // In `{ key: value }` patterns only the value side binds.
                if node.kind() == "pair_pattern"
                    && node.child_by_field_name("key").map(|k| k.id()) == Some(child.id())
                {
                    continue;
                }
                collect_bindings(child, out);
            }
        }
    }
}

/// A project's build-info file name, from its path rather than its position in the list, so
/// a carve that checks a different set of projects reuses each one's own build info
/// (DECISIONS.md D51). `%` and `/` are escaped, so two paths never share a name.
fn build_info_name(project: &str) -> String {
    if project == "." {
        return "%2E".to_owned();
    }
    project.replace('%', "%25").replace('/', "%2F")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_info_names_are_by_path_and_never_collide() {
        assert_eq!(build_info_name("."), "%2E");
        assert_eq!(build_info_name("packages/a"), "packages%2Fa");
        assert_ne!(build_info_name("a/b"), build_info_name("a%2Fb"));
        assert_ne!(build_info_name("."), build_info_name("%2E"));
    }

    fn facts(src: &str) -> FileFacts {
        TypeScript.analyze(b"x.tsx", src.as_bytes()).unwrap()
    }

    #[test]
    fn collects_definitions_with_scope() {
        let f = facts(
            "export function parseHeader(s: string) {\n  const inner = 1;\n  return s;\n}\nclass Box {\n  size = 1;\n  grow() {}\n}\nexport const fmtBytes = (n: number) => `${n}`;\ninterface Opts { a: string }\ntype Id = string;\nenum Color { Red }\n",
        );
        let summary: Vec<(String, DefKind, bool, Range<u32>)> = f
            .defs
            .iter()
            .map(|d| (d.name.clone(), d.kind, d.top_level, d.lines.clone()))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("parseHeader".into(), DefKind::Function, true, 0..4),
                ("inner".into(), DefKind::Variable, false, 1..2),
                ("Box".into(), DefKind::Class, true, 4..8),
                ("size".into(), DefKind::Field, false, 5..6),
                ("grow".into(), DefKind::Method, false, 6..7),
                ("fmtBytes".into(), DefKind::Variable, true, 8..9),
                ("Opts".into(), DefKind::Interface, true, 9..10),
                ("Id".into(), DefKind::Type, true, 10..11),
                ("Color".into(), DefKind::Enum, true, 11..12),
            ]
        );
    }

    #[test]
    fn collects_imports_refs_and_debug() {
        let f = facts(
            "import def, { a, b as c } from './util.js';\nimport * as ns from \"@/lib/x\";\nexport function run() {\n  console.log('here');\n  debugger;\n  ns.helper(c);\n  console.error('kept');\n}\n",
        );
        assert_eq!(
            f.imports,
            vec![
                Import {
                    local: "def".into(),
                    imported: Imported::Default,
                    source: "./util.js".into(),
                    lines: 0..1
                },
                Import {
                    local: "a".into(),
                    imported: Imported::Named("a".into()),
                    source: "./util.js".into(),
                    lines: 0..1
                },
                Import {
                    local: "c".into(),
                    imported: Imported::Named("b".into()),
                    source: "./util.js".into(),
                    lines: 0..1
                },
                Import {
                    local: "ns".into(),
                    imported: Imported::Namespace,
                    source: "@/lib/x".into(),
                    lines: 1..2
                },
            ]
        );
        assert_eq!(f.import_lines, vec![0..1, 1..2]);
        let kinds: Vec<&DebugKind> = f.debug.iter().map(|d| &d.kind).collect();
        assert_eq!(
            kinds,
            vec![
                &DebugKind::Console("log".into()),
                &DebugKind::Debugger,
                &DebugKind::Console("error".into())
            ]
        );
        assert!(f.console_methods.contains("log") && f.console_methods.contains("error"));
        let helper = f.refs.iter().find(|r| r.name == "helper").unwrap();
        assert_eq!(helper.qualifier.as_deref(), Some("ns"));
        assert!(f.refs.iter().any(|r| r.name == "c" && r.line == 5));
        assert!(
            !f.refs.iter().any(|r| r.name == "run"),
            "definition names are not references"
        );
    }

    #[test]
    fn arrow_function_bodies_are_not_top_level() {
        let f = facts("export const Button = () => {\n  const inner = 1;\n  return inner;\n};\n");
        let inner = f.defs.iter().find(|d| d.name == "inner").unwrap();
        assert!(!inner.top_level);
        assert_eq!(inner.parent, Some(0));
    }

    #[test]
    fn default_check_follows_project_references() {
        let dir = tempfile::tempdir().unwrap();
        let check = |json: &str| {
            std::fs::write(dir.path().join("tsconfig.json"), json).unwrap();
            TypeScript
                .default_check(dir.path(), &[], &[])
                .map(|c| c.display)
        };
        assert_eq!(check("{}").unwrap(), "tsc --noEmit -p .");
        assert_eq!(
            check("{ \"files\": [], \"references\": [{ \"path\": \"./tsconfig.app.json\" }, { \"path\": \"./tsconfig.node.json\" }] }").unwrap(),
            "tsc --noEmit -p tsconfig.app.json && tsc --noEmit -p tsconfig.node.json"
        );
        assert_eq!(
            check("{ \"include\": [\"src\"], \"references\": [{ \"path\": \"tools\" }] }").unwrap(),
            "tsc --noEmit -p . && tsc --noEmit -p tools"
        );
        std::fs::remove_file(dir.path().join("tsconfig.json")).unwrap();
        let err = TypeScript
            .default_check(dir.path(), &["web/tsconfig.json".into()], &[])
            .unwrap_err();
        assert!(err.contains("web/tsconfig.json"), "{err}");
    }

    #[test]
    fn default_check_covers_projects_owning_changed_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("tsconfig.json"),
            "{ \"include\": [\"src\"] }",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("web/node_modules")).unwrap();
        std::fs::write(dir.path().join("web/tsconfig.json"), "{}").unwrap();
        let changed = [
            "src/a.ts".to_owned(),
            "web/app/page.tsx".to_owned(),
            "scripts/x.ts".to_owned(),
        ];
        let check = TypeScript.default_check(dir.path(), &[], &changed).unwrap();
        assert_eq!(check.display, "tsc --noEmit -p . && tsc --noEmit -p web");
        assert_eq!(check.links, vec!["web/node_modules"]);
        // No root config: the owning project alone is the check.
        std::fs::remove_file(dir.path().join("tsconfig.json")).unwrap();
        let check = TypeScript.default_check(dir.path(), &[], &changed).unwrap();
        assert_eq!(check.display, "tsc --noEmit -p web");
    }

    #[test]
    fn default_exports_are_named() {
        assert_eq!(
            facts("export default function App() {}\n")
                .default_export
                .as_deref(),
            Some("App")
        );
        assert_eq!(
            facts("const X = 1;\nexport default X;\n")
                .default_export
                .as_deref(),
            Some("X")
        );
    }
}
