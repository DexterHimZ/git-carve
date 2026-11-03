//! git carve splits a dirty working tree into commits that each pass the user's check.
//! It never writes commit messages. See `docs/SPEC.md`.

pub mod atom;
pub mod branch;
pub mod check;
pub mod cli;
pub mod commit;
pub mod editor;
pub mod error;
pub mod eval;
pub mod exit;
pub mod facts;
pub mod git;
pub mod graph;
pub mod group;
pub mod judge;
pub mod lang;
pub mod model;
pub mod plan;
pub mod precondition;
pub mod reconstruct;
pub mod repo;
pub mod report;
pub mod session;
pub mod snapshot;
pub mod text;
pub mod tree;
pub mod undo;
pub mod verify;

pub use error::{Error, Result};
