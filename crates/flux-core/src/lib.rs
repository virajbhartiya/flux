//! Core data model for flux (SPEC §2–§3).
//!
//! Everything in this crate is pure: no I/O, no async. The canonical encoding
//! implemented in [`cbor`] is the single source of truth for object identity.

pub mod cbor;
pub mod changeset;
pub mod error;
pub mod id;
pub mod path;
pub mod tree;

pub use changeset::{Changeset, FileChange, FileKind, Timestamp};
pub use error::{Error, Result};
pub use id::{ChangesetId, ChunkId, ContentId, DerivedId, ObjectKind, TreeId};
pub use path::{PathComponent, RepoPath};
pub use tree::{EntryKind, Tree, TreeEntry};
