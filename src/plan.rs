//! The plan buffer (SPEC §9): rendering, parsing, and validation. The buffer is a user
//! interface: every commit message is written here by the user, never by carve.
//! Byte-deterministic for a given state (DECISIONS.md D19).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use serde::{Deserialize, Serialize};

use crate::atom::{AtomId, AtomKind, Changes};
use crate::facts::Analysis;
use crate::graph::Plan;
use crate::report::atom_line;
use crate::verify::{Dropped, Verification};

/// Everything below this line is ignored, as with `git commit -v` (DECISIONS.md D16).
pub const SCISSORS: &str = "# ------------------------ >8 ------------------------";
const DIFF_PER_ATOM: usize = 40;
pub const DIFF_TOTAL_DEFAULT: usize = 400;

/// Header comment lines carve adds on reopen; stripped before new ones are added.
const NOTE_PREFIXES: &[&str] = &[
    "# ERROR:",
    "# CHECK FAILED",
    "#   | ",
    "# hint:",
    "# From your last attempt",
    "#   > ",
];

pub struct RenderOptions<'a> {
    pub verification: Option<&'a Verification>,
    /// A message to pre-fill per group (D17).
    pub messages: &'a [Option<String>],
    /// Extra `#` lines for the top (restored messages that did not match).
    pub notes: &'a [String],
    /// Diff lines below the scissors (0: no diff section).
    pub diff_lines: usize,
    /// Show the judge's `# unsure:` notes (DECISIONS.md D30). Off for blinded renders.
    pub unsure: bool,
}

pub fn render(changes: &Changes, analysis: &Analysis, plan: &Plan, opts: &RenderOptions) -> String {
    render_range(changes, analysis, plan, opts, None)
}

/// What a range carve adds to the buffer (D46 phase 1): the old commits, and where each
/// atom's lines came from.
pub struct RangeView<'a> {
    pub base: &'a crate::precondition::ResolvedBase,
    pub sources: &'a crate::attribution::Sources,
}

/// `# from: 57468c8 wip (12 lines), uncommitted (2 lines)`: the old commits a block's lines
/// come from, oldest first. At column 0, so the parser reads it as a comment.
fn from_line(view: &RangeView, group: &[AtomId]) -> Option<String> {
    let mut lines: Vec<(Option<&str>, u32, bool)> = Vec::new();
    for id in group {
        for (oid, n) in view.sources.get(id).into_iter().flatten() {
            let key = oid.as_deref();
            match lines.iter_mut().find(|(k, _, _)| *k == key) {
                Some((_, total, whole)) => {
                    *total += n;
                    *whole |= *n == 0;
                }
                None => lines.push((key, *n, *n == 0)),
            }
        }
    }
    if lines.is_empty() {
        return None;
    }
    let order = |k: Option<&str>| {
        k.and_then(|oid| view.base.commits.iter().position(|c| c.oid == oid))
            .unwrap_or(usize::MAX)
    };
    lines.sort_by_key(|(k, _, _)| order(*k));
    let parts: Vec<String> = lines
        .iter()
        .map(|(k, n, whole)| {
            let name = match k {
                Some(oid) => {
                    let subject = view
                        .base
                        .commits
                        .iter()
                        .find(|c| c.oid == *oid)
                        .map_or("", |c| c.subject.as_str());
                    let subject: String = subject.chars().take(40).collect();
                    format!("{} {subject}", &oid[..oid.len().min(7)])
                }
                None => "uncommitted".to_owned(),
            };
            let size = match (n, whole) {
                (0, _) => "file".to_owned(),
                (1, false) => "1 line".to_owned(),
                (n, false) => format!("{n} lines"),
                (n, true) => format!("file, {n} line{}", if *n == 1 { "" } else { "s" }),
            };
            format!("{name} ({size})")
        })
        .collect();
    Some(format!("# from: {}", parts.join(", ")))
}

/// The most subjects the range header lists before "and N more".
const RANGE_LISTED: usize = 20;

/// [`render`] for `git carve <base>`: the header also lists the commits being re-carved,
/// as comments. Their messages are for reference only; nothing is copied into a commit.
pub fn render_range(
    changes: &Changes,
    analysis: &Analysis,
    plan: &Plan,
    opts: &RenderOptions,
    view: Option<&RangeView>,
) -> String {
    let view = view.filter(|v| !v.base.commits.is_empty());
    let range = view.map(|v| v.base);
    let mut out = String::new();
    let commits = plan.groups.len();
    let left = plan.leave.len();
    let counts = format!(
        "# git carve: {commits} commit{}, {left} hunk{} left out.",
        if commits == 1 { "" } else { "s" },
        if left == 1 { "" } else { "s" }
    );
    match opts.verification {
        Some(v) => {
            match v.baseline_errors {
                Some(e) => {
                    let _ = writeln!(
                        out,
                        "{counts} {} already fails the check with {e} error{}; no commit below adds a new one: {}",
                        if range.is_some() { "The base" } else { "HEAD" },
                        if e == 1 { "" } else { "s" },
                        v.check
                    );
                }
                None => {
                    let _ = writeln!(out, "{counts} Every commit below passed: {}", v.check);
                }
            }
            for d in &v.dropped {
                let _ = match d {
                    Dropped::CheckFailed { after, .. } => writeln!(
                        out,
                        "# (merged planned commits {after} and {}: they did not pass separately)",
                        after + 1
                    ),
                    Dropped::Syntax { after, path } => writeln!(
                        out,
                        "# (merged planned commits {after} and {}: {after} alone would break the syntax of {path})",
                        after + 1
                    ),
                };
            }
            for id in &v.unleft {
                let _ = writeln!(
                    out,
                    "# ({id} is kept: your working tree needs it to pass the check)"
                );
            }
            if let Some(f) = &v.failing {
                let ids: Vec<String> = f.left.iter().map(ToString::to_string).collect();
                let _ = writeln!(
                    out,
                    "# Your working tree does not pass the check, so {} stay{} uncommitted (under leave) \
                     and everything else passes:",
                    ids.join(" "),
                    if f.left.len() == 1 { "s" } else { "" }
                );
                for e in &f.errors {
                    let _ = writeln!(out, "#   {e}");
                }
            }
            for path in &v.unverified {
                let _ = writeln!(
                    out,
                    "# WARNING: the check passed on a deliberately broken {path}; changes to it are unverified."
                );
            }
        }
        None => {
            let _ = writeln!(out, "{counts} These commits are NOT verified (--no-check).");
        }
    }
    if let Some(r) = range {
        let k = r.commits.len();
        let _ = writeln!(
            out,
            "#\n# Re-carving {k} commit{} since {}, plus any uncommitted changes.\n\
             # Their old messages, for reference only (nothing is copied into a commit):",
            if k == 1 { "" } else { "s" },
            &r.oid[..r.oid.len().min(7)]
        );
        for c in r.commits.iter().take(RANGE_LISTED) {
            let subject = if c.subject.is_empty() {
                "(no subject)"
            } else {
                &c.subject
            };
            let _ = writeln!(out, "#   {} {subject}", &c.oid[..c.oid.len().min(7)]);
        }
        if k > RANGE_LISTED {
            let _ = writeln!(out, "#   ... and {} more", k - RANGE_LISTED);
        }
        let mut authors: Vec<&str> = r.commits.iter().map(|c| c.author.as_str()).collect();
        authors.sort_unstable();
        authors.dedup();
        if authors.len() > 1 {
            let _ = writeln!(out, "#   authors: {}", authors.join(", "));
        }
        if !r.trailers.is_empty() {
            let _ = writeln!(
                out,
                "# Trailers from those messages (copy any you want to keep):"
            );
            for t in &r.trailers {
                let _ = writeln!(out, "#   {t}");
            }
        }
    }
    out.push_str(
        "#\n\
         # Write a message under each \"commit\" line (first line = subject).\n\
         # Move \"@\" lines between blocks to regroup. Reorder blocks to reorder commits.\n\
         # Delete a \"commit\" line to merge its block into the block above.\n\
         # \"@\" lines under \"leave\" stay in your working tree, uncommitted.\n\
         # Lines starting with '#' are ignored. Any commit with an empty message aborts everything.\n\
         # If you change grouping or order, the check re-runs before anything is committed.\n",
    );
    for note in opts.notes {
        let _ = writeln!(out, "{note}");
    }
    for (k, group) in plan.groups.iter().enumerate() {
        out.push_str("\ncommit\n");
        if plan.style == Some(k) {
            out.push_str("# style-only (whitespace)\n");
        }
        match opts.messages.get(k).cloned().flatten() {
            Some(message) => {
                let _ = writeln!(out, "{}", message.trim_end());
                out.push('\n');
            }
            None => out.push_str("\n\n"),
        }
        for &id in group {
            let _ = writeln!(out, "{}", atom_line(changes, analysis, plan, id));
        }
        if let Some(from) = view.and_then(|v| from_line(v, group)) {
            let _ = writeln!(out, "{from}");
        }
        if opts.unsure {
            for (_, note) in plan.unsure.iter().filter(|(g, _)| *g == k) {
                let _ = writeln!(out, "{note}");
            }
        }
    }
    if !plan.leave.is_empty() {
        out.push_str("\nleave\n");
        for &id in &plan.leave {
            let _ = writeln!(out, "{}", atom_line(changes, analysis, plan, id));
        }
    }
    if opts.diff_lines > 0 {
        out.push('\n');
        out.push_str(SCISSORS);
        out.push_str("\n# Do not modify or remove the line above.\n# Everything below it is ignored: each hunk's diff, for reference.\n");
        let mut budget = opts.diff_lines;
        let order: Vec<AtomId> = plan
            .groups
            .iter()
            .flatten()
            .chain(&plan.leave)
            .copied()
            .collect();
        for id in order {
            if budget == 0 {
                out.push_str("\n(diff truncated: carve.planDiff)\n");
                break;
            }
            let lines = diff_lines(changes, id);
            let shown = lines.len().min(DIFF_PER_ATOM).min(budget);
            let _ = writeln!(out, "\n@@ {id}  {}", changes.position(id));
            for line in &lines[..shown] {
                let _ = writeln!(out, "{line}");
            }
            if shown < lines.len() {
                let _ = writeln!(out, "... {} more lines", lines.len() - shown);
            }
            budget -= shown;
        }
    }
    out
}

fn diff_lines(changes: &Changes, id: AtomId) -> Vec<String> {
    let atom = changes.atom(id);
    let file = &changes.files[atom.file];
    let text = |bytes: &[u8]| {
        String::from_utf8_lossy(bytes)
            .trim_end_matches(['\n', '\r'])
            .to_owned()
    };
    match &atom.kind {
        AtomKind::Region(r) => {
            let pair = file.text.as_ref().expect("regions keep their text");
            let mut lines: Vec<String> = pair
                .base_lines(r.base.clone())
                .iter()
                .map(|(_, l)| format!("-{}", text(l)))
                .collect();
            lines.extend(
                pair.new_lines(r.new.clone())
                    .iter()
                    .map(|(_, l)| format!("+{}", text(l))),
            );
            lines
        }
        AtomKind::Whole(w) => vec![format!("({w})")],
        AtomKind::Mode { from, to } => vec![format!("(mode {from} -> {to})")],
    }
}

/// A plan as the user edited it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edited {
    pub groups: Vec<EditedGroup>,
    pub leave: Vec<AtomId>,
    /// Atoms missing from the buffer: they stay in the working tree (a warning, not an error).
    pub missing: Vec<AtomId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditedGroup {
    pub atoms: Vec<AtomId>,
    pub message: String,
}

impl Edited {
    /// The grouping, without messages: what re-verification depends on (§9.5).
    pub fn structure(&self) -> (Vec<Vec<AtomId>>, Vec<AtomId>) {
        let mut leave = self.leave.clone();
        leave.extend(&self.missing);
        leave.sort_unstable();
        (self.groups.iter().map(|g| g.atoms.clone()).collect(), leave)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Parsed {
    Plan(Edited),
    /// Reopen the editor with these errors at the top (§9.4).
    Errors(Vec<String>),
    /// Stop with no changes (exit 1): an empty plan or an empty message.
    Abort(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Block {
    Commit,
    Leave,
}

struct RawBlock {
    kind: Block,
    atoms: Vec<(AtomId, usize)>,
    message: Vec<String>,
}

/// Blocks as written, before validation. Lines are numbered from 1.
fn blocks(text: &str) -> (Vec<RawBlock>, Vec<String>) {
    let mut blocks: Vec<RawBlock> = Vec::new();
    let mut errors = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim_end_matches('\r');
        if line == SCISSORS {
            break;
        }
        if line.starts_with('#') {
            continue;
        }
        let trimmed = line.trim();
        if trimmed == "commit" || trimmed == "leave" {
            let kind = if trimmed == "commit" {
                Block::Commit
            } else {
                Block::Leave
            };
            blocks.push(RawBlock {
                kind,
                atoms: Vec::new(),
                message: Vec::new(),
            });
            continue;
        }
        if let Some(id) = atom_id(line) {
            match blocks.last_mut() {
                Some(block) => block.atoms.push((id, n + 1)),
                None => errors.push(format!(
                    "line {}: {id} appears before the first \"commit\" line",
                    n + 1
                )),
            }
            continue;
        }
        match blocks.last_mut() {
            Some(block) if block.kind == Block::Commit => block.message.push(line.to_owned()),
            Some(_) if trimmed.is_empty() => {}
            Some(_) => errors.push(format!("line {}: only \"@\" lines belong under \"leave\"", n + 1)),
            None if trimmed.is_empty() => {}
            None => errors.push(format!("line {}: text before the first \"commit\" line (did you delete the first \"commit\" line?)", n + 1)),
        }
    }
    (blocks, errors)
}

/// `^\s*@\s+(a\d+)\b`: only the id is read; the label after it is ignored.
fn atom_id(line: &str) -> Option<AtomId> {
    let rest = line.trim_start().strip_prefix('@')?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let token = rest.trim_start();
    let digits: String = token
        .strip_prefix('a')?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let after = token[1 + digits.len()..].chars().next();
    if digits.is_empty() || after.is_some_and(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    digits.parse().ok().map(AtomId)
}

/// Trims blank lines at both ends and collapses runs of blank lines.
fn clean(lines: &[String]) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in lines {
        let line = line.trim_end();
        if line.is_empty() && out.last().is_none_or(|l| l.is_empty()) {
            continue;
        }
        out.push(line);
    }
    while out.last() == Some(&"") {
        out.pop();
    }
    if out.is_empty() {
        String::new()
    } else {
        out.join("\n") + "\n"
    }
}

pub fn parse(text: &str, changes: &Changes, hard: &[(AtomId, AtomId, &'static str)]) -> Parsed {
    let (blocks, mut errors) = blocks(text);
    if blocks.iter().all(|b| b.atoms.is_empty())
        && blocks.iter().all(|b| clean(&b.message).is_empty())
    {
        return Parsed::Abort("the plan is empty; nothing was committed".to_owned());
    }
    let mut seen: BTreeMap<AtomId, usize> = BTreeMap::new();
    let mut block_of: BTreeMap<AtomId, usize> = BTreeMap::new();
    for (b, block) in blocks.iter().enumerate() {
        for &(id, line) in &block.atoms {
            if !changes.contains(id) {
                errors.push(format!("line {line}: unknown atom {id}"));
            } else if let Some(first) = seen.insert(id, line) {
                errors.push(format!("line {line}: {id} is also on line {first}"));
            } else {
                block_of.insert(id, b);
            }
        }
    }
    let mut commit_number = 0;
    for block in &blocks {
        if block.kind == Block::Commit {
            commit_number += 1;
            if block.atoms.is_empty() {
                errors.push(format!("commit {commit_number} has no \"@\" lines (delete its \"commit\" line to drop it)"));
            }
        }
    }
    let mut reported = BTreeSet::new();
    for &(a, b, why) in hard {
        if let (Some(x), Some(y)) = (block_of.get(&a), block_of.get(&b)) {
            if x != y && reported.insert((a, b)) {
                errors.push(format!("{a} and {b} must be in the same commit: {why}"));
            }
        }
    }
    if !errors.is_empty() {
        return Parsed::Errors(errors);
    }

    let mut groups = Vec::new();
    let mut leave = Vec::new();
    for block in &blocks {
        let atoms: Vec<AtomId> = block.atoms.iter().map(|&(id, _)| id).collect();
        match block.kind {
            Block::Leave => leave.extend(atoms),
            Block::Commit => {
                let message = clean(&block.message);
                if message.is_empty() {
                    return Parsed::Abort(format!(
                        "commit {} has an empty message; nothing was committed",
                        groups.len() + 1
                    ));
                }
                groups.push(EditedGroup { atoms, message });
            }
        }
    }
    leave.sort_unstable();
    let missing = changes
        .atoms
        .iter()
        .map(|a| a.id)
        .filter(|id| !seen.contains_key(id))
        .collect();
    Parsed::Plan(Edited {
        groups,
        leave,
        missing,
    })
}

/// The user's text with carve's previous notes removed and new ones put at the top.
pub fn with_notes(text: &str, notes: &[String]) -> String {
    let kept: Vec<&str> = text
        .lines()
        .filter(|l| !NOTE_PREFIXES.iter().any(|p| l.starts_with(p)))
        .collect();
    let mut out = String::new();
    for note in notes {
        out.push_str(note);
        out.push('\n');
    }
    out.push_str(&kept.join("\n"));
    out.push('\n');
    out
}

// Keeping typed messages across aborted runs (DECISIONS.md D17).

/// Content fingerprint of an atom: path, kind, and changed bytes. Survives id changes.
pub fn atom_fingerprint(changes: &Changes, id: AtomId) -> u64 {
    let atom = changes.atom(id);
    let file = &changes.files[atom.file];
    let mut h = Fnv::new();
    h.write(&file.path);
    match &atom.kind {
        AtomKind::Region(r) => {
            let pair = file.text.as_ref().expect("regions keep their text");
            h.write(b"R");
            for (_, l) in pair.base_lines(r.base.clone()) {
                h.write(l);
            }
            h.write(b"\0");
            for (_, l) in pair.new_lines(r.new.clone()) {
                h.write(l);
            }
        }
        AtomKind::Whole(w) => {
            h.write(format!("W{w}").as_bytes());
            for entry in [&file.base, &file.new].into_iter().flatten() {
                h.write(entry.oid.as_bytes());
            }
        }
        AtomKind::Mode { from, to } => h.write(format!("M{from}{to}").as_bytes()),
    }
    h.finish()
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Saved {
    pub groups: Vec<SavedGroup>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SavedGroup {
    /// Sorted atom fingerprints; empty when the buffer could not be read.
    pub atoms: Vec<u64>,
    pub message: String,
}

/// The messages in a buffer, whatever its validity, keyed by the content of their atoms.
pub fn save(text: &str, changes: &Changes) -> Saved {
    let (blocks, _) = blocks(text);
    let groups = blocks
        .into_iter()
        .filter(|b| b.kind == Block::Commit)
        .filter_map(|b| {
            let message = clean(&b.message);
            if message.is_empty() {
                return None;
            }
            let mut atoms: Vec<u64> = b
                .atoms
                .iter()
                .filter(|(id, _)| changes.contains(*id))
                .map(|&(id, _)| atom_fingerprint(changes, id))
                .collect();
            atoms.sort_unstable();
            Some(SavedGroup { atoms, message })
        })
        .collect();
    Saved { groups }
}

/// Messages for `groups` from a saved buffer: exact content matches only. Returns the
/// per-group messages and the leftover messages as `#` note lines.
pub fn restore(
    saved: &Saved,
    changes: &Changes,
    groups: &[Vec<AtomId>],
) -> (Vec<Option<String>>, Vec<String>) {
    let mut used = vec![false; saved.groups.len()];
    let messages = groups
        .iter()
        .map(|group| {
            let mut fps: Vec<u64> = group
                .iter()
                .map(|&id| atom_fingerprint(changes, id))
                .collect();
            fps.sort_unstable();
            let found = saved
                .groups
                .iter()
                .enumerate()
                .find(|(i, s)| !used[*i] && !s.atoms.is_empty() && s.atoms == fps);
            found.map(|(i, s)| {
                used[i] = true;
                s.message.clone()
            })
        })
        .collect();
    let mut notes = Vec::new();
    let unused: Vec<&SavedGroup> = saved
        .groups
        .iter()
        .enumerate()
        .filter(|(i, _)| !used[*i])
        .map(|(_, s)| s)
        .collect();
    if !unused.is_empty() {
        notes.push("# From your last attempt (not applied here; copy what you need):".to_owned());
        for s in unused {
            for line in s.message.lines() {
                notes.push(format!("#   > {line}"));
            }
        }
    }
    (messages, notes)
}

/// FNV-1a, 64-bit: a stable content hash without a dependency.
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
        self.0 ^= 0xff;
        self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

/// D49: every message line of a plan buffer becomes a `# proposed:` comment, so only the
/// human's own typing can fill a message. Comments, atom lines, and blank lines stay.
pub fn messages_as_proposals(text: &str) -> String {
    let mut in_message = false;
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let trimmed = line.trim_start();
        if line == "commit" {
            in_message = true;
        } else if line == "leave" || trimmed.starts_with('@') {
            in_message = false;
        } else if in_message && !trimmed.is_empty() && !trimmed.starts_with('#') {
            out.push_str("# proposed: ");
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atom_lines_read_only_the_id() {
        assert_eq!(
            atom_id("  @ a12  src/x.ts:3  +1 -0  foo  [new]"),
            Some(AtomId(12))
        );
        assert_eq!(atom_id("@\ta3"), Some(AtomId(3)));
        assert_eq!(atom_id("@a3"), None);
        assert_eq!(atom_id("  @ b3"), None);
        assert_eq!(atom_id("  @ a3x"), None);
        assert_eq!(atom_id("email me @ a3"), None);
    }

    #[test]
    fn messages_are_cleaned() {
        let lines: Vec<String> = ["", "Subject", "", "", "Body line", "", ""]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(clean(&lines), "Subject\n\nBody line\n");
        assert_eq!(clean(&[String::new(), "  ".into()]), "");
    }

    #[test]
    fn notes_replace_earlier_notes() {
        let text = "# ERROR: old\ncommit\nmsg\n  @ a1\n";
        let out = with_notes(text, &["# ERROR: new".into()]);
        assert_eq!(out, "# ERROR: new\ncommit\nmsg\n  @ a1\n");
    }
}
