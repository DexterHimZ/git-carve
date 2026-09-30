//! What a file looks like when a subset of its atoms applies (SPEC §5.4). Content is rebuilt
//! from the base plus selected regions; no patch is ever applied.

use crate::atom::{AtomId, AtomKind, Changes};
use crate::model::{Entry, Mode};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// An object that already exists (the base or the new blob).
    Entry(Entry),
    /// Rebuilt content that still needs to be written.
    Blob {
        mode: Mode,
        bytes: Vec<u8>,
    },
    Absent,
}

pub fn target(changes: &Changes, file: usize, selected: &dyn Fn(AtomId) -> bool) -> Target {
    let f = &changes.files[file];
    let mut mode_selected = None;
    let mut whole_selected = None;
    let mut regions = 0;
    let mut chosen = Vec::new();
    for &i in &changes.file_atoms[file] {
        let atom = &changes.atoms[i];
        let on = selected(atom.id);
        match &atom.kind {
            AtomKind::Mode { .. } => mode_selected = Some(on),
            AtomKind::Whole(_) => whole_selected = Some(on),
            AtomKind::Region(r) => {
                regions += 1;
                if on {
                    chosen.push(r);
                }
            }
        }
    }
    // A mode atom exists only when both sides are regular files.
    let mode_for = |own: Mode| match (mode_selected, &f.base, &f.new) {
        (Some(true), _, Some(new)) => new.mode,
        (Some(false), Some(base), _) => base.mode,
        _ => own,
    };

    if let Some(on) = whole_selected {
        let side = if on { &f.new } else { &f.base };
        return match side {
            Some(e) => Target::Entry(Entry {
                mode: mode_for(e.mode),
                oid: e.oid.clone(),
            }),
            None => Target::Absent,
        };
    }
    let (Some(base), Some(new)) = (&f.base, &f.new) else {
        unreachable!("added and deleted files always have a whole-file atom");
    };
    let mode = mode_for(base.mode);
    if chosen.is_empty() {
        Target::Entry(Entry {
            mode,
            oid: base.oid.clone(),
        })
    } else if chosen.len() == regions {
        // R1: rebuilding from all regions yields the new blob exactly.
        Target::Entry(Entry {
            mode,
            oid: new.oid.clone(),
        })
    } else {
        let text = f.text.as_ref().expect("files with regions keep their text");
        Target::Blob {
            mode,
            bytes: text.splice(chosen),
        }
    }
}
