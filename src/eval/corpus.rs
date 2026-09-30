//! Building the evaluation corpus (EVAL.md §1, DECISIONS.md D22–D24): sources with roles,
//! clones in a cache (source repositories are only ever read), a leakage guard, deterministic
//! non-overlapping windows, per-line provenance from `git blame`, and debug injection.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tree_sitter::{Node, Parser};

use super::sha256::{Rng, digest_mod, hex, sha256};
use crate::error::{Error, Result, io};
use crate::git::Git;
use crate::lang::adapter_for;
use crate::text;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    EvalUser,
    EvalOss,
    Train,
}

impl Role {
    pub fn is_eval(self) -> bool {
        matches!(self, Role::EvalUser | Role::EvalOss)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Source {
    pub name: String,
    /// A path or URL that `git clone` accepts.
    pub location: String,
    pub role: Role,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WindowRules {
    pub max_files: usize,
    /// Per-source cap, so one large repository cannot dominate pooled metrics.
    pub max_windows_per_source: usize,
    /// Commits per window (EVAL.md: 2–4). Training corpora may differ (DECISIONS.md D38).
    #[serde(default = "two")]
    pub min_commits: usize,
    #[serde(default = "four")]
    pub max_commits: usize,
}

fn two() -> usize {
    2
}

fn four() -> usize {
    4
}

impl Default for WindowRules {
    fn default() -> Self {
        Self {
            max_files: 15,
            max_windows_per_source: 150,
            min_commits: 2,
            max_commits: 4,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sources {
    pub version: u32,
    pub seed: u64,
    #[serde(default)]
    pub window: WindowRules,
    pub sources: Vec<Source>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Split {
    Tune,
    Test,
}

/// Which window commit produced each changed line (index into `Window::commits`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// 0-based line in the last commit's file → commit index.
    pub added: BTreeMap<u32, u8>,
    /// 0-based line in the base file → index of the commit that removed it.
    pub removed: BTreeMap<u32, u8>,
}

/// A debug statement inserted before line `at` (0-based, in the last commit's file).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Injection {
    pub path: String,
    pub at: u32,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Window {
    pub id: String,
    pub source: String,
    pub role: Role,
    pub root: String,
    pub base: String,
    pub commits: Vec<String>,
    pub split: Split,
    pub files: Vec<String>,
    pub injections: Vec<Injection>,
    pub provenance: BTreeMap<String, Provenance>,
}

impl Window {
    pub fn last(&self) -> &str {
        self.commits.last().expect("windows have commits")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub seed: u64,
    pub sources: Vec<SourceRecord>,
    pub windows: Vec<Window>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceRecord {
    pub name: String,
    pub role: Role,
    pub root: String,
    pub head: String,
    pub windows: usize,
    /// Why candidate windows were rejected, by reason.
    pub rejected: BTreeMap<String, usize>,
}

/// A source cloned into the cache.
pub struct Cloned {
    pub source: Source,
    pub dir: PathBuf,
    pub git: Git,
    pub root: String,
    pub head: String,
}

/// `$XDG_CACHE_HOME/git-carve/eval`, or `~/.cache/git-carve/eval`.
pub fn cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from(".cache"));
    base.join("git-carve").join("eval")
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Clones (or reuses) every source, dedupes by root commit, and refuses a training source
/// that shares history with any eval source (D22). Returns the sources whose role is in
/// `roles`, in file order.
pub fn clone_sources(
    sources: &Sources,
    roles: &[Role],
    cache: &Path,
    env: &Git,
) -> Result<Vec<Cloned>> {
    let repos = cache.join("repos");
    std::fs::create_dir_all(&repos).map_err(io(format!("create {}", repos.display())))?;
    let mut seen: BTreeMap<String, (String, Role)> = BTreeMap::new();
    let mut eval_roots: BTreeMap<String, String> = BTreeMap::new();
    let mut all = Vec::new();
    for source in &sources.sources {
        let dir = repos.join(sanitize(&source.name));
        if !dir.join(".git").exists() {
            let target = dir.to_string_lossy().into_owned();
            env.with_dir(&repos).run([
                "clone",
                "--quiet",
                "--no-tags",
                source.location.as_str(),
                target.as_str(),
            ])?;
        }
        let git = env.with_dir(&dir);
        let head = git.run_str(["rev-parse", "refs/remotes/origin/HEAD"])?;
        let root = git
            .run_str(["rev-list", "--max-parents=0", head.as_str()])?
            .lines()
            .min()
            .unwrap_or_default()
            .to_owned();
        if let Some((first, first_role)) = seen.get(&root) {
            // A training copy of an eval repository is the leak D22 exists to stop: refuse,
            // never quietly dedupe it away.
            if first_role.is_eval() != source.role.is_eval() {
                return Err(Error::Invalid(format!(
                    "{} ({:?}) shares history with {first} ({first_role:?}) (root {root}); refusing, it would leak into the evaluation",
                    source.name, source.role
                )));
            }
            eprintln!(
                "eval: {} shares its root commit with {first}; using {first} only",
                source.name
            );
            continue;
        }
        seen.insert(root.clone(), (source.name.clone(), source.role));
        if source.role.is_eval() {
            eval_roots.insert(root.clone(), source.name.clone());
        }
        all.push(Cloned {
            source: source.clone(),
            dir,
            git,
            root,
            head,
        });
    }
    for c in all.iter().filter(|c| c.source.role == Role::Train) {
        if let Some(eval) = eval_roots.get(&c.root) {
            return Err(Error::Invalid(format!(
                "training source {} shares history with eval source {eval} (root {}); refusing, it would leak into the evaluation",
                c.source.name, c.root
            )));
        }
    }
    Ok(all
        .into_iter()
        .filter(|c| roles.contains(&c.source.role))
        .collect())
}

const LOCKFILES: &[&str] = &[
    "package.json",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lock",
    "bun.lockb",
];
const VENDORED: &[&str] = &["vendor", "vendors", "node_modules", "third_party"];

/// Non-overlapping windows of `min_commits..=max_commits` commits (2–4 for EVAL) along
/// first-parent history (D24).
pub fn windows(
    c: &Cloned,
    rules: &WindowRules,
    seed: u64,
) -> Result<(Vec<Window>, BTreeMap<String, usize>)> {
    let git = &c.git;
    let chain: Vec<String> = git
        .run_str(["rev-list", "--first-parent", "--reverse", c.head.as_str()])?
        .lines()
        .map(str::to_owned)
        .collect();
    let mut parents: BTreeMap<String, usize> = BTreeMap::new();
    let mut author: BTreeMap<String, String> = BTreeMap::new();
    for line in git
        .run_str([
            "log",
            "--first-parent",
            "--format=%H %P%x00%ae",
            c.head.as_str(),
        ])?
        .lines()
    {
        let (ids, email) = line.split_once('\0').unwrap_or((line, ""));
        let mut ids = ids.split(' ');
        let commit = ids.next().unwrap_or_default().to_owned();
        parents.insert(commit.clone(), ids.filter(|s| !s.is_empty()).count());
        author.insert(commit, email.to_owned());
    }
    let hex_len = git.object_format()?.hex_len();
    let mut rejected: BTreeMap<String, usize> = BTreeMap::new();
    let mut out = Vec::new();
    let mut i = 1;
    while i < chain.len() {
        let span = (rules.max_commits.max(rules.min_commits) - rules.min_commits + 1) as u64;
        let k = rules.min_commits
            + digest_mod(&sha256(format!("{}{}", c.root, chain[i]).as_bytes()), span) as usize;
        if i + k > chain.len() {
            break;
        }
        let base = &chain[i - 1];
        let commits = &chain[i..i + k];
        match accept(c, base, commits, &parents, &author, rules, hex_len, seed)? {
            Ok(window) => {
                out.push(window);
                i += k;
            }
            Err(reason) => {
                *rejected.entry(reason).or_default() += 1;
                i += 1;
            }
        }
    }
    if out.len() > rules.max_windows_per_source {
        // Deterministic subsample: smallest window keys, then back to history order.
        let key = |w: &Window| hex(&sha256(format!("{}{}", w.root, w.base).as_bytes()));
        out.sort_by_key(key);
        out.truncate(rules.max_windows_per_source);
        let order: BTreeMap<&str, usize> = chain
            .iter()
            .enumerate()
            .map(|(n, s)| (s.as_str(), n))
            .collect();
        out.sort_by_key(|w| order[w.base.as_str()]);
    }
    Ok((out, rejected))
}

#[allow(clippy::too_many_arguments)]
fn accept(
    c: &Cloned,
    base: &str,
    commits: &[String],
    parents: &BTreeMap<String, usize>,
    author: &BTreeMap<String, String>,
    rules: &WindowRules,
    hex_len: usize,
    seed: u64,
) -> Result<std::result::Result<Window, String>> {
    let git = &c.git;
    let last = commits.last().expect("k >= 2");
    if commits.iter().any(|x| parents.get(x) != Some(&1)) {
        return Ok(Err("merge in window".into()));
    }
    let authors: BTreeSet<&String> = commits.iter().filter_map(|x| author.get(x)).collect();
    if authors.len() != 1 {
        return Ok(Err("several authors".into()));
    }
    let raw = git.run([
        "diff",
        "--raw",
        "-z",
        "--no-renames",
        "--no-abbrev",
        base,
        last.as_str(),
    ])?;
    let fields: Vec<&[u8]> = raw.split(|&b| b == 0).filter(|f| !f.is_empty()).collect();
    let mut files = Vec::new();
    for pair in fields.chunks(2) {
        let [meta, path] = pair else { continue };
        let meta = String::from_utf8_lossy(meta);
        if meta.contains("160000") {
            return Ok(Err("submodule change".into()));
        }
        files.push(String::from_utf8_lossy(path).into_owned());
    }
    if files.is_empty() || files.len() > rules.max_files {
        return Ok(Err(if files.is_empty() {
            "no file changes"
        } else {
            "too many files"
        }
        .into()));
    }
    for f in &files {
        let name = f.rsplit('/').next().unwrap_or(f);
        if LOCKFILES.contains(&name) {
            return Ok(Err("manifest or lockfile change".into()));
        }
        if f.split('/').any(|seg| VENDORED.contains(&seg)) {
            return Ok(Err("vendored path".into()));
        }
    }
    let mut attr_input = Vec::new();
    for f in &files {
        attr_input.extend_from_slice(f.as_bytes());
        attr_input.push(0);
    }
    let attrs = git.run_with_stdin(
        [
            "check-attr",
            "-z",
            "--stdin",
            "--source",
            last.as_str(),
            "filter",
        ],
        &attr_input,
    )?;
    if String::from_utf8_lossy(&attrs)
        .split('\0')
        .any(|v| v == "lfs")
    {
        return Ok(Err("LFS file".into()));
    }

    let in_tree = |tree: &str| -> Result<BTreeSet<String>> {
        let mut args = vec!["ls-tree", "-r", "--name-only", "-z", tree, "--"];
        args.extend(files.iter().map(String::as_str));
        Ok(String::from_utf8_lossy(&git.run(args)?)
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect())
    };
    let (in_base, in_last) = (in_tree(base)?, in_tree(last)?);
    let range = format!("{base}..{last}");
    let index: BTreeMap<&str, u8> = commits
        .iter()
        .enumerate()
        .map(|(n, x)| (x.as_str(), n as u8))
        .collect();
    let mut provenance = BTreeMap::new();
    for f in &files {
        let mut p = Provenance::default();
        if in_last.contains(f) {
            for (line, sha) in blame(
                git,
                &["blame", "--porcelain", range.as_str(), "--", f],
                hex_len,
            )? {
                if let Some(&n) = index.get(sha.as_str()) {
                    p.added.insert(line, n);
                }
            }
        }
        if in_base.contains(f) {
            for (line, sha) in blame(
                git,
                &["blame", "--porcelain", "--reverse", range.as_str(), "--", f],
                hex_len,
            )? {
                // The youngest commit that still had the line; its successor removed it.
                let removed_by = if sha == base {
                    Some(0)
                } else {
                    index.get(sha.as_str()).map(|&n| n + 1)
                };
                if let Some(n) = removed_by.filter(|&n| (n as usize) < commits.len()) {
                    p.removed.insert(line, n);
                }
            }
        }
        if !p.added.is_empty() || !p.removed.is_empty() {
            provenance.insert(f.clone(), p);
        }
    }
    if provenance.is_empty() {
        return Ok(Err("no line-level changes".into()));
    }
    let split = if digest_mod(&sha256(format!("{}{}", c.root, base).as_bytes()), 10) < 3 {
        Split::Tune
    } else {
        Split::Test
    };
    let mut window = Window {
        id: format!("{}:{}", c.source.name, &base[..10.min(base.len())]),
        source: c.source.name.clone(),
        role: c.source.role,
        root: c.root.clone(),
        base: base.to_owned(),
        commits: commits.to_vec(),
        split,
        files,
        injections: Vec::new(),
        provenance,
    };
    window.injections = plan_injections(git, &window, &in_base, seed)?;
    Ok(Ok(window))
}

/// `(0-based final line, commit)` pairs from `git blame --porcelain`.
fn blame(git: &Git, args: &[&str], hex_len: usize) -> Result<Vec<(u32, String)>> {
    let out = git.run(args.iter().copied())?;
    let mut lines = Vec::new();
    for line in String::from_utf8_lossy(&out).lines() {
        let mut parts = line.split(' ');
        let (Some(sha), Some(_orig), Some(fin)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        if sha.len() == hex_len && sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            if let Ok(n) = fin.parse::<u32>() {
                lines.push((n - 1, sha.to_owned()));
            }
        }
    }
    Ok(lines)
}

const INJECTED: &[&str] = &[
    "console.log(\"here\");",
    "console.log(\"debug\", 42);",
    "debugger;",
];

/// EVAL.md's debug injection: in half the windows, 1–2 statements after a statement inside a
/// function body, with an unchanged line on each side, and the file must still parse.
fn plan_injections(
    git: &Git,
    w: &Window,
    in_base: &BTreeSet<String>,
    seed: u64,
) -> Result<Vec<Injection>> {
    let digest = sha256(format!("{seed}:{}:{}:inject", w.root, w.base).as_bytes());
    if digest[0] % 2 == 1 {
        return Ok(Vec::new());
    }
    let wanted = 1 + (digest[1] % 2) as usize;
    let mut rng = Rng::from_digest(&digest);
    let mut candidates: Vec<(String, u32, String)> = Vec::new();
    // Only files that exist in the base: an injection into a new file would merge into its
    // whole-file atom and make that atom's ground truth mixed.
    for path in w.files.iter().filter(|f| {
        adapter_for(f.as_bytes()).is_some() && w.provenance.contains_key(*f) && in_base.contains(*f)
    }) {
        let Ok(last) = git.run(["cat-file", "blob", &format!("{}:{path}", w.last())]) else {
            continue;
        };
        let base = git.run(["cat-file", "blob", &format!("{}:{path}", w.base)])?;
        let Some(tree) = parse(path, &last) else {
            continue;
        };
        if tree.root_node().has_error() {
            continue;
        }
        let n_lines = text::line_starts(&last).len() - 1;
        let mut touched = vec![false; n_lines + 1];
        for r in text::diff(&base, &last) {
            if r.new.is_empty() {
                for l in [r.new.start.saturating_sub(1), r.new.start] {
                    if let Some(t) = touched.get_mut(l as usize) {
                        *t = true;
                    }
                }
            }
            for l in r.new.clone() {
                touched[l as usize] = true;
            }
        }
        let lines: Vec<&[u8]> = last.split(|&b| b == b'\n').collect();
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
            let in_body = node.parent().is_some_and(|p| p.kind() == "statement_block");
            let terminal = matches!(
                node.kind(),
                "return_statement"
                    | "throw_statement"
                    | "break_statement"
                    | "continue_statement"
                    | "comment"
            );
            if !in_body || terminal {
                continue;
            }
            let end = end_row(node);
            let at = end + 1;
            if (at as usize) < n_lines && !touched[end as usize] && !touched[at as usize] {
                let first = lines
                    .get(node.start_position().row)
                    .copied()
                    .unwrap_or_default();
                let indent: String = String::from_utf8_lossy(first)
                    .chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .collect();
                candidates.push((path.clone(), at, indent));
            }
        }
    }
    candidates.sort();
    candidates.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
    let mut chosen: Vec<Injection> = Vec::new();
    while chosen.len() < wanted && !candidates.is_empty() {
        let (path, at, indent) = candidates.remove(rng.below(candidates.len()));
        if chosen
            .iter()
            .any(|c| c.path == path && c.at.abs_diff(at) < 3)
        {
            continue;
        }
        let text = format!("{indent}{}", INJECTED[rng.below(INJECTED.len())]);
        let content = git.run(["cat-file", "blob", &format!("{}:{path}", w.last())])?;
        let mut trial = chosen.clone();
        trial.push(Injection {
            path: path.clone(),
            at,
            text: text.clone(),
        });
        let injected = apply(
            &content,
            &trial
                .iter()
                .filter(|i| i.path == path)
                .cloned()
                .collect::<Vec<_>>(),
        );
        if parse(&path, &injected).is_some_and(|t| !t.root_node().has_error()) {
            chosen.push(Injection { path, at, text });
        }
    }
    chosen.sort_by(|a, b| (&a.path, a.at).cmp(&(&b.path, b.at)));
    Ok(chosen)
}

fn end_row(node: Node) -> u32 {
    let end = node.end_position();
    let row = if end.column == 0 && end.row > node.start_position().row {
        end.row - 1
    } else {
        end.row
    };
    row as u32
}

fn parse(path: &str, src: &[u8]) -> Option<tree_sitter::Tree> {
    let language = if path.ends_with(".tsx") {
        tree_sitter_typescript::LANGUAGE_TSX
    } else {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT
    };
    let mut parser = Parser::new();
    parser.set_language(&language.into()).ok()?;
    parser.parse(src, None)
}

/// The file with each injection inserted before its line (all for the same path).
pub fn apply(content: &[u8], injections: &[Injection]) -> Vec<u8> {
    let starts = text::line_starts(content);
    let mut sorted: Vec<&Injection> = injections.iter().collect();
    sorted.sort_by_key(|i| i.at);
    let mut out = Vec::with_capacity(content.len() + 64);
    let mut from = 0usize;
    for inj in sorted {
        let pos = starts
            .get(inj.at as usize)
            .copied()
            .unwrap_or(content.len());
        out.extend_from_slice(&content[from..pos]);
        out.extend_from_slice(inj.text.as_bytes());
        out.push(b'\n');
        from = pos;
    }
    out.extend_from_slice(&content[from..]);
    out
}

/// Builds the manifest for the given roles.
pub fn build(sources: &Sources, roles: &[Role], cache: &Path, env: &Git) -> Result<Manifest> {
    let clones = clone_sources(sources, roles, cache, env)?;
    let mut manifest = Manifest {
        version: 1,
        seed: sources.seed,
        sources: Vec::new(),
        windows: Vec::new(),
    };
    for c in &clones {
        let (windows, rejected) = windows(c, &sources.window, sources.seed)?;
        eprintln!(
            "eval: {}: {} windows ({} rejected)",
            c.source.name,
            windows.len(),
            rejected.values().sum::<usize>()
        );
        manifest.sources.push(SourceRecord {
            name: c.source.name.clone(),
            role: c.source.role,
            root: c.root.clone(),
            head: c.head.clone(),
            windows: windows.len(),
            rejected,
        });
        manifest.windows.extend(windows);
    }
    Ok(manifest)
}
