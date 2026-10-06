//! Reconstruction invariants R1–R3 (SPEC §5.5) on raw bytes, 1000 cases each.

use git_carve::text::{Region, TextPair};
use proptest::prelude::*;
use proptest::sample::Index;

/// A small alphabet so the two sides share lines, plus CRLF, blank lines, and unicode.
fn line() -> impl Strategy<Value = String> {
    (
        prop::sample::select(vec!["a", "b", "c", "", "  x", "é∂", "\t}", "a"]),
        prop::sample::select(vec!["\n", "\n", "\r\n"]),
    )
        .prop_map(|(text, end)| format!("{text}{end}"))
}

/// Text that may be empty and may lack a final newline.
fn text() -> impl Strategy<Value = Vec<u8>> {
    (
        prop::collection::vec(line(), 0..24),
        prop::option::of(prop::sample::select(vec!["z", "a", "é"])),
    )
        .prop_map(|(lines, tail)| {
            let mut text = lines.concat();
            text.push_str(tail.unwrap_or(""));
            text.into_bytes()
        })
}

/// A base and a realistic edit of it (insertions, deletions, replacements).
fn edited() -> impl Strategy<Value = (Vec<u8>, Vec<u8>)> {
    (
        prop::collection::vec(line(), 0..30),
        prop::collection::vec((0u8..3, any::<Index>(), line()), 0..8),
        any::<bool>(),
    )
        .prop_map(|(base, ops, drop_final_newline)| {
            let mut new = base.clone();
            for (op, at, line) in ops {
                match op {
                    0 => new.insert(at.index(new.len() + 1), line),
                    1 if !new.is_empty() => {
                        new.remove(at.index(new.len()));
                    }
                    _ if !new.is_empty() => {
                        let i = at.index(new.len());
                        new[i] = line;
                    }
                    _ => new.push(line),
                }
            }
            let mut new = new.concat().into_bytes();
            if drop_final_newline && new.last() == Some(&b'\n') {
                new.pop();
            }
            (base.concat().into_bytes(), new)
        })
}

fn sides() -> impl Strategy<Value = (Vec<u8>, Vec<u8>)> {
    prop_oneof![edited(), (text(), text())]
}

/// Sides plus a group assignment per region: 0..4 are ordered groups, 4 is leave.
fn partitioned() -> impl Strategy<Value = (Vec<u8>, Vec<u8>, Vec<u8>)> {
    sides().prop_flat_map(|(base, new)| {
        let regions = TextPair::new(base.clone(), new.clone()).regions().len();
        (
            Just(base),
            Just(new),
            prop::collection::vec(0u8..5, regions),
        )
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    /// R1: all regions rebuild the new side byte for byte.
    #[test]
    fn r1_all_regions_rebuild_new((base, new) in sides()) {
        let pair = TextPair::new(base, new.clone());
        let regions = pair.regions();
        prop_assert_eq!(pair.splice(&regions), new);
    }

    /// R2: no regions rebuild the base byte for byte.
    #[test]
    fn r2_no_regions_rebuild_base((base, new) in sides()) {
        let pair = TextPair::new(base.clone(), new);
        prop_assert_eq!(pair.splice(&Vec::<Region>::new()), base);
    }

    /// Regions are non-empty, ordered, and separated by an unchanged line on both sides.
    #[test]
    fn regions_are_maximal_and_separated((base, new) in sides()) {
        let regions = TextPair::new(base, new).regions();
        for r in &regions {
            prop_assert!(!r.base.is_empty() || !r.new.is_empty());
        }
        for w in regions.windows(2) {
            prop_assert!(w[0].base.end < w[1].base.start, "{:?}", w);
            prop_assert!(w[0].new.end < w[1].new.start, "{:?}", w);
        }
    }

    /// R3: whatever the partition and order, the last group ends at base + non-leave
    /// regions, and each prefix is independent of region order.
    #[test]
    fn r3_partition_ends_at_non_leave((base, new, assignment) in partitioned()) {
        let pair = TextPair::new(base, new);
        let regions = pair.regions();
        let mut cumulative: Vec<&Region> = Vec::new();
        for group in 0..4u8 {
            cumulative.extend(regions.iter().zip(&assignment).filter(|(_, g)| **g == group).map(|(r, _)| r));
            let forward = pair.splice(cumulative.iter().copied());
            let backward = pair.splice(cumulative.iter().rev().copied());
            prop_assert_eq!(forward, backward);
        }
        let non_leave: Vec<&Region> = regions.iter().zip(&assignment).filter(|(_, g)| **g != 4).map(|(r, _)| r).collect();
        prop_assert_eq!(pair.splice(cumulative), pair.splice(non_leave));
    }
}
