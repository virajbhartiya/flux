//! Derived data (SPEC §6). Phase 1 ships `tree_v1` plus the tree walking and
//! diffing primitives that checkout, status and the git importer build on.

pub mod lastmod;
pub mod tree;

pub use lastmod::{LmEntry, LmNode, LmWriter, Side, derive_lastmod, lastmod_dir, load_lm, lookup_lastmod};
pub use tree::{
    TreeDiffEntry, TreeFile, TreeOverlay, TreeSource, TreeWriter, derive_tree, derive_tree_buffered, diff_trees, diff_trees_in,
    lookup_path, lookup_path_in, walk_tree,
};

use flux_store::DerivedTypeId;

/// Stable derived-type ids. Never reuse a number.
pub const TREE_V1: DerivedTypeId = 1;
pub const LASTMOD_V1: DerivedTypeId = 2;

#[derive(Debug, thiserror::Error)]
pub enum DeriveError {
    #[error("missing tree object {0}")]
    MissingTree(flux_core::TreeId),
    #[error("missing derived object {0}")]
    MissingDerived(flux_core::DerivedId),
    #[error(transparent)]
    Core(#[from] flux_core::Error),
    #[error(transparent)]
    Store(#[from] flux_store::StoreError),
}

pub type DeriveResult<T> = Result<T, DeriveError>;
