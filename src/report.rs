//! Printing a plan: the human preview (plan-buffer layout, SPEC §9.2) and `--json`.

use std::fmt::Write;

use serde::Serialize;

use crate::atom::{AtomId, AtomKind, Changes};
use crate::facts::Analysis;
use crate::graph::{Diagnostics, Plan, Tag};
use crate::verify::Verification;

fn tag_text(tag: &Tag) -> &'static str {
    match tag {
        Tag::New => "new",
        Tag::Rename => "rename",
        Tag::DebugOnly => "debug-only line",
        Tag::LooksLikeDebug => "looks like debug output",
        Tag::Style => "whitespace",
        Tag::FailsCheck => "does not pass the check",
        Tag::SameFile => "same file",
    }
}

/// The code-generated part of an atom line: location, counts, symbol, tags. Never a
/// message: carve does not write those.
pub fn atom_line(changes: &Changes, analysis: &Analysis, plan: &Plan, id: AtomId) -> String {
    let facts = &analysis.atoms[id.0 as usize - 1];
    let mut line = format!("  @ {:<4} {}", id.to_string(), changes.label(id));
    if let Some(symbol) = facts.new_symbol.as_ref().or(facts.enclosing.as_ref()) {
        let _ = write!(line, "  {symbol}");
    }
    let tags: Vec<&str> = plan
        .tags
        .get(&id)
        .into_iter()
        .flatten()
        .map(tag_text)
        .collect();
    if !tags.is_empty() {
        let _ = write!(line, "  [{}]", tags.join(", "));
    }
    if changes.atom(id).leave_by_default {
        line.push_str("  # large or binary new file");
    }
    line
}

#[derive(Serialize)]
struct JsonPlan<'a> {
    version: u32,
    atoms: Vec<JsonAtom>,
    groups: &'a [Vec<AtomId>],
    style_group: Option<usize>,
    leave: &'a [AtomId],
    edges: &'a [(AtomId, AtomId, Vec<&'static str>)],
    diagnostics: &'a Diagnostics,
    verification: Option<&'a Verification>,
    /// `git carve <base>` only (D46 phase 1): what was re-carved.
    #[serde(skip_serializing_if = "Option::is_none")]
    range: Option<JsonRange<'a>>,
    /// The plan buffer exactly as `--plan-only` prints it (D50): an agent saves it, adds
    /// `# proposed:` lines, and the human runs `--plan-file <file> --edit` (D49).
    #[serde(skip_serializing_if = "Option::is_none")]
    buffer: Option<&'a str>,
}

#[derive(Serialize)]
struct JsonRange<'a> {
    base: &'a str,
    merge_based: bool,
    commits: Vec<JsonRangeCommit<'a>>,
    /// Per atom: the old commits its lines come from (`oid` null: uncommitted; `lines` 0:
    /// the whole file).
    sources: std::collections::BTreeMap<AtomId, Vec<JsonSource<'a>>>,
}

#[derive(Serialize)]
struct JsonSource<'a> {
    oid: Option<&'a str>,
    lines: u32,
}

#[derive(Serialize)]
struct JsonRangeCommit<'a> {
    oid: &'a str,
    subject: &'a str,
    author: &'a str,
}

#[derive(Serialize)]
struct JsonAtom {
    id: AtomId,
    path: String,
    kind: String,
    base: Option<[u32; 2]>,
    new: Option<[u32; 2]>,
    symbol: Option<String>,
    tags: Vec<Tag>,
}

pub fn json(
    changes: &Changes,
    analysis: &Analysis,
    plan: &Plan,
    verification: Option<&Verification>,
) -> String {
    json_range(
        changes,
        analysis,
        plan,
        verification,
        None,
        &crate::attribution::Sources::new(),
        None,
    )
}

/// [`json`] plus the re-carved range, when there is one.
pub fn json_range(
    changes: &Changes,
    analysis: &Analysis,
    plan: &Plan,
    verification: Option<&Verification>,
    range: Option<&crate::precondition::ResolvedBase>,
    sources: &crate::attribution::Sources,
    buffer: Option<&str>,
) -> String {
    let atoms = changes
        .atoms
        .iter()
        .map(|a| {
            let facts = &analysis.atoms[a.id.0 as usize - 1];
            let (kind, base, new) = match &a.kind {
                AtomKind::Region(r) => (
                    "region".to_owned(),
                    Some([r.base.start, r.base.end]),
                    Some([r.new.start, r.new.end]),
                ),
                AtomKind::Whole(w) => (w.to_string(), None, None),
                AtomKind::Mode { .. } => ("mode".to_owned(), None, None),
            };
            JsonAtom {
                id: a.id,
                path: crate::model::show(&changes.files[a.file].path),
                kind,
                base,
                new,
                symbol: facts.new_symbol.clone().or_else(|| facts.enclosing.clone()),
                tags: plan.tags.get(&a.id).cloned().unwrap_or_default(),
            }
        })
        .collect();
    let doc = JsonPlan {
        version: 1,
        atoms,
        groups: &plan.groups,
        style_group: plan.style,
        leave: &plan.leave,
        edges: &plan.edges,
        diagnostics: &plan.diagnostics,
        verification,
        range: range.filter(|r| !r.commits.is_empty()).map(|r| JsonRange {
            base: &r.oid,
            merge_based: r.merge_based,
            commits: r
                .commits
                .iter()
                .map(|c| JsonRangeCommit {
                    oid: &c.oid,
                    subject: &c.subject,
                    author: &c.author,
                })
                .collect(),
            sources: sources
                .iter()
                .map(|(id, list)| {
                    (
                        *id,
                        list.iter()
                            .map(|(oid, lines)| JsonSource {
                                oid: oid.as_deref(),
                                lines: *lines,
                            })
                            .collect(),
                    )
                })
                .collect(),
        }),
        buffer,
    };
    serde_json::to_string_pretty(&doc).expect("plan serializes") + "\n"
}
