//! git interoperability (SPEC §10). Phase 1: history import + verification.

pub mod git;
pub mod import;

pub use import::{ImportOptions, ImportStats, import, verify};
