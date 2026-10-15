//! Storage backends for flux (SPEC §5).
//!
//! * [`MetaStore`]: changesets, refs, derived-data mappings, content metadata
//!   and git mappings. Implemented for SQLite and Postgres (`sql` module).
//! * [`BlobStore`]: immutable, content-addressed objects on any
//!   `object_store` backend (local fs, S3-compatible, in-memory).

mod blob;
pub mod dict;
mod meta;
pub mod segment;
mod sql;

pub use blob::{
    BlobStats, BlobStore, CHUNK_THRESHOLD, ChunkSink, ContentLayout, MetaHasher, ObjectCodec, PutObject, content_meta,
};
pub use meta::*;
pub use sql::{PostgresStore, SqliteStore};

use std::sync::Arc;

/// The blob-store URL for a local directory (portable: `file:///C:/...` on
/// Windows). The directory need not exist yet.
pub fn file_url(dir: &std::path::Path) -> anyhow::Result<String> {
    let abs = std::path::absolute(dir)?;
    Ok(url::Url::from_directory_path(&abs).map_err(|_| anyhow::anyhow!("not a usable directory: {}", abs.display()))?.to_string())
}

/// Open a metadata store from a URL: `sqlite://path/to/db`, `sqlite::memory:`,
/// or `postgres://...`. Migrations are applied on open.
pub async fn open_meta(url: &str) -> anyhow::Result<Arc<dyn MetaStore>> {
    if url.starts_with("sqlite:") {
        Ok(Arc::new(SqliteStore::open(url).await?))
    } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        Ok(Arc::new(PostgresStore::open(url).await?))
    } else {
        anyhow::bail!("unsupported metadata store url {url:?} (expected sqlite: or postgres://)")
    }
}
