//! Facts per atom (SPEC §6.2): which definitions an atom adds, removes, or modifies, which
//! references it adds or removes, its enclosing definition, and its class (normal, style,
//! debug, import-only, or symbol-less). Cross-file references resolve through imports
//! (DECISIONS.md D10).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;

use crate::atom::{AtomKind, Changes, Whole};
use crate::error::Result;
use crate::lang::resolve::{Resolver, Target};
use crate::lang::typescript::DEBUG_CONSOLE;
use crate::lang::{DebugKind, DefKind, FileFacts, Imported, adapter_for};
use crate::model::Mode;
use crate::repo::Repo;

/// Files larger than this are not parsed (same cap as line diffing).
const MAX_PARSE_BYTES: u64 = 2 << 20;

/// A definition, identified across versions and files.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Key {
    /// Module-level in the file (index into `Changes::files`): reachable from other files.
    Top(usize, String),
    /// Nested, or otherwise file-local.
    Local(usize, String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Class {
    #[default]
    Normal,
    /// No symbol facts at all: a file without an adapter, a binary, a mode change.
    Symbolless,
    /// Differs from the base only in whitespace (§6.5).
    Style,
    /// Only adds debug statements (§6.5). `leave` follows DECISIONS.md D8.
    Debug { leave: bool },
    /// Only edits import statements: placed by the import post-pass (§6.4 step 7).
    ImportOnly {
        added: BTreeSet<String>,
        removed: BTreeSet<String>,
    },
}

#[derive(Clone, Debug, Default)]
pub struct AtomFacts {
    pub adds_def: BTreeSet<Key>,
    pub removes_def: BTreeSet<Key>,
    pub modifies_def: BTreeSet<Key>,
    /// Strongly resolved references, net of what the atom removes.
    pub adds_ref: BTreeSet<Key>,
    pub removes_ref: BTreeSet<Key>,
    /// Names the atom adds or removes, net, including definitions it adds or removes (for
    /// F2, the rare-identifier edge). A name on both the removed and added lines is context,
    /// not a change, and does not count.
    pub names: BTreeSet<String>,
    /// Every identifier on the atom's changed lines, either side, net or not: a learned judge
    /// feature (DECISIONS.md D28) for edits that change types without adding or removing names.
    pub mentioned: BTreeSet<String>,
    /// Reference names the atom adds or removes, net (for the import post-pass).
    pub added_names: BTreeSet<String>,
    pub removed_names: BTreeSet<String>,
    /// Qualified name of the innermost definition present in both versions around the atom.
    pub enclosing: Option<String>,
    /// A definition this atom introduces, for the plan label.
    pub new_symbol: Option<String>,
    pub class: Class,
}

pub struct Analysis {
    pub atoms: Vec<AtomFacts>,
    pub base: Vec<Option<FileFacts>>,
    pub fin: Vec<Option<FileFacts>>,
    /// Files whose parse had errors (facts still used; the check is the backstop).
    pub parse_errors: Vec<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Version {
    Base,
    Final,
}

pub fn analyze(repo: &Repo, changes: &Changes) -> Result<Analysis> {
    let n = changes.files.len();
    let (mut base, mut fin): (Vec<Option<FileFacts>>, Vec<Option<FileFacts>>) =
        (vec![None; n], vec![None; n]);

    // Contents: text pairs are in memory; whole added or deleted files are loaded here.
    let mut to_load: Vec<(usize, Version, String)> = Vec::new();
    for (i, f) in changes.files.iter().enumerate() {
        let Some(adapter) = adapter_for(&f.path) else {
            continue;
        };
        if let Some(text) = &f.text {
            base[i] = adapter.analyze(&f.path, &text.base);
            fin[i] = adapter.analyze(&f.path, &text.new);
            continue;
        }
        for (version, entry) in [(Version::Base, &f.base), (Version::Final, &f.new)] {
            if let Some(e) = entry.as_ref().filter(|e| e.mode.is_regular()) {
                to_load.push((i, version, e.oid.clone()));
            }
        }
    }
    let oids: Vec<&str> = to_load.iter().map(|(_, _, oid)| oid.as_str()).collect();
    let sizes = repo.blob_sizes(&oids)?;
    let small: Vec<usize> = (0..to_load.len())
        .filter(|&k| sizes[k] <= MAX_PARSE_BYTES)
        .collect();
    let small_oids: Vec<&str> = small.iter().map(|&k| to_load[k].2.as_str()).collect();
    for (k, blob) in small.iter().zip(repo.read_blobs(&small_oids)?) {
        let (i, version, _) = &to_load[*k];
        let path = &changes.files[*i].path;
        let facts = adapter_for(path).and_then(|a| a.analyze(path, &blob));
        match version {
            Version::Base => base[*i] = facts,
            Version::Final => fin[*i] = facts,
        }
    }

    let parse_errors = (0..n)
        .filter(|&i| {
            base[i].as_ref().is_some_and(|f| f.has_error)
                || fin[i].as_ref().is_some_and(|f| f.has_error)
        })
        .collect();
    let ctx = Context::new(repo, changes, &base, &fin);
    let atoms = changes
        .atoms
        .iter()
        .map(|atom| ctx.atom_facts(atom.file, &atom.kind))
        .collect();
    Ok(Analysis {
        atoms,
        base,
        fin,
        parse_errors,
    })
}

struct Context<'a> {
    changes: &'a Changes,
    base: &'a [Option<FileFacts>],
    fin: &'a [Option<FileFacts>],
    /// Import resolution per (file, version, import index): the changed file it names.
    imports: HashMap<(usize, bool, usize), Option<usize>>,
    /// Top-level definition names per changed file, either version.
    top_names: Vec<BTreeSet<String>>,
    stoplist: BTreeSet<&'static str>,
    /// Every adapter's stop words, for name-only resolution (D45).
    any_stoplist: BTreeSet<&'static str>,
}

impl<'a> Context<'a> {
    fn new(
        repo: &Repo,
        changes: &'a Changes,
        base: &'a [Option<FileFacts>],
        fin: &'a [Option<FileFacts>],
    ) -> Self {
        let by_path: HashMap<&[u8], usize> = changes
            .files
            .iter()
            .enumerate()
            .map(|(i, f)| (f.path.as_slice(), i))
            .collect();
        let mut resolver = Resolver::new(&repo.toplevel);
        let mut imports = HashMap::new();
        for (i, f) in changes.files.iter().enumerate() {
            for (is_final, facts) in [(false, &base[i]), (true, &fin[i])] {
                let Some(facts) = facts else { continue };
                let exists = |p: &[u8]| match by_path.get(p) {
                    Some(&j) => {
                        let side = if is_final {
                            &changes.files[j].new
                        } else {
                            &changes.files[j].base
                        };
                        side.is_some()
                    }
                    None => repo.path_of(p).is_file(),
                };
                for (k, import) in facts.imports.iter().enumerate() {
                    let target = match resolver.resolve(&f.path, &import.source, &exists) {
                        Target::Path(p) => by_path.get(p.as_slice()).copied(),
                        Target::Unresolved => None,
                    };
                    imports.insert((i, is_final, k), target);
                }
            }
        }
        let top_names = (0..changes.files.len())
            .map(|i| {
                [&base[i], &fin[i]]
                    .into_iter()
                    .flatten()
                    .flat_map(|f| {
                        f.defs
                            .iter()
                            .filter(|d| d.top_level)
                            .map(|d| d.name.clone())
                    })
                    .collect()
            })
            .collect();
        let stoplist = adapter_for(b"x.ts")
            .map(|a| a.stoplist().iter().copied().collect())
            .unwrap_or_default();
        Self {
            changes,
            base,
            fin,
            imports,
            top_names,
            stoplist,
            any_stoplist: crate::lang::stoplist(),
        }
    }

    fn facts(&self, file: usize, version: Version) -> Option<&'a FileFacts> {
        match version {
            Version::Base => self.base[file].as_ref(),
            Version::Final => self.fin[file].as_ref(),
        }
    }

    fn usable(&self, name: &str) -> bool {
        name.len() >= 3 && !self.stoplist.contains(name)
    }

    fn def_key(&self, file: usize, facts: &FileFacts, def: usize) -> Key {
        let d = &facts.defs[def];
        if d.top_level {
            Key::Top(file, d.name.clone())
        } else {
            Key::Local(file, d.name.clone())
        }
    }

    /// Resolves a reference to a definition in a changed file, if it strongly names one.
    fn resolve(
        &self,
        file: usize,
        version: Version,
        name: &str,
        qualifier: Option<&str>,
    ) -> Option<Key> {
        let facts = self.facts(file, version)?;
        let is_final = version == Version::Final;
        let import_target = |k: usize| self.imports.get(&(file, is_final, k)).copied().flatten();
        let name_only =
            adapter_for(&self.changes.files[file].path).is_some_and(|a| a.name_only_resolution());
        if let Some(q) = qualifier {
            // D45: `module.name` resolves by name only when `module` is an imported module, never
            // for `self.x` or `obj.method`.
            if name_only {
                return facts
                    .imports
                    .iter()
                    .any(|i| i.local == q)
                    .then(|| self.by_name(file, name))
                    .flatten();
            }
            // `ns.name` with `import * as ns from './x'` names x's export `name`.
            let (k, _) = facts
                .imports
                .iter()
                .enumerate()
                .find(|(_, i)| i.local == q && i.imported == Imported::Namespace)?;
            let target = import_target(k)?;
            return self.top_names[target]
                .contains(name)
                .then(|| Key::Top(target, name.to_owned()));
        }
        if let Some(def) = facts.defs.iter().position(|d| d.name == name) {
            let key = if facts.defs.iter().any(|d| d.name == name && d.top_level) {
                Key::Top(file, name.to_owned())
            } else {
                self.def_key(file, facts, def)
            };
            return match &key {
                Key::Local(..) if !self.usable(name) => None,
                _ if name.len() < 3 => None,
                _ => Some(key),
            };
        }
        if name_only {
            // D45: no module resolution; an imported or unqualified name resolves to the one
            // changed file of the same language that defines it at top level.
            return self.by_name(file, name);
        }
        let (k, import) = facts
            .imports
            .iter()
            .enumerate()
            .find(|(_, i)| i.local == name)?;
        let target = import_target(k)?;
        let exported = match &import.imported {
            Imported::Named(original) => original.clone(),
            Imported::Default => self
                .facts(target, version)
                .and_then(|f| f.default_export.clone())?,
            Imported::Namespace => return None,
        };
        Some(Key::Top(target, exported))
    }

    /// D45: the single other changed file, in the same language, that defines `name` at top
    /// level. Common names and ambiguous ones resolve to nothing.
    fn by_name(&self, file: usize, name: &str) -> Option<Key> {
        if name.len() < 3 || self.any_stoplist.contains(name) {
            return None;
        }
        let ext = crate::lang::extension(&self.changes.files[file].path);
        let mut found = (0..self.changes.files.len()).filter(|&j| {
            j != file
                && crate::lang::extension(&self.changes.files[j].path) == ext
                && self.top_names[j].contains(name)
        });
        match (found.next(), found.next()) {
            (Some(target), None) => Some(Key::Top(target, name.to_owned())),
            _ => None,
        }
    }

    fn atom_facts(&self, file: usize, kind: &AtomKind) -> AtomFacts {
        let f = &self.changes.files[file];
        let (base, fin) = (
            self.facts(file, Version::Base),
            self.facts(file, Version::Final),
        );
        match kind {
            AtomKind::Region(r) => match (base, fin) {
                (Some(b), Some(n)) => self.region_facts(file, b, n, &r.base, &r.new),
                _ => symbolless(),
            },
            AtomKind::Whole(Whole::Added)
                if f.new.as_ref().is_some_and(|e| e.mode != Mode::LINK) =>
            {
                match fin {
                    Some(n) => {
                        self.region_facts(file, &FileFacts::default(), n, &(0..0), &(0..u32::MAX))
                    }
                    None => symbolless(),
                }
            }
            AtomKind::Whole(Whole::Deleted) => match base {
                Some(b) => {
                    self.region_facts(file, b, &FileFacts::default(), &(0..u32::MAX), &(0..0))
                }
                None => symbolless(),
            },
            _ => symbolless(),
        }
    }

    /// Facts for base lines `old` replaced by final lines `new` in `file`.
    fn region_facts(
        &self,
        file: usize,
        base: &FileFacts,
        fin: &FileFacts,
        old: &Range<u32>,
        new: &Range<u32>,
    ) -> AtomFacts {
        let mut facts = AtomFacts::default();
        let keys = |f: &FileFacts| -> BTreeSet<Key> {
            (0..f.defs.len())
                .map(|d| self.def_key(file, f, d))
                .collect()
        };
        let (base_keys, fin_keys) = (keys(base), keys(fin));

        // Definitions added, removed, and modified.
        for (d, def) in fin.defs.iter().enumerate() {
            let key = self.def_key(file, fin, d);
            if new.contains(&def.name_line) && !base_keys.contains(&key) {
                facts.new_symbol.get_or_insert_with(|| def.name.clone());
                facts.adds_def.insert(key);
            }
        }
        for (d, def) in base.defs.iter().enumerate() {
            let key = self.def_key(file, base, d);
            if old.contains(&def.name_line) && !fin_keys.contains(&key) {
                facts.removes_def.insert(key);
            }
        }
        for (side, range, other) in [(fin, new, &base_keys), (base, old, &fin_keys)] {
            for d in innermost_touched(side, range) {
                let key = self.def_key(file, side, d);
                if other.contains(&key) {
                    facts.modifies_def.insert(key);
                }
            }
        }

        // References, net: added minus removed.
        let mut strong: BTreeMap<Key, i32> = BTreeMap::new();
        let mut names: BTreeMap<String, i32> = BTreeMap::new();
        for (side, range, version, sign) in [
            (fin, new, Version::Final, 1),
            (base, old, Version::Base, -1),
        ] {
            for r in side.refs.iter().filter(|r| range.contains(&r.line)) {
                facts.mentioned.insert(r.name.clone());
                *names.entry(r.name.clone()).or_default() += sign;
                if let Some(key) = self.resolve(file, version, &r.name, r.qualifier.as_deref()) {
                    *strong.entry(key).or_default() += sign;
                }
            }
        }
        for (key, net) in strong {
            match net.cmp(&0) {
                std::cmp::Ordering::Greater => facts.adds_ref.insert(key),
                std::cmp::Ordering::Less => facts.removes_ref.insert(key),
                std::cmp::Ordering::Equal => false,
            };
        }
        for key in facts.adds_def.iter().chain(&facts.removes_def) {
            let (Key::Top(_, name) | Key::Local(_, name)) = key;
            facts.names.insert(name.clone());
        }
        for (name, net) in names {
            if net != 0 {
                facts.names.insert(name.clone());
            }
            match net.cmp(&0) {
                std::cmp::Ordering::Greater => facts.added_names.insert(name),
                std::cmp::Ordering::Less => facts.removed_names.insert(name),
                std::cmp::Ordering::Equal => false,
            };
        }

        facts.enclosing = enclosing(base, fin, old, new);
        facts.class = self.classify(file, base, fin, old, new, &facts);
        facts
    }

    fn classify(
        &self,
        file: usize,
        base: &FileFacts,
        fin: &FileFacts,
        old: &Range<u32>,
        new: &Range<u32>,
        facts: &AtomFacts,
    ) -> Class {
        let Some(text) = &self.changes.files[file].text else {
            // Whole added or deleted files.
            let empty =
                facts.adds_def.is_empty() && facts.removes_def.is_empty() && facts.names.is_empty();
            return if empty {
                Class::Symbolless
            } else {
                Class::Normal
            };
        };
        let removed = text.base_lines(old.clone());
        let added = text.new_lines(new.clone());
        let blank = |line: &[u8]| line.iter().all(u8::is_ascii_whitespace);

        // Import-only: every non-blank changed line is inside an import statement.
        let in_imports = |lines: &[(u32, &[u8])], ranges: &[Range<u32>]| {
            lines
                .iter()
                .all(|(n, l)| blank(l) || ranges.iter().any(|r| r.contains(n)))
        };
        let nonblank = removed.iter().chain(&added).any(|(_, l)| !blank(l));
        if nonblank
            && in_imports(&removed, &base.import_lines)
            && in_imports(&added, &fin.import_lines)
        {
            let locals = |f: &FileFacts, range: &Range<u32>| -> BTreeSet<String> {
                f.imports
                    .iter()
                    .filter(|i| i.lines.start < range.end && range.start < i.lines.end)
                    .map(|i| i.local.clone())
                    .collect()
            };
            let (was, now) = (locals(base, old), locals(fin, new));
            return Class::ImportOnly {
                added: now.difference(&was).cloned().collect(),
                removed: was.difference(&now).cloned().collect(),
            };
        }

        // Style: the same bytes once whitespace is ignored (TypeScript is not
        // whitespace-significant).
        let squash = |lines: &[(u32, &[u8])]| -> Vec<u8> {
            lines
                .iter()
                .flat_map(|(_, l)| l.iter().copied())
                .filter(|b| !b.is_ascii_whitespace())
                .collect()
        };
        if squash(&removed) == squash(&added) {
            return Class::Style;
        }

        // Debug: only adds lines, all inside debug statements that lie within the atom. Logging
        // in an error path is error reporting, not debugging (D42).
        let in_error_path = fin
            .debug
            .iter()
            .any(|s| s.error_path && new.start <= s.lines.start && s.lines.end <= new.end);
        if old.is_empty() && !in_error_path {
            let stmts: Vec<&DebugKind> = fin
                .debug
                .iter()
                .filter(|s| new.start <= s.lines.start && s.lines.end <= new.end)
                .filter(|s| match &s.kind {
                    DebugKind::Debugger => true,
                    DebugKind::Console(m) => DEBUG_CONSOLE.contains(&m.as_str()),
                })
                .map(|s| &s.kind)
                .collect();
            let covered = added.iter().all(|(n, l)| {
                blank(l)
                    || fin.debug.iter().any(|s| {
                        s.lines.contains(n)
                            && match &s.kind {
                                DebugKind::Debugger => true,
                                DebugKind::Console(m) => DEBUG_CONSOLE.contains(&m.as_str()),
                            }
                    })
            });
            if !stmts.is_empty() && covered {
                let leave = stmts.iter().all(|k| match k {
                    DebugKind::Debugger => true,
                    DebugKind::Console(m) => !base.console_methods.contains(m),
                });
                return Class::Debug { leave };
            }
        }

        let empty = facts.adds_def.is_empty()
            && facts.removes_def.is_empty()
            && facts.modifies_def.is_empty()
            && facts.names.is_empty()
            && facts.enclosing.is_none();
        if empty {
            Class::Symbolless
        } else {
            Class::Normal
        }
    }
}

fn symbolless() -> AtomFacts {
    AtomFacts {
        class: Class::Symbolless,
        ..AtomFacts::default()
    }
}

/// Does a changed line range touch a definition? An empty range (a pure insertion or
/// deletion point) touches only definitions it falls strictly inside.
fn touches(range: &Range<u32>, def: &Range<u32>) -> bool {
    if range.is_empty() {
        def.start < range.start && range.start < def.end
    } else {
        range.start < def.end && def.start < range.end
    }
}

/// Touched definitions with no touched definition nested inside them.
fn innermost_touched(facts: &FileFacts, range: &Range<u32>) -> Vec<usize> {
    let touched: Vec<usize> = (0..facts.defs.len())
        .filter(|&d| touches(range, &facts.defs[d].lines))
        .collect();
    touched
        .iter()
        .copied()
        .filter(|&d| !touched.iter().any(|&t| t != d && is_ancestor(facts, d, t)))
        .collect()
}

fn is_ancestor(facts: &FileFacts, ancestor: usize, mut d: usize) -> bool {
    while let Some(p) = facts.defs[d].parent {
        if p == ancestor {
            return true;
        }
        d = p;
    }
    false
}

fn qualified(facts: &FileFacts, mut d: usize) -> String {
    let mut parts = vec![facts.defs[d].name.as_str()];
    while let Some(p) = facts.defs[d].parent {
        parts.push(&facts.defs[p].name);
        d = p;
    }
    parts.reverse();
    parts.join(".")
}

/// The innermost definition, present in both versions under the same qualified name, that
/// contains the atom (§6.2).
fn enclosing(
    base: &FileFacts,
    fin: &FileFacts,
    old: &Range<u32>,
    new: &Range<u32>,
) -> Option<String> {
    let (side, range, other) = if new.is_empty() {
        (base, old, fin)
    } else {
        (fin, new, base)
    };
    let other_names: BTreeSet<String> =
        (0..other.defs.len()).map(|d| qualified(other, d)).collect();
    (0..side.defs.len())
        .filter(|&d| {
            let def = &side.defs[d];
            // One-line variables and fields are too narrow to group by; multi-line ones
            // (arrow-function components, config objects) are real containers.
            let container =
                !matches!(def.kind, DefKind::Variable | DefKind::Field) || def.lines.len() > 1;
            container && def.lines.start <= range.start && range.end <= def.lines.end
        })
        .map(|d| qualified(side, d))
        .filter(|q| other_names.contains(q))
        .max_by_key(|q| q.matches('.').count())
}
