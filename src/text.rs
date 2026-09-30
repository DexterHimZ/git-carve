//! Line diffing and splicing on raw bytes (SPEC §5.3, §5.4). Pure functions: this module is
//! the one the reconstruction property tests (R1–R3) hammer.

use std::ops::Range;

use imara_diff::sources::byte_lines;
use imara_diff::{Algorithm, Diff, InternedInput};

/// One maximal changed region: base lines `base` became new lines `new` (0-based, half-open).
/// Consecutive regions are separated by at least one unchanged line on both sides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Region {
    pub base: Range<u32>,
    pub new: Range<u32>,
}

/// Start offset of each line, plus a final entry equal to `bytes.len()`. A line keeps its
/// terminator, so a missing final newline is simply a different last line.
pub fn line_starts(bytes: &[u8]) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(
        bytes
            .iter()
            .enumerate()
            .filter(|&(_, &b)| b == b'\n')
            .map(|(i, _)| i + 1)
            .filter(|&end| end < bytes.len()),
    );
    if bytes.is_empty() {
        starts.clear();
    }
    starts.push(bytes.len());
    starts
}

/// Zero-context histogram diff, with the indent heuristic for readable region placement.
pub fn diff(base: &[u8], new: &[u8]) -> Vec<Region> {
    let input = InternedInput::new(byte_lines(base), byte_lines(new));
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);
    diff.hunks()
        .map(|hunk| Region {
            base: hunk.before,
            new: hunk.after,
        })
        .collect()
}

/// Both sides of a changed text file with their line tables.
#[derive(Clone, Debug)]
pub struct TextPair {
    pub base: Vec<u8>,
    pub new: Vec<u8>,
    base_starts: Vec<usize>,
    new_starts: Vec<usize>,
}

impl TextPair {
    pub fn new(base: Vec<u8>, new: Vec<u8>) -> Self {
        let base_starts = line_starts(&base);
        let new_starts = line_starts(&new);
        Self {
            base,
            new,
            base_starts,
            new_starts,
        }
    }

    /// Base lines in `range`, with their 0-based numbers.
    pub fn base_lines(&self, range: Range<u32>) -> Vec<(u32, &[u8])> {
        range
            .map(|i| {
                (
                    i,
                    &self.base[self.base_starts[i as usize]..self.base_starts[i as usize + 1]],
                )
            })
            .collect()
    }

    /// New lines in `range`, with their 0-based numbers.
    pub fn new_lines(&self, range: Range<u32>) -> Vec<(u32, &[u8])> {
        range
            .map(|i| {
                (
                    i,
                    &self.new[self.new_starts[i as usize]..self.new_starts[i as usize + 1]],
                )
            })
            .collect()
    }

    pub fn regions(&self) -> Vec<Region> {
        diff(&self.base, &self.new)
    }

    /// The base with each given region replaced by its new lines. `regions` must be
    /// disjoint regions from [`TextPair::regions`]; order does not matter.
    pub fn splice<'a>(&self, regions: impl IntoIterator<Item = &'a Region>) -> Vec<u8> {
        let mut regions: Vec<&Region> = regions.into_iter().collect();
        regions.sort_by_key(|r| r.base.start);
        let line = |starts: &[usize], i: u32| starts[i as usize];
        let mut out = Vec::with_capacity(self.base.len().max(self.new.len()));
        let mut pos = 0;
        for r in regions {
            out.extend_from_slice(
                &self.base[line(&self.base_starts, pos)..line(&self.base_starts, r.base.start)],
            );
            out.extend_from_slice(
                &self.new[line(&self.new_starts, r.new.start)..line(&self.new_starts, r.new.end)],
            );
            pos = r.base.end;
        }
        out.extend_from_slice(&self.base[line(&self.base_starts, pos)..]);
        out
    }
}
