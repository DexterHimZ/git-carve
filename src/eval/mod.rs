//! `git carve eval` (SPEC §11, EVAL.md, DECISIONS.md D22–D25): the corpus builder and the
//! arm runner. One engine for the pre-registered evaluation, judge training data, and
//! regression measurement.

#[cfg(feature = "eval")]
pub mod corpus;
#[cfg(feature = "eval")]
pub mod decide;
#[cfg(feature = "eval")]
pub mod deps;
#[cfg(feature = "eval")]
pub mod metrics;
#[cfg(feature = "eval")]
pub mod pairs;
#[cfg(feature = "eval")]
pub mod range_check;
#[cfg(feature = "eval")]
pub mod real;
#[cfg(feature = "eval")]
pub mod run;
pub mod sha256;
#[cfg(feature = "eval")]
pub mod speed;
#[cfg(feature = "eval")]
pub mod sweep;
