//! `git carve eval` (SPEC §11, EVAL.md, DECISIONS.md D22–D25): the corpus builder and the
//! arm runner. One engine for the pre-registered evaluation, judge training data, and
//! regression measurement.

pub mod corpus;
pub mod decide;
pub mod deps;
pub mod metrics;
pub mod pairs;
pub mod range_check;
pub mod real;
pub mod run;
pub mod sha256;
pub mod speed;
pub mod sweep;
