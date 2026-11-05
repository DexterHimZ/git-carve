//! Resolving TypeScript module specifiers to repository paths (DECISIONS.md D10):
//! relative paths, the `.js`-suffix ESM style, and tsconfig `paths` aliases.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde_json::Value;

/// Where an import points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A repository-relative path (the file may or may not be among the changed files).
    Path(Vec<u8>),
    /// A package, or an alias carve cannot resolve.
    Unresolved,
}

/// Candidate files for a module path without extension, in TypeScript's lookup order; the
/// JavaScript ones last, as under `allowJs` (D54).
const EXTENSIONS: &[&str] = &[
    ".ts",
    ".tsx",
    ".d.ts",
    ".mts",
    ".cts",
    "/index.ts",
    "/index.tsx",
    ".js",
    ".jsx",
    ".mjs",
    ".cjs",
    "/index.js",
    "/index.jsx",
];

pub struct Resolver<'a> {
    root: &'a Path,
    /// Nearest tsconfig per directory, memoized: (config dir, `paths`, `baseUrl`).
    configs: HashMap<Vec<u8>, Option<Aliases>>,
}

#[derive(Clone, Debug)]
struct Aliases {
    base: Vec<u8>,
    paths: Vec<(String, Vec<String>)>,
}

impl<'a> Resolver<'a> {
    pub fn new(root: &'a Path) -> Self {
        Self {
            root,
            configs: HashMap::new(),
        }
    }

    /// Resolves `spec` imported from the file at repository path `from`. `exists` says
    /// whether a repository path is a file in the version being analyzed.
    pub fn resolve(&mut self, from: &[u8], spec: &str, exists: &dyn Fn(&[u8]) -> bool) -> Target {
        let dir = parent(from);
        let bases: Vec<Vec<u8>> = if spec.starts_with("./") || spec.starts_with("../") {
            vec![join(&dir, spec.as_bytes())]
        } else {
            self.aliases(&dir)
                .map(|a| a.expand(spec))
                .unwrap_or_default()
        };
        for base in &bases {
            if let Some(found) = candidates(base).into_iter().find(|c| exists(c)) {
                return Target::Path(found);
            }
        }
        Target::Unresolved
    }

    fn aliases(&mut self, dir: &[u8]) -> Option<Aliases> {
        if let Some(cached) = self.configs.get(dir) {
            return cached.clone();
        }
        let found = self.load(dir).or_else(|| {
            if dir.is_empty() {
                None
            } else {
                self.aliases(&parent(dir))
            }
        });
        self.configs.insert(dir.to_vec(), found.clone());
        found
    }

    /// `tsconfig.json` in `dir` (following one `extends`), or a Vite-style
    /// `tsconfig.app.json` next to it.
    fn load(&self, dir: &[u8]) -> Option<Aliases> {
        let read = |name: &str| -> Option<Value> {
            let rel = join(dir, name.as_bytes());
            let text = fs::read(self.root.join(String::from_utf8_lossy(&rel).as_ref())).ok()?;
            serde_json::from_slice(&strip_jsonc(&text)).ok()
        };
        let main = Some(read("tsconfig.json")?);
        let options = |v: &Option<Value>| {
            v.as_ref()
                .and_then(|v| v.get("compilerOptions"))
                .filter(|o| o.get("paths").is_some())
                .cloned()
        };
        let extended = main
            .as_ref()
            .and_then(|m| m.get("extends"))
            .and_then(Value::as_str)
            .filter(|e| e.starts_with('.'))
            .and_then(|e| read(e.trim_start_matches("./")));
        let chosen = options(&main)
            .or_else(|| options(&read("tsconfig.app.json")))
            .or_else(|| options(&extended))?;
        let base = match chosen.get("baseUrl").and_then(Value::as_str) {
            Some(url) => join(dir, format!("{url}/x").as_bytes()),
            None => join(dir, b"x"),
        };
        let paths = chosen
            .get("paths")?
            .as_object()?
            .iter()
            .map(|(k, v)| {
                let targets = v
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                (k.clone(), targets)
            })
            .collect();
        Some(Aliases {
            base: parent(&base),
            paths,
        })
    }
}

impl Aliases {
    fn expand(&self, spec: &str) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for (pattern, targets) in &self.paths {
            let captured = match pattern.split_once('*') {
                Some((prefix, suffix)) => spec
                    .strip_prefix(prefix)
                    .and_then(|rest| rest.strip_suffix(suffix)),
                None => (pattern == spec).then_some(""),
            };
            if let Some(captured) = captured {
                for target in targets {
                    let path = target.replacen('*', captured, 1);
                    out.push(join(&self.base, format!("./{path}").as_bytes()));
                }
            }
        }
        out
    }
}

fn candidates(base: &[u8]) -> Vec<Vec<u8>> {
    let mut out = vec![base.to_vec()];
    // ESM style: `import './util.js'` names `util.ts`.
    for (js, ts) in [
        (".js", ".ts"),
        (".jsx", ".tsx"),
        (".mjs", ".mts"),
        (".cjs", ".cts"),
    ] {
        if let Some(stem) = base.strip_suffix(js.as_bytes()) {
            out.push([stem, ts.as_bytes()].concat());
        }
    }
    out.extend(EXTENSIONS.iter().map(|ext| [base, ext.as_bytes()].concat()));
    out
}

/// The directory part of a repository path (`b""` at the top level).
fn parent(path: &[u8]) -> Vec<u8> {
    match path.iter().rposition(|&b| b == b'/') {
        Some(i) => path[..i].to_vec(),
        None => Vec::new(),
    }
}

/// Joins `dir` and a relative path, normalizing `.` and `..`.
fn join(dir: &[u8], rel: &[u8]) -> Vec<u8> {
    let mut parts: Vec<&[u8]> = dir
        .split(|&b| b == b'/')
        .filter(|p| !p.is_empty())
        .collect();
    for part in rel.split(|&b| b == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join(&b'/')
}

/// Removes `//` and `/* */` comments and trailing commas, so tsconfig parses as JSON.
pub fn strip_jsonc(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    let mut in_string = false;
    while i < text.len() {
        let c = text[i];
        if in_string {
            out.push(c);
            if c == b'\\' && i + 1 < text.len() {
                out.push(text[i + 1]);
                i += 1;
            } else if c == b'"' {
                in_string = false;
            }
        } else if c == b'"' {
            in_string = true;
            out.push(c);
        } else if text[i..].starts_with(b"//") {
            while i < text.len() && text[i] != b'\n' {
                i += 1;
            }
            continue;
        } else if text[i..].starts_with(b"/*") {
            i += 2;
            while i + 1 < text.len() && !text[i..].starts_with(b"*/") {
                i += 1;
            }
            i += 2;
            continue;
        } else if c == b',' {
            let next = text[i + 1..].iter().find(|b| !b.is_ascii_whitespace());
            if !matches!(next, Some(b'}') | Some(b']')) {
                out.push(c);
            }
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_and_normalizes() {
        assert_eq!(join(b"src/net", b"../util"), b"src/util");
        assert_eq!(join(b"", b"./a/b"), b"a/b");
        assert_eq!(parent(b"src/a.ts"), b"src");
        assert_eq!(parent(b"a.ts"), b"");
    }

    #[test]
    fn strips_jsonc() {
        let text = br#"{ // c
  "a": "http://x", /* block */ "b": [1, 2,],
}"#;
        let v: Value = serde_json::from_slice(&strip_jsonc(text)).unwrap();
        assert_eq!(v["a"], "http://x");
        assert_eq!(v["b"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn resolves_relative_esm_and_alias_imports() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("tsconfig.json"),
            "{ \"compilerOptions\": { \"paths\": { \"@/*\": [\"./src/*\"] } }, }",
        )
        .unwrap();
        let files: Vec<&[u8]> = vec![b"src/util.ts", b"src/lib/index.ts", b"src/Button.tsx"];
        let exists = |p: &[u8]| files.contains(&p);
        let mut r = Resolver::new(dir.path());
        let t = |r: &mut Resolver, from: &[u8], spec| r.resolve(from, spec, &exists);
        assert_eq!(
            t(&mut r, b"src/net/a.ts", "../util"),
            Target::Path(b"src/util.ts".to_vec())
        );
        assert_eq!(
            t(&mut r, b"src/a.ts", "./util.js"),
            Target::Path(b"src/util.ts".to_vec())
        );
        assert_eq!(
            t(&mut r, b"src/a.ts", "./lib"),
            Target::Path(b"src/lib/index.ts".to_vec())
        );
        assert_eq!(
            t(&mut r, b"src/deep/a.ts", "@/Button"),
            Target::Path(b"src/Button.tsx".to_vec())
        );
        assert_eq!(t(&mut r, b"src/a.ts", "react"), Target::Unresolved);
        // D54: JavaScript resolves after TypeScript, as under allowJs.
        let js: Vec<&[u8]> = vec![
            b"src/legacy.js",
            b"src/both.ts",
            b"src/both.js",
            b"src/w/index.jsx",
        ];
        let exists = |p: &[u8]| js.contains(&p);
        let t = |r: &mut Resolver, from: &[u8], spec| r.resolve(from, spec, &exists);
        assert_eq!(
            t(&mut r, b"src/a.ts", "./legacy"),
            Target::Path(b"src/legacy.js".to_vec())
        );
        assert_eq!(
            t(&mut r, b"src/a.js", "./both"),
            Target::Path(b"src/both.ts".to_vec())
        );
        assert_eq!(
            t(&mut r, b"src/a.js", "./w"),
            Target::Path(b"src/w/index.jsx".to_vec())
        );
    }
}
