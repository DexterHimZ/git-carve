//! The code graph (SPEC §6.3–§6.5): hard binds, soft affinities, order constraints,
//! components, strongly connected component merging, a deterministic topological order,
//! and the post-passes that place import-only, debug, style, and symbol-less atoms.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::atom::{AtomId, AtomKind, BindReason, Changes};
use crate::facts::{Analysis, Class};
use crate::group::UnionFind;
use crate::judge::{LearnedJudge, Merge};
use crate::lang::primary;

/// F2: an identifier linking at most this many atoms is rare enough to mean something.
const RARE_MAX_ATOMS: usize = 3;
const RARE_MIN_LEN: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum Tag {
    /// Introduces a definition.
    New,
    /// Half of a rename pair.
    Rename,
    /// Left out: debug output (DECISIONS.md D8).
    DebugOnly,
    /// Kept, but looks like debug output; the user may want to move it to leave.
    LooksLikeDebug,
    /// Whitespace only.
    Style,
    /// Left out: the working tree does not pass the check with it (DECISIONS.md D40).
    FailsCheck,
    /// In its group only because it is in the same file as the rest (DECISIONS.md D41).
    SameFile,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Diagnostics {
    pub hard_binds: usize,
    pub soft_edges: usize,
    /// Soft edges per rule (F0–F3); a pair linked by two rules counts under both.
    pub soft_edges_by_kind: BTreeMap<String, usize>,
    pub order_edges: usize,
    /// Groups fused because their order constraints formed a cycle.
    pub cycle_merges: usize,
    /// Import-only atoms whose new names are used by two or more groups (DECISIONS.md D5).
    pub bridging_import_atoms: Vec<AtomId>,
    /// Groups made only of symbol-less atoms with no stem sibling (DECISIONS.md D9).
    pub orphan_groups: usize,
    pub parse_errors: Vec<String>,
    /// The judge that decided soft merges (`learned:<weights hash>`); absent in code-only mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub judge: Option<String>,
    /// Each judge merge in order: both sides' atoms, the score, the top features.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub judge_merges: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Commit order. Every atom appears exactly once across `groups` and `leave` (R4).
    pub groups: Vec<Vec<AtomId>>,
    pub leave: Vec<AtomId>,
    /// Index of the trailing whitespace-only group, if any.
    pub style: Option<usize>,
    pub tags: BTreeMap<AtomId, Vec<Tag>>,
    /// Every soft edge with the rules that produced it, for analysis (`--json`).
    pub edges: Vec<(AtomId, AtomId, Vec<&'static str>)>,
    /// Pairs that must share a commit, with the reason (§6.3 B1–B4, renames, swaps).
    pub hard: Vec<(AtomId, AtomId, &'static str)>,
    pub diagnostics: Diagnostics,
    /// Judge mode (DECISIONS.md D30): `#` notes on the least certain boundaries, by group
    /// index, at most [`UNSURE_MAX`]. Empty in code-only mode.
    pub unsure: Vec<(usize, String)>,
}

/// D30: a judge merge within this margin of θ, made or not, is flagged.
pub const UNSURE_MARGIN: f64 = 0.1;
pub const UNSURE_MAX: usize = 3;

/// What a component is, which decides where it goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Role {
    Core,
    Symbolless,
    Import,
    Debug,
    Style,
    Leave,
}

/// How soft merges are decided.
pub enum Soft<'a> {
    /// Code-only: every soft edge merges.
    Code,
    /// A judge's merge sequence ([`judge_sequence`]), applied until the first merge scoring
    /// below θ (DECISIONS.md D28, D38).
    Replay {
        judge: &'a str,
        merges: &'a [Merge],
        theta: f64,
    },
}

pub fn plan(changes: &Changes, analysis: &Analysis) -> Plan {
    plan_soft(changes, analysis, Soft::Code)
}

/// [`plan`], with the learned judge deciding soft merges instead of code-only unions
/// (DECISIONS.md D28) when one is given.
pub fn plan_with(changes: &Changes, analysis: &Analysis, judge: Option<&LearnedJudge>) -> Plan {
    match judge {
        None => plan(changes, analysis),
        Some(j) => {
            // Below θ only to find D30's near miss; the applied prefix is the same.
            let merges = judge_sequence(changes, analysis, j, j.theta - UNSURE_MARGIN);
            plan_soft(
                changes,
                analysis,
                Soft::Replay {
                    judge: &j.name,
                    merges: &merges,
                    theta: j.theta,
                },
            )
        }
    }
}

/// The judge's merges over this tree's hard-bound components, down to `min_theta`.
pub fn judge_sequence(
    changes: &Changes,
    analysis: &Analysis,
    judge: &LearnedJudge,
    min_theta: f64,
) -> Vec<Merge> {
    let e = edges(changes, analysis);
    let mut sets = UnionFind::new(changes.atoms.len());
    for &(a, b, _) in &e.hard {
        sets.union(a, b);
    }
    judge.sequence(changes, analysis, &e, sets, min_theta)
}

pub fn plan_soft(changes: &Changes, analysis: &Analysis, soft_mode: Soft) -> Plan {
    let n = changes.atoms.len();
    let facts = &analysis.atoms;
    let mut diagnostics = Diagnostics {
        parse_errors: analysis
            .parse_errors
            .iter()
            .map(|&f| crate::model::show(&changes.files[f].path))
            .collect(),
        judge: match &soft_mode {
            Soft::Code => None,
            Soft::Replay { judge, .. } => Some((*judge).to_owned()),
        },
        ..Diagnostics::default()
    };

    let Edges { hard, soft } = edges(changes, analysis);
    let mut sets = UnionFind::new(n);
    for &(a, b, _) in &hard {
        if sets.find(a) != sets.find(b) {
            diagnostics.hard_binds += 1;
        }
        sets.union(a, b);
    }

    diagnostics.soft_edges = soft.len();
    for kinds in soft.values() {
        for kind in kinds {
            *diagnostics
                .soft_edges_by_kind
                .entry((*kind).to_owned())
                .or_default() += 1;
        }
    }
    // Every soft edge merges in code-only mode. (D11 had F2 not merging; its own re-validation
    // rule reversed that at M5, DECISIONS.md D26.) The judge may veto them in M6.
    let mut replayed: Option<(&[Merge], usize, f64)> = None;
    match soft_mode {
        Soft::Replay { merges, theta, .. } => {
            let applied = merges.iter().take_while(|m| m.p >= theta).count();
            for m in &merges[..applied] {
                sets.union(m.a, m.b);
                diagnostics.judge_merges.push(m.why.clone());
            }
            replayed = Some((merges, applied, theta));
        }
        Soft::Code => {
            for &(a, b) in soft.keys() {
                sets.union(a, b);
            }
        }
    }
    let soft_list = soft
        .iter()
        .map(|(&(a, b), kinds)| {
            (
                changes.atoms[a].id,
                changes.atoms[b].id,
                kinds.iter().copied().collect(),
            )
        })
        .collect();

    // Components, in order of their smallest atom.
    let mut component_of = vec![usize::MAX; n];
    let mut components: Vec<Vec<usize>> = Vec::new();
    for i in 0..n {
        let root = sets.find(i);
        if component_of[root] == usize::MAX {
            component_of[root] = components.len();
            components.push(Vec::new());
        }
        component_of[i] = component_of[root];
        components[component_of[i]].push(i);
    }
    let role = |atoms: &[usize]| -> Role {
        let leaves = atoms.iter().all(|&i| {
            changes.atoms[i].leave_by_default
                || matches!(facts[i].class, Class::Debug { leave: true })
        });
        if leaves {
            return Role::Leave;
        }
        let classes: BTreeSet<Role> = atoms
            .iter()
            .map(|&i| match facts[i].class {
                Class::Normal => Role::Core,
                Class::Symbolless => Role::Symbolless,
                Class::ImportOnly { .. } => Role::Import,
                Class::Debug { .. } => Role::Debug,
                Class::Style => Role::Style,
            })
            .collect();
        if classes.contains(&Role::Core) {
            Role::Core
        } else if classes.len() == 1 {
            *classes.first().unwrap()
        } else {
            // A hard-bound mix of placeable atoms: place it like a symbol-less change.
            Role::Symbolless
        }
    };
    let roles: Vec<Role> = components.iter().map(|c| role(c)).collect();

    // Order constraints between core components (O1, O2), then cycle merging.
    let core: Vec<usize> = (0..components.len())
        .filter(|&c| roles[c] == Role::Core)
        .collect();
    let mut edges: BTreeSet<(usize, usize)> = BTreeSet::new();
    for &ca in &core {
        for &cb in &core {
            if ca == cb {
                continue;
            }
            let before = components[ca].iter().any(|&a| {
                components[cb].iter().any(|&b| {
                    // O1: a definition comes no later than its uses.
                    facts[a].adds_def.intersection(&facts[b].adds_ref).next().is_some()
                    // O2: removed uses go no later than the removed definition.
                        || facts[b].removes_def.intersection(&facts[a].removes_ref).next().is_some()
                })
            });
            if before {
                edges.insert((ca, cb));
            }
        }
    }
    diagnostics.order_edges = edges.len();
    let scc = tarjan(components.len(), &edges);
    let mut merged: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for &c in &core {
        merged.entry(scc[c]).or_default().push(c);
    }
    diagnostics.cycle_merges = merged.values().map(|v| v.len() - 1).sum();

    // Nodes: merged core groups, as atom lists.
    let mut nodes: Vec<Vec<usize>> = merged
        .values()
        .map(|cs| {
            let mut atoms: Vec<usize> = cs
                .iter()
                .flat_map(|&c| components[c].iter().copied())
                .collect();
            atoms.sort_unstable();
            atoms
        })
        .collect();
    let node_of_component: BTreeMap<usize, usize> = merged
        .values()
        .enumerate()
        .flat_map(|(node, cs)| cs.iter().map(move |&c| (c, node)))
        .collect();

    // D9: symbol-less components join the group with the most atoms from stem siblings.
    for (c, atoms) in components
        .iter()
        .enumerate()
        .filter(|(c, _)| roles[*c] == Role::Symbolless)
    {
        let stems: BTreeSet<(Vec<u8>, Vec<u8>)> = atoms
            .iter()
            .map(|&i| stem_key(&changes.files[changes.atoms[i].file].path))
            .collect();
        let best = (0..nodes.len())
            .map(|node| {
                let count = nodes[node]
                    .iter()
                    .filter(|&&i| {
                        stems.contains(&stem_key(&changes.files[changes.atoms[i].file].path))
                    })
                    .count();
                (count, std::cmp::Reverse(nodes[node][0]), node)
            })
            .filter(|(count, ..)| *count > 0)
            .max();
        match best {
            Some((_, _, node)) => {
                nodes[node].extend(atoms);
                nodes[node].sort_unstable();
            }
            None => {
                diagnostics.orphan_groups += 1;
                nodes.push(atoms.clone());
            }
        }
        let _ = c;
    }

    // Topological order; among ready groups, the one with the smallest atom goes first.
    let node_edges: BTreeSet<(usize, usize)> = edges
        .iter()
        .filter_map(|&(a, b)| {
            let (na, nb) = (node_of_component.get(&a)?, node_of_component.get(&b)?);
            (na != nb).then_some((*na, *nb))
        })
        .collect();
    let order = topo(&nodes, &node_edges);
    let mut groups: Vec<Vec<usize>> = order.into_iter().map(|node| nodes[node].clone()).collect();

    // Import post-pass (§6.4 step 7), then debug and style placement.
    let line_of = |i: usize| match &changes.atoms[i].kind {
        AtomKind::Region(r) => {
            if r.new.is_empty() {
                r.base.start
            } else {
                r.new.start
            }
        }
        _ => 0,
    };
    let nearest_in_file = |groups: &Vec<Vec<usize>>, i: usize| -> Option<usize> {
        let file = changes.atoms[i].file;
        (0..groups.len())
            .filter_map(|g| {
                groups[g]
                    .iter()
                    .filter(|&&j| changes.atoms[j].file == file)
                    .map(|&j| line_of(j).abs_diff(line_of(i)))
                    .min()
                    .map(|d| (d, g))
            })
            .min()
            .map(|(_, g)| g)
    };
    let mut tail: Vec<Vec<usize>> = Vec::new();
    let mut style: Vec<usize> = Vec::new();
    let mut leave: Vec<usize> = Vec::new();
    let mut tags: BTreeMap<AtomId, Vec<Tag>> = BTreeMap::new();
    for (c, atoms) in components.iter().enumerate() {
        match roles[c] {
            Role::Import => {
                let (mut added, mut removed) = (BTreeSet::new(), BTreeSet::new());
                for &i in atoms {
                    if let Class::ImportOnly {
                        added: a,
                        removed: r,
                    } = &facts[i].class
                    {
                        added.extend(a.iter().cloned());
                        removed.extend(r.iter().cloned());
                    }
                }
                let file = changes.atoms[atoms[0]].file;
                let users = |names: &BTreeSet<String>, added_side: bool| -> Vec<usize> {
                    (0..groups.len())
                        .filter(|&g| {
                            groups[g].iter().any(|&j| {
                                let f = &facts[j];
                                let used = if added_side {
                                    &f.added_names
                                } else {
                                    &f.removed_names
                                };
                                changes.atoms[j].file == file && !used.is_disjoint(names)
                            })
                        })
                        .collect()
                };
                let adders = users(&added, true);
                if adders.len() >= 2 {
                    diagnostics
                        .bridging_import_atoms
                        .extend(atoms.iter().map(|&i| changes.atoms[i].id));
                }
                let target = adders
                    .first()
                    .copied()
                    .or_else(|| users(&removed, false).last().copied())
                    .or_else(|| nearest_in_file(&groups, atoms[0]));
                match target {
                    Some(g) => groups[g].extend(atoms),
                    None => tail.push(atoms.clone()),
                }
            }
            Role::Debug => {
                for &i in atoms {
                    tags.entry(changes.atoms[i].id)
                        .or_default()
                        .push(Tag::LooksLikeDebug);
                }
                match nearest_in_file(&groups, atoms[0]) {
                    Some(g) => groups[g].extend(atoms),
                    None => tail.push(atoms.clone()),
                }
            }
            Role::Style => style.extend(atoms),
            Role::Leave => leave.extend(atoms),
            Role::Core | Role::Symbolless => {}
        }
    }
    groups.extend(tail);
    groups.retain(|g| !g.is_empty());
    let style_index = (!style.is_empty()).then(|| {
        groups.push(style.clone());
        groups.len() - 1
    });

    for (i, f) in facts.iter().enumerate() {
        let id = changes.atoms[i].id;
        if f.new_symbol.is_some() {
            tags.entry(id).or_default().push(Tag::New);
        }
        match f.class {
            Class::Debug { leave: true } if leave.contains(&i) => {
                tags.entry(id).or_default().push(Tag::DebugOnly)
            }
            Class::Style => tags.entry(id).or_default().push(Tag::Style),
            _ => {}
        }
    }
    for b in changes
        .binds
        .iter()
        .filter(|b| b.reason == BindReason::Rename)
    {
        for id in [b.a, b.b] {
            tags.entry(id).or_default().push(Tag::Rename);
        }
    }

    let ids = |atoms: &[usize]| -> Vec<AtomId> {
        let mut ids: Vec<AtomId> = atoms.iter().map(|&i| changes.atoms[i].id).collect();
        ids.sort_unstable();
        ids
    };
    // D41: atoms whose place in their group rests on the same-file rule alone.
    if replayed.is_none() {
        let mut without = UnionFind::new(n);
        for &(a, b, _) in &hard {
            without.union(a, b);
        }
        for (&(a, b), kinds) in &soft {
            if kinds.iter().any(|k| *k != "F7") {
                without.union(a, b);
            }
        }
        for group in &groups {
            let mut parts: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
            for &i in group {
                parts.entry(without.find(i)).or_default().push(i);
            }
            if parts.len() < 2 {
                continue;
            }
            let anchor = parts
                .iter()
                .max_by_key(|(root, atoms)| (atoms.len(), std::cmp::Reverse(**root)))
                .map(|(root, _)| *root);
            for (root, atoms) in &parts {
                let joined_by_file = atoms.iter().any(|&i| {
                    soft.iter()
                        .any(|(&(a, b), kinds)| kinds.contains("F7") && (a == i || b == i))
                });
                if Some(*root) != anchor && joined_by_file {
                    for &i in atoms {
                        tags.entry(changes.atoms[i].id)
                            .or_default()
                            .push(Tag::SameFile);
                    }
                }
            }
        }
    }
    let unsure = replayed.map_or_else(Vec::new, |(merges, applied, theta)| {
        unsure_notes(changes, &groups, merges, applied, theta)
    });
    Plan {
        groups: groups.iter().map(|g| ids(g)).collect(),
        leave: ids(&leave),
        style: style_index,
        tags,
        edges: soft_list,
        hard: hard
            .iter()
            .map(|&(a, b, why)| (changes.atoms[a].id, changes.atoms[b].id, why))
            .collect(),
        diagnostics,
        unsure,
    }
}

/// D30: notes on the judge merges closest to θ. A merge made just above θ may have joined
/// an odd one out; the first merge not made, just below θ, may be a missed join.
fn unsure_notes(
    changes: &Changes,
    groups: &[Vec<usize>],
    merges: &[Merge],
    applied: usize,
    theta: f64,
) -> Vec<(usize, String)> {
    let group_of: BTreeMap<usize, usize> = groups
        .iter()
        .enumerate()
        .flat_map(|(g, atoms)| atoms.iter().map(move |&a| (a, g)))
        .collect();
    let ids = |atoms: &[usize]| -> String {
        let mut v: Vec<AtomId> = atoms.iter().map(|&i| changes.atoms[i].id).collect();
        v.sort_unstable();
        v.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut notes: Vec<(f64, usize, String)> = Vec::new();
    for m in merges[..applied]
        .iter()
        .filter(|m| m.p < theta + UNSURE_MARGIN)
    {
        if let Some(&g) = group_of.get(&m.a) {
            notes.push((
                m.p - theta,
                g,
                format!(
                    "# unsure: {} joined this commit at {:.2} (threshold {theta:.2}), on {}",
                    ids(&m.smaller),
                    m.p,
                    m.top.join(", ")
                ),
            ));
        }
    }
    if let Some(m) = merges.get(applied).filter(|m| m.p >= theta - UNSURE_MARGIN) {
        if let (Some(&ga), Some(&gb)) = (group_of.get(&m.a), group_of.get(&m.b)) {
            if ga != gb {
                for (here, there) in [(ga, gb), (gb, ga)] {
                    notes.push((
                        theta - m.p,
                        here,
                        format!(
                            "# unsure: this commit may belong with commit {} ({:.2}, threshold {theta:.2}), on {}",
                            there + 1,
                            m.p,
                            m.top.join(", ")
                        ),
                    ));
                }
            }
        }
    }
    notes.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    notes.truncate(UNSURE_MAX);
    notes.sort_by_key(|n| n.1);
    notes.into_iter().map(|(_, g, text)| (g, text)).collect()
}

/// Hard binds and soft affinities between atoms (atom indices), before any merging.
pub struct Edges {
    /// Pairs that must share a commit, with the reason.
    pub hard: Vec<(usize, usize, &'static str)>,
    /// Soft affinities with the rules that produced them (F0–F3, F6).
    pub soft: BTreeMap<(usize, usize), BTreeSet<&'static str>>,
}

/// Every hard bind (B1–B4, renames, directory/file swaps) and soft affinity (§6.3).
pub fn edges(changes: &Changes, analysis: &Analysis) -> Edges {
    let n = changes.atoms.len();
    let facts = &analysis.atoms;
    // Hard binds: M1's structural binds (B3 renames, directory/file swaps), then B1, B2, B4.
    // Each is recorded with its reason, so the plan buffer can refuse to split it (§9.4).
    let mut hard: Vec<(usize, usize, &'static str)> = Vec::new();
    for b in &changes.binds {
        let reason = match b.reason {
            BindReason::Rename => "rename",
            BindReason::DirectoryFile => "a file and a directory trade places",
        };
        hard.push((b.a.0 as usize - 1, b.b.0 as usize - 1, reason));
    }
    for (file, atoms) in changes.file_atoms.iter().enumerate() {
        // B4: a mode change travels with the file's content changes.
        if atoms
            .iter()
            .any(|&i| matches!(changes.atoms[i].kind, AtomKind::Mode { .. }))
        {
            for w in atoms.windows(2) {
                hard.push((w[0], w[1], "a mode change travels with its file's content"));
            }
        }
        // B1: atoms overlapping the same newly added definition.
        if let (Some(fin), Some(base)) = (&analysis.fin[file], &analysis.base[file]) {
            let base_names: BTreeSet<(&str, bool)> = base
                .defs
                .iter()
                .map(|d| (d.name.as_str(), d.top_level))
                .collect();
            for def in fin
                .defs
                .iter()
                .filter(|d| !base_names.contains(&(d.name.as_str(), d.top_level)))
            {
                let overlapping: Vec<usize> = atoms
                    .iter()
                    .copied()
                    .filter(|&i| match &changes.atoms[i].kind {
                        AtomKind::Region(r) => {
                            !r.new.is_empty()
                                && r.new.start < def.lines.end
                                && def.lines.start < r.new.end
                        }
                        _ => false,
                    })
                    .collect();
                for w in overlapping.windows(2) {
                    hard.push((w[0], w[1], "parts of one new definition"));
                }
            }
        }
    }
    // B2: a lockfile travels with every changed manifest in its directory tree.
    for (lock_file, lock) in changes.files.iter().enumerate() {
        let (dir, name) = split_path(&lock.path);
        for &(manifest, lockfile) in primary().manifest_lock_pairs() {
            if name != lockfile.as_bytes() {
                continue;
            }
            for (m, f) in changes.files.iter().enumerate() {
                let (mdir, mname) = split_path(&f.path);
                if mname == manifest.as_bytes()
                    && (mdir == dir || dir.is_empty() || mdir.starts_with(&[dir, b"/"].concat()))
                {
                    let all: Vec<usize> = changes.file_atoms[lock_file]
                        .iter()
                        .chain(&changes.file_atoms[m])
                        .copied()
                        .collect();
                    for w in all.windows(2) {
                        hard.push((w[0], w[1], "a manifest travels with its lockfile"));
                    }
                }
            }
        }
    }
    // Soft affinities (code-only mode unions them), among atoms that take part in edges.
    let joins = |i: usize| matches!(facts[i].class, Class::Normal);
    // Dev-only ablation knob for the eval harness: `GIT_CARVE_DEV_DISABLE_EDGES=F1,F2`.
    let disabled: BTreeSet<String> = std::env::var("GIT_CARVE_DEV_DISABLE_EDGES")
        .unwrap_or_default()
        .split(',')
        .map(|k| k.trim().to_owned())
        .collect();
    let mut soft: BTreeMap<(usize, usize), BTreeSet<&'static str>> = BTreeMap::new();
    let mut add = |a: usize, b: usize, kind: &'static str| {
        if a != b && joins(a) && joins(b) && !disabled.contains(kind) {
            soft.entry((a.min(b), a.max(b))).or_default().insert(kind);
        }
    };
    for a in 0..n {
        for b in 0..n {
            if a == b {
                continue;
            }
            // F0: a new definition and its strongly resolved uses (no rarity cap, D10).
            if facts[a]
                .adds_def
                .intersection(&facts[b].adds_ref)
                .next()
                .is_some()
            {
                add(a, b, "F0");
            }
            // F3: a modified definition and references to it added or removed elsewhere.
            if facts[a]
                .modifies_def
                .iter()
                .any(|k| facts[b].adds_ref.contains(k) || facts[b].removes_ref.contains(k))
            {
                add(a, b, "F3");
            }
        }
    }
    // F6: systematic edits (DECISIONS.md D26). Atoms making the same change (identical text,
    // e.g. a generated mirror) or the same small token substitution (e.g. a rename) link.
    let systematic = |i: usize| matches!(facts[i].class, Class::Normal | Class::Symbolless);
    let mut by_signature: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, atom) in changes
        .atoms
        .iter()
        .enumerate()
        .filter(|(i, _)| systematic(*i))
    {
        for key in signatures(changes, atom) {
            by_signature.entry(key).or_default().push(i);
        }
    }
    let mut systematic_pairs: Vec<(usize, usize)> = Vec::new();
    for atoms in by_signature.values().filter(|v| v.len() >= 2) {
        for w in atoms.windows(2) {
            if w[0] != w[1] && !disabled.contains("F6") {
                systematic_pairs.push((w[0].min(w[1]), w[0].max(w[1])));
            }
        }
    }
    // F1: same enclosing definition in the same file.
    let mut by_enclosing: BTreeMap<(usize, &str), Vec<usize>> = BTreeMap::new();
    for (i, f) in facts.iter().enumerate() {
        if let Some(e) = &f.enclosing {
            by_enclosing
                .entry((changes.atoms[i].file, e))
                .or_default()
                .push(i);
        }
    }
    for atoms in by_enclosing.values() {
        for (k, &a) in atoms.iter().enumerate() {
            for &b in &atoms[k + 1..] {
                add(a, b, "F1");
            }
        }
    }
    // F2: a shared rare identifier.
    let stoplist: BTreeSet<&str> = primary().stoplist().iter().copied().collect();
    let mut by_name: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, f) in facts.iter().enumerate().filter(|(i, _)| joins(*i)) {
        for name in &f.names {
            by_name.entry(name).or_default().push(i);
        }
    }
    for (name, atoms) in &by_name {
        if name.len() >= RARE_MIN_LEN && !stoplist.contains(name) && atoms.len() <= RARE_MAX_ATOMS {
            for (k, &a) in atoms.iter().enumerate() {
                for &b in &atoms[k + 1..] {
                    add(a, b, "F2");
                }
            }
        }
    }
    for pair in systematic_pairs {
        soft.entry(pair).or_default().insert("F6");
    }
    // F7: edits in the same file (DECISIONS.md D41). On the train repositories, components
    // code kept apart in one file belonged together 93% of the time. A drive-by shape (a
    // small modification that defines nothing and is the only change in its function) never
    // joins by file alone (SPEC §12 M2).
    let mut per_scope: BTreeMap<(usize, Option<&str>), usize> = BTreeMap::new();
    for (i, f) in facts.iter().enumerate() {
        *per_scope
            .entry((changes.atoms[i].file, f.enclosing.as_deref()))
            .or_default() += 1;
    }
    let drive_by = |i: usize| match &changes.atoms[i].kind {
        AtomKind::Region(r) => {
            let (b, a) = (r.base.end - r.base.start, r.new.end - r.new.start);
            b > 0
                && b <= DRIVE_BY_MAX_LINES
                && a <= DRIVE_BY_MAX_LINES
                && facts[i].adds_def.is_empty()
                && per_scope[&(changes.atoms[i].file, facts[i].enclosing.as_deref())] == 1
        }
        _ => false,
    };
    if !disabled.contains("F7") {
        for atoms in &changes.file_atoms {
            let eligible: Vec<usize> = atoms
                .iter()
                .copied()
                .filter(|&i| joins(i) && !drive_by(i))
                .collect();
            for w in eligible.windows(2) {
                soft.entry((w[0].min(w[1]), w[0].max(w[1])))
                    .or_default()
                    .insert("F7");
            }
        }
    }
    Edges { hard, soft }
}

/// `(directory without __tests__/__snapshots__/__mocks__, name up to the first dot)`.
fn stem_key(path: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let (dir, name) = split_path(path);
    let mut dir = dir.to_vec();
    for special in [&b"__tests__"[..], b"__snapshots__", b"__mocks__"] {
        if let Some(stripped) = dir.strip_suffix(special) {
            dir = stripped.strip_suffix(b"/").unwrap_or(stripped).to_vec();
        }
    }
    let stem = name.split(|&b| b == b'.').next().unwrap_or(name).to_vec();
    (dir, stem)
}

fn split_path(path: &[u8]) -> (&[u8], &[u8]) {
    match path.iter().rposition(|&b| b == b'/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => (&[], path),
    }
}

/// D41: a modification at most this many lines long on each side can be a drive-by fix.
const DRIVE_BY_MAX_LINES: u32 = 3;

/// Strongly connected component id per node (Tarjan). Ids are the smallest member index,
/// so the result does not depend on traversal order.
fn tarjan(n: usize, edges: &BTreeSet<(usize, usize)>) -> Vec<usize> {
    let mut adj = vec![Vec::new(); n];
    for &(a, b) in edges {
        adj[a].push(b);
    }
    struct State {
        index: Vec<Option<usize>>,
        low: Vec<usize>,
        on_stack: Vec<bool>,
        stack: Vec<usize>,
        next: usize,
        comp: Vec<usize>,
    }
    fn strongconnect(v: usize, adj: &[Vec<usize>], s: &mut State) {
        s.index[v] = Some(s.next);
        s.low[v] = s.next;
        s.next += 1;
        s.stack.push(v);
        s.on_stack[v] = true;
        for &w in &adj[v] {
            match s.index[w] {
                None => {
                    strongconnect(w, adj, s);
                    s.low[v] = s.low[v].min(s.low[w]);
                }
                Some(iw) if s.on_stack[w] => s.low[v] = s.low[v].min(iw),
                _ => {}
            }
        }
        if Some(s.low[v]) == s.index[v] {
            let mut members = Vec::new();
            loop {
                let w = s.stack.pop().expect("stack holds the component");
                s.on_stack[w] = false;
                members.push(w);
                if w == v {
                    break;
                }
            }
            let id = *members.iter().min().unwrap();
            for w in members {
                s.comp[w] = id;
            }
        }
    }
    let mut s = State {
        index: vec![None; n],
        low: vec![0; n],
        on_stack: vec![false; n],
        stack: Vec::new(),
        next: 0,
        comp: (0..n).collect(),
    };
    for v in 0..n {
        if s.index[v].is_none() {
            strongconnect(v, &adj, &mut s);
        }
    }
    s.comp
}

/// Kahn's algorithm over nodes; ties go to the node with the smallest first atom.
fn topo(nodes: &[Vec<usize>], edges: &BTreeSet<(usize, usize)>) -> Vec<usize> {
    let mut indegree = vec![0usize; nodes.len()];
    for &(_, b) in edges {
        indegree[b] += 1;
    }
    let mut ready: BTreeSet<(usize, usize)> = (0..nodes.len())
        .filter(|&v| indegree[v] == 0)
        .map(|v| (nodes[v][0], v))
        .collect();
    let mut order = Vec::with_capacity(nodes.len());
    while let Some((_, v)) = ready.pop_first() {
        order.push(v);
        for &(a, b) in edges.range((v, 0)..(v + 1, 0)) {
            debug_assert_eq!(a, v);
            indegree[b] -= 1;
            if indegree[b] == 0 {
                ready.insert((nodes[b][0], b));
            }
        }
    }
    debug_assert_eq!(order.len(), nodes.len(), "cycles were merged");
    order
}

/// Signatures of a changed region for F6: its exact text (when long enough to mean something)
/// and its token substitution (removed-only and added-only words, when small).
fn signatures(changes: &Changes, atom: &crate::atom::Atom) -> Vec<String> {
    let AtomKind::Region(r) = &atom.kind else {
        return Vec::new();
    };
    let Some(text) = &changes.files[atom.file].text else {
        return Vec::new();
    };
    let side = |lines: Vec<(u32, &[u8])>| -> String {
        lines
            .iter()
            .map(|(_, l)| String::from_utf8_lossy(l).trim().to_owned())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let (removed, added) = (
        side(text.base_lines(r.base.clone())),
        side(text.new_lines(r.new.clone())),
    );
    let mut keys = Vec::new();
    let meaningful = removed
        .chars()
        .chain(added.chars())
        .filter(|c| !c.is_whitespace())
        .count();
    if meaningful >= 12 {
        keys.push(format!("X\u{1}{removed}\u{1}{added}"));
    }
    if !removed.is_empty() && !added.is_empty() {
        let words = |s: &str| -> Vec<String> {
            s.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
                .filter(|w| !w.is_empty())
                .map(str::to_owned)
                .collect()
        };
        let (mut gone, mut came) = (words(&removed), words(&added));
        let common: Vec<String> = gone.iter().filter(|w| came.contains(w)).cloned().collect();
        for w in &common {
            if let (Some(a), Some(b)) = (
                gone.iter().position(|x| x == w),
                came.iter().position(|x| x == w),
            ) {
                gone.remove(a);
                came.remove(b);
            }
        }
        gone.sort();
        came.sort();
        let small = (1..=6).contains(&gone.len()) && (1..=6).contains(&came.len());
        let substantive = gone
            .iter()
            .chain(&came)
            .any(|w| w.len() >= 3 && !w.chars().all(|c| c.is_ascii_digit()));
        if small && substantive {
            keys.push(format!("S\u{1}{}\u{1}{}", gone.join(" "), came.join(" ")));
        }
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tarjan_merges_cycles_only() {
        let edges: BTreeSet<(usize, usize)> =
            [(0, 1), (1, 0), (1, 2), (3, 4)].into_iter().collect();
        assert_eq!(tarjan(5, &edges), vec![0, 0, 2, 3, 4]);
    }

    #[test]
    fn topo_respects_edges_then_smallest_atom() {
        let nodes = vec![vec![5], vec![1], vec![3]];
        let edges: BTreeSet<(usize, usize)> = [(0, 1)].into_iter().collect();
        assert_eq!(topo(&nodes, &edges), vec![2, 0, 1]);
    }

    #[test]
    fn stems_ignore_extensions_and_test_directories() {
        assert_eq!(
            stem_key(b"src/Button.module.css"),
            stem_key(b"src/Button.tsx")
        );
        assert_eq!(
            stem_key(b"src/__snapshots__/Button.test.tsx.snap"),
            stem_key(b"src/Button.tsx")
        );
        assert_ne!(stem_key(b"lib/Button.tsx"), stem_key(b"src/Button.tsx"));
    }
}
