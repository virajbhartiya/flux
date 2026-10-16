//! SQL metadata stores (SQLite and Postgres) sharing one implementation.
//!
//! Both backends accept `$N` placeholders, so the queries are identical; the
//! differences (connection setup, log-sequence serialization) are isolated in
//! small per-backend hooks. Batch lookups build `IN (...)` lists in chunks.

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use flux_core::{ChangesetId, ContentId};
use sqlx::Row;

use crate::meta::*;

mod collab;

const IN_CHUNK: usize = 500;

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn placeholders(start: usize, n: usize) -> String {
    (start..start + n).map(|i| format!("${i}")).collect::<Vec<_>>().join(",")
}

/// `($s,$s+1,..),(..)` for `rows` rows of `cols` columns.
fn values(start: usize, rows: usize, cols: usize) -> String {
    let mut out = String::with_capacity(rows * cols * 5);
    for r in 0..rows {
        if r > 0 {
            out.push(',');
        }
        out.push('(');
        for c in 0..cols {
            if c > 0 {
                out.push(',');
            }
            out.push('$');
            out.push_str(&(start + r * cols + c).to_string());
        }
        out.push(')');
    }
    out
}

/// Rows per multi-row INSERT (keeps parameter counts well under limits).
const INSERT_CHUNK: usize = 200;
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

/// Changeset bodies are stored zstd-compressed when that helps. Canonical
/// CBOR starts with 0x89, so a zstd frame magic is unambiguous.
fn pack_body(b: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    if b.len() >= 256
        && let Ok(c) = zstd::bulk::compress(b, 3)
        && c.len() + 16 < b.len()
    {
        return std::borrow::Cow::Owned(c);
    }
    std::borrow::Cow::Borrowed(b)
}

fn unpack_body(b: Vec<u8>) -> StoreResult<Vec<u8>> {
    if b.starts_with(&ZSTD_MAGIC) {
        zstd::stream::decode_all(&b[..]).map_err(|e| StoreError::Corrupt(format!("changeset body: {e}")))
    } else {
        Ok(b)
    }
}

fn cs_id(bytes: &[u8]) -> StoreResult<ChangesetId> {
    ChangesetId::from_slice(bytes).map_err(|e| StoreError::Corrupt(e.to_string()))
}

fn arr<const N: usize>(bytes: &[u8], what: &str) -> StoreResult<[u8; N]> {
    bytes.try_into().map_err(|_| StoreError::Corrupt(format!("{what}: expected {N} bytes, got {}", bytes.len())))
}

fn review_row<R: Row>(r: R) -> StoreResult<Review>
where
    usize: sqlx::ColumnIndex<R>,
    for<'a> i64: sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    for<'a> String: sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    for<'a> Vec<u8>: sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    for<'a> Option<Vec<u8>>: sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
{
    Ok(Review {
        number: r.try_get(0)?,
        title: r.try_get(1)?,
        body: r.try_get(2)?,
        author: r.try_get(3)?,
        head: cs_id(&r.try_get::<Vec<u8>, _>(4)?)?,
        target: r.try_get(5)?,
        state: r.try_get(6)?,
        landed: r.try_get::<Option<Vec<u8>>, _>(7)?.map(|b| cs_id(&b)).transpose()?,
        created_ms: r.try_get(8)?,
        updated_ms: r.try_get(9)?,
    })
}

/// Order results to match the request order.
fn ordered<K: std::hash::Hash + Eq, V: Clone>(keys: &[K], map: &std::collections::HashMap<K, V>) -> Vec<Option<V>> {
    keys.iter().map(|k| map.get(k).cloned()).collect()
}

fn account<R: sqlx::Row>(r: &R) -> StoreResult<Account>
where
    usize: sqlx::ColumnIndex<R>,
    for<'a> i64: sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    for<'a> bool: sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    for<'a> String: sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    for<'a> Option<String>: sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
{
    Ok(Account {
        user: User { id: r.try_get(0)?, name: r.try_get(1)?, is_admin: r.try_get(2)? },
        email: r.try_get(3)?,
        display_name: r.try_get(4)?,
        disabled: r.try_get(5)?,
        created_at: r.try_get(6)?,
    })
}

fn org<R: sqlx::Row>(r: &R, o: usize) -> StoreResult<Org>
where
    usize: sqlx::ColumnIndex<R>,
    for<'a> i64: sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
    for<'a> String: sqlx::Decode<'a, R::Database> + sqlx::Type<R::Database>,
{
    Ok(Org {
        id: r.try_get(o)?,
        name: r.try_get(o + 1)?,
        display_name: r.try_get(o + 2)?,
        description: r.try_get(o + 3)?,
        base_role: r.try_get(o + 4)?,
        created_at: r.try_get(o + 5)?,
    })
}

macro_rules! sql_store {
    ($name:ident, $pool:ty) => {
        #[derive(Clone)]
        pub struct $name {
            pool: $pool,
            /// Queues write transactions in-process (SQLite has one writer;
            /// waiting here is FIFO, unlike its sleeping busy handler).
            writer: Option<std::sync::Arc<tokio::sync::Mutex<()>>>,
        }

        impl $name {
            pub fn pool(&self) -> &$pool {
                &self.pool
            }

            async fn write_lock(&self) -> Option<tokio::sync::MutexGuard<'_, ()>> {
                match &self.writer {
                    Some(w) => Some(w.lock().await),
                    None => None,
                }
            }
        }

        #[async_trait]
        impl MetaStore for $name {
            async fn create_repo(&self, name: &str) -> StoreResult<RepoId> {
                let row = sqlx::query(
                    "INSERT INTO repos (name, config, created_at) VALUES ($1, '{}', $2) \
                     ON CONFLICT (name) DO NOTHING RETURNING repo_id",
                )
                .bind(name)
                .bind(now_ms())
                .fetch_optional(&self.pool)
                .await?;
                match row {
                    Some(r) => Ok(RepoId(r.try_get::<i64, _>(0)?)),
                    None => Err(StoreError::RepoExists(name.to_string())),
                }
            }

            async fn get_repo(&self, name: &str) -> StoreResult<Option<RepoInfo>> {
                let row = sqlx::query("SELECT repo_id, name, config FROM repos WHERE name = $1")
                    .bind(name)
                    .fetch_optional(&self.pool)
                    .await?;
                row.map(|r| {
                    Ok(RepoInfo { id: RepoId(r.try_get(0)?), name: r.try_get(1)?, config: r.try_get(2)? })
                })
                .transpose()
            }

            async fn list_repos(&self) -> StoreResult<Vec<RepoInfo>> {
                let rows = sqlx::query("SELECT repo_id, name, config FROM repos ORDER BY name")
                    .fetch_all(&self.pool)
                    .await?;
                rows.into_iter()
                    .map(|r| Ok(RepoInfo { id: RepoId(r.try_get(0)?), name: r.try_get(1)?, config: r.try_get(2)? }))
                    .collect()
            }

            async fn set_repo_config(&self, repo: RepoId, config: &str) -> StoreResult<()> {
                sqlx::query("UPDATE repos SET config = $1 WHERE repo_id = $2").bind(config).bind(repo.0).execute(&self.pool).await?;
                Ok(())
            }

            async fn put_changesets(&self, repo: RepoId, batch: &[NewChangeset]) -> StoreResult<Vec<LogSeq>> {
                let _w = self.write_lock().await;
                let mut tx = self.pool.begin_with(Self::BEGIN_WRITE).await?;
                Self::lock_log(&mut tx, repo).await?;
                // Existing ids keep their original log position.
                let mut seq_of: std::collections::HashMap<ChangesetId, i64> = std::collections::HashMap::new();
                for chunk in batch.chunks(IN_CHUNK) {
                    let q = format!(
                        "SELECT cs_id, log_seq FROM changesets WHERE repo_id = $1 AND cs_id IN ({})",
                        placeholders(2, chunk.len())
                    );
                    let mut query = sqlx::query(&q).bind(repo.0);
                    for cs in chunk {
                        query = query.bind(cs.id.as_bytes().as_slice());
                    }
                    for r in query.fetch_all(&mut *tx).await? {
                        seq_of.insert(cs_id(r.try_get::<&[u8], _>(0)?)?, r.try_get(1)?);
                    }
                }
                let mut next: i64 = sqlx::query("SELECT COALESCE(MAX(log_seq), 0) + 1 FROM changesets WHERE repo_id = $1")
                    .bind(repo.0)
                    .fetch_one(&mut *tx)
                    .await?
                    .try_get(0)?;
                let mut fresh: Vec<(&NewChangeset, i64)> = Vec::new();
                for cs in batch {
                    if !seq_of.contains_key(&cs.id) {
                        seq_of.insert(cs.id, next);
                        fresh.push((cs, next));
                        next += 1;
                    }
                }
                let now = now_ms();
                let packed: Vec<std::borrow::Cow<'_, [u8]>> = {
                    use rayon::prelude::*;
                    fresh.par_iter().map(|(cs, _)| pack_body(&cs.body)).collect()
                };
                for (chunk, bodies) in fresh.chunks(INSERT_CHUNK).zip(packed.chunks(INSERT_CHUNK)) {
                    let q = format!(
                        "INSERT INTO changesets (repo_id, cs_id, gen, log_seq, created_at, body) VALUES {}",
                        values(1, chunk.len(), 6)
                    );
                    let mut query = sqlx::query(&q);
                    for ((cs, seq), body) in chunk.iter().zip(bodies) {
                        query = query
                            .bind(repo.0)
                            .bind(cs.id.as_bytes().as_slice())
                            .bind(cs.gen_num as i64)
                            .bind(*seq)
                            .bind(now)
                            .bind(body.as_ref());
                    }
                    query.execute(&mut *tx).await?;
                }
                let parents: Vec<(&ChangesetId, &ChangesetId, i32)> = fresh
                    .iter()
                    .flat_map(|(cs, _)| cs.parents.iter().enumerate().map(move |(i, p)| (p, &cs.id, i as i32)))
                    .collect();
                for chunk in parents.chunks(INSERT_CHUNK) {
                    let q = format!(
                        "INSERT INTO changeset_parents (repo_id, parent, child, idx) VALUES {} ON CONFLICT DO NOTHING",
                        values(1, chunk.len(), 4)
                    );
                    let mut query = sqlx::query(&q);
                    for (p, c, i) in chunk {
                        query = query.bind(repo.0).bind(p.as_bytes().as_slice()).bind(c.as_bytes().as_slice()).bind(*i);
                    }
                    query.execute(&mut *tx).await?;
                }
                // Derived values and git mappings for every row (existing
                // ones too: a retried batch may have lost them).
                let derived: Vec<(&ChangesetId, DerivedTypeId, &[u8])> =
                    batch.iter().flat_map(|cs| cs.derived.iter().map(move |(t, v)| (&cs.id, *t, v.as_slice()))).collect();
                for chunk in derived.chunks(INSERT_CHUNK) {
                    let q = format!(
                        "INSERT INTO derived (repo_id, type_id, cs_id, value_ref) VALUES {} ON CONFLICT DO NOTHING",
                        values(1, chunk.len(), 4)
                    );
                    let mut query = sqlx::query(&q);
                    for (id, ty, v) in chunk {
                        query = query.bind(repo.0).bind(*ty).bind(id.as_bytes().as_slice()).bind(*v);
                    }
                    query.execute(&mut *tx).await?;
                }
                let git: Vec<(&ChangesetId, &[u8; 20])> =
                    batch.iter().filter_map(|cs| cs.git_sha1.as_ref().map(|s| (&cs.id, s))).collect();
                for chunk in git.chunks(INSERT_CHUNK) {
                    let q = format!(
                        "INSERT INTO git_map (repo_id, cs_id, git_sha1) VALUES {} ON CONFLICT DO NOTHING",
                        values(1, chunk.len(), 3)
                    );
                    let mut query = sqlx::query(&q);
                    for (id, sha) in chunk {
                        query = query.bind(repo.0).bind(id.as_bytes().as_slice()).bind(sha.as_slice());
                    }
                    query.execute(&mut *tx).await?;
                }
                tx.commit().await?;
                Ok(batch.iter().map(|cs| seq_of[&cs.id]).collect())
            }

            async fn get_changesets(&self, repo: RepoId, ids: &[ChangesetId]) -> StoreResult<Vec<Option<StoredChangeset>>> {
                let mut found = std::collections::HashMap::new();
                for chunk in ids.chunks(IN_CHUNK) {
                    let q = format!(
                        "SELECT cs_id, gen, log_seq, body FROM changesets WHERE repo_id = $1 AND cs_id IN ({})",
                        placeholders(2, chunk.len())
                    );
                    let mut query = sqlx::query(&q).bind(repo.0);
                    for id in chunk {
                        query = query.bind(id.as_bytes().as_slice());
                    }
                    for r in query.fetch_all(&self.pool).await? {
                        let id = cs_id(r.try_get::<&[u8], _>(0)?)?;
                        found.insert(
                            id,
                            StoredChangeset {
                                id,
                                gen_num: r.try_get::<i64, _>(1)? as u64,
                                log_seq: r.try_get(2)?,
                                body: unpack_body(r.try_get::<Vec<u8>, _>(3)?)?,
                            },
                        );
                    }
                }
                Ok(ordered(ids, &found))
            }

            async fn get_gens(&self, repo: RepoId, ids: &[ChangesetId]) -> StoreResult<Vec<Option<u64>>> {
                let mut found = std::collections::HashMap::new();
                for chunk in ids.chunks(IN_CHUNK) {
                    let q = format!(
                        "SELECT cs_id, gen FROM changesets WHERE repo_id = $1 AND cs_id IN ({})",
                        placeholders(2, chunk.len())
                    );
                    let mut query = sqlx::query(&q).bind(repo.0);
                    for id in chunk {
                        query = query.bind(id.as_bytes().as_slice());
                    }
                    for r in query.fetch_all(&self.pool).await? {
                        found.insert(cs_id(r.try_get::<&[u8], _>(0)?)?, r.try_get::<i64, _>(1)? as u64);
                    }
                }
                Ok(ordered(ids, &found))
            }

            async fn log_tail(&self, repo: RepoId, after: LogSeq, limit: u32) -> StoreResult<Vec<(LogSeq, ChangesetId)>> {
                let rows = sqlx::query(
                    "SELECT log_seq, cs_id FROM changesets WHERE repo_id = $1 AND log_seq > $2 ORDER BY log_seq LIMIT $3",
                )
                .bind(repo.0)
                .bind(after)
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await?;
                rows.into_iter().map(|r| Ok((r.try_get(0)?, cs_id(r.try_get::<&[u8], _>(1)?)?))).collect()
            }

            async fn children(&self, repo: RepoId, id: &ChangesetId) -> StoreResult<Vec<ChangesetId>> {
                let rows = sqlx::query("SELECT child FROM changeset_parents WHERE repo_id = $1 AND parent = $2")
                    .bind(repo.0)
                    .bind(id.as_bytes().as_slice())
                    .fetch_all(&self.pool)
                    .await?;
                rows.into_iter().map(|r| cs_id(r.try_get::<&[u8], _>(0)?)).collect()
            }

            async fn resolve_prefix(&self, repo: RepoId, hex_prefix: &str, limit: u32) -> StoreResult<Vec<ChangesetId>> {
                let Some((lo, hi)) = prefix_range(hex_prefix) else {
                    return Ok(vec![]);
                };
                let rows = match hi {
                    Some(hi) => sqlx::query(
                        "SELECT cs_id FROM changesets WHERE repo_id = $1 AND cs_id >= $2 AND cs_id < $3 \
                         ORDER BY cs_id LIMIT $4",
                    )
                    .bind(repo.0)
                    .bind(lo)
                    .bind(hi)
                    .bind(limit as i64)
                    .fetch_all(&self.pool)
                    .await?,
                    None => sqlx::query(
                        "SELECT cs_id FROM changesets WHERE repo_id = $1 AND cs_id >= $2 ORDER BY cs_id LIMIT $3",
                    )
                    .bind(repo.0)
                    .bind(lo)
                    .bind(limit as i64)
                    .fetch_all(&self.pool)
                    .await?,
                };
                rows.into_iter().map(|r| cs_id(r.try_get::<&[u8], _>(0)?)).collect()
            }

            async fn get_ref(&self, repo: RepoId, name: &[u8]) -> StoreResult<Option<RefValue>> {
                let row = sqlx::query("SELECT cs_id, version FROM refs WHERE repo_id = $1 AND name = $2")
                    .bind(repo.0)
                    .bind(name)
                    .fetch_optional(&self.pool)
                    .await?;
                row.map(|r| {
                    Ok(RefValue { target: cs_id(r.try_get::<&[u8], _>(0)?)?, version: r.try_get::<i64, _>(1)? as u64 })
                })
                .transpose()
            }

            async fn cas_ref(
                &self,
                repo: RepoId,
                name: &[u8],
                expect: RefExpect,
                new: Option<ChangesetId>,
                log: RefLogEntry,
            ) -> StoreResult<CasOutcome> {
                let _w = self.write_lock().await;
                let mut tx = self.pool.begin_with(Self::BEGIN_WRITE).await?;
                // Take the write lock up front with a no-op write so the read
                // below cannot be invalidated by a concurrent writer.
                Self::lock_ref(&mut tx, repo, name).await?;
                let current = sqlx::query("SELECT cs_id, version FROM refs WHERE repo_id = $1 AND name = $2")
                    .bind(repo.0)
                    .bind(name)
                    .fetch_optional(&mut *tx)
                    .await?
                    .map(|r| -> StoreResult<RefValue> {
                        Ok(RefValue { target: cs_id(r.try_get::<&[u8], _>(0)?)?, version: r.try_get::<i64, _>(1)? as u64 })
                    })
                    .transpose()?;
                let ok = match (expect, current) {
                    (RefExpect::Any, _) => true,
                    (RefExpect::Absent, c) => c.is_none(),
                    (RefExpect::Version(v), Some(c)) => c.version == v,
                    (RefExpect::Target(t), Some(c)) => c.target == t,
                    (_, None) => false,
                };
                if !ok {
                    return Ok(CasOutcome::Mismatch { current });
                }
                let outcome = match new {
                    Some(target) => {
                        let version = current.map(|c| c.version + 1).unwrap_or(1);
                        sqlx::query(
                            "INSERT INTO refs (repo_id, name, cs_id, version) VALUES ($1, $2, $3, $4) \
                             ON CONFLICT (repo_id, name) DO UPDATE SET cs_id = excluded.cs_id, version = excluded.version",
                        )
                        .bind(repo.0)
                        .bind(name)
                        .bind(target.as_bytes().as_slice())
                        .bind(version as i64)
                        .execute(&mut *tx)
                        .await?;
                        CasOutcome::Updated { version }
                    }
                    None => {
                        sqlx::query("DELETE FROM refs WHERE repo_id = $1 AND name = $2")
                            .bind(repo.0)
                            .bind(name)
                            .execute(&mut *tx)
                            .await?;
                        CasOutcome::Deleted
                    }
                };
                sqlx::query(
                    "INSERT INTO ref_log (repo_id, name, old_cs, new_cs, actor, reason, ts) VALUES ($1, $2, $3, $4, $5, $6, $7)",
                )
                .bind(repo.0)
                .bind(name)
                .bind(current.map(|c| c.target.as_bytes().to_vec()))
                .bind(new.map(|n| n.as_bytes().to_vec()))
                .bind(&log.actor)
                .bind(&log.reason)
                .bind(now_ms())
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(outcome)
            }

            async fn list_refs(&self, repo: RepoId, prefix: &[u8], limit: u32) -> StoreResult<Vec<(Vec<u8>, RefValue)>> {
                let rows = sqlx::query(
                    "SELECT name, cs_id, version FROM refs WHERE repo_id = $1 AND name >= $2 ORDER BY name LIMIT $3",
                )
                .bind(repo.0)
                .bind(prefix)
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await?;
                let mut out = Vec::new();
                for r in rows {
                    let name: Vec<u8> = r.try_get(0)?;
                    if !name.starts_with(prefix) {
                        break;
                    }
                    out.push((
                        name,
                        RefValue { target: cs_id(r.try_get::<&[u8], _>(1)?)?, version: r.try_get::<i64, _>(2)? as u64 },
                    ));
                }
                Ok(out)
            }

            async fn get_derived(&self, repo: RepoId, ty: DerivedTypeId, ids: &[ChangesetId]) -> StoreResult<Vec<Option<Vec<u8>>>> {
                let mut found = std::collections::HashMap::new();
                for chunk in ids.chunks(IN_CHUNK) {
                    let q = format!(
                        "SELECT cs_id, value_ref FROM derived WHERE repo_id = $1 AND type_id = $2 AND cs_id IN ({})",
                        placeholders(3, chunk.len())
                    );
                    let mut query = sqlx::query(&q).bind(repo.0).bind(ty);
                    for id in chunk {
                        query = query.bind(id.as_bytes().as_slice());
                    }
                    for r in query.fetch_all(&self.pool).await? {
                        found.insert(cs_id(r.try_get::<&[u8], _>(0)?)?, r.try_get::<Vec<u8>, _>(1)?);
                    }
                }
                Ok(ordered(ids, &found))
            }

            async fn put_derived(&self, repo: RepoId, ty: DerivedTypeId, entries: &[(ChangesetId, Vec<u8>)]) -> StoreResult<()> {
                let _w = self.write_lock().await;
                let mut tx = self.pool.begin_with(Self::BEGIN_WRITE).await?;
                for chunk in entries.chunks(INSERT_CHUNK) {
                    let q = format!(
                        "INSERT INTO derived (repo_id, type_id, cs_id, value_ref) VALUES {} ON CONFLICT DO NOTHING",
                        values(1, chunk.len(), 4)
                    );
                    let mut query = sqlx::query(&q);
                    for (id, value) in chunk {
                        query = query.bind(repo.0).bind(ty).bind(id.as_bytes().as_slice()).bind(value.as_slice());
                    }
                    query.execute(&mut *tx).await?;
                }
                tx.commit().await?;
                Ok(())
            }

            async fn derived_ids(&self, repo: RepoId, ty: DerivedTypeId) -> StoreResult<Vec<ChangesetId>> {
                let rows = sqlx::query("SELECT cs_id FROM derived WHERE repo_id = $1 AND type_id = $2")
                    .bind(repo.0)
                    .bind(ty)
                    .fetch_all(&self.pool)
                    .await?;
                rows.into_iter().map(|r| cs_id(r.try_get::<&[u8], _>(0)?)).collect()
            }

            async fn get_content_meta(&self, ids: &[ContentId]) -> StoreResult<Vec<Option<ContentMeta>>> {
                let mut found = std::collections::HashMap::new();
                for chunk in ids.chunks(IN_CHUNK) {
                    let q = format!(
                        "SELECT content_id, size, sha1, sha256, is_binary FROM content_meta WHERE content_id IN ({})",
                        placeholders(1, chunk.len())
                    );
                    let mut query = sqlx::query(&q);
                    for id in chunk {
                        query = query.bind(id.as_bytes().as_slice());
                    }
                    for r in query.fetch_all(&self.pool).await? {
                        let id = ContentId::from_slice(r.try_get::<&[u8], _>(0)?)
                            .map_err(|e| StoreError::Corrupt(e.to_string()))?;
                        found.insert(
                            id,
                            ContentMeta {
                                size: r.try_get::<i64, _>(1)? as u64,
                                git_sha1: arr(r.try_get::<&[u8], _>(2)?, "sha1")?,
                                sha256: arr(r.try_get::<&[u8], _>(3)?, "sha256")?,
                                is_binary: r.try_get(4)?,
                            },
                        );
                    }
                }
                Ok(ordered(ids, &found))
            }

            async fn put_content_meta(&self, metas: &[(ContentId, ContentMeta)]) -> StoreResult<()> {
                if metas.is_empty() {
                    return Ok(());
                }
                let _w = self.write_lock().await;
                let mut tx = self.pool.begin_with(Self::BEGIN_WRITE).await?;
                for chunk in metas.chunks(INSERT_CHUNK) {
                    let q = format!(
                        "INSERT INTO content_meta (content_id, size, sha1, sha256, is_binary) VALUES {} ON CONFLICT DO NOTHING",
                        values(1, chunk.len(), 5)
                    );
                    let mut query = sqlx::query(&q);
                    for (id, m) in chunk {
                        query = query
                            .bind(id.as_bytes().as_slice())
                            .bind(m.size as i64)
                            .bind(m.git_sha1.as_slice())
                            .bind(m.sha256.as_slice())
                            .bind(m.is_binary);
                    }
                    query.execute(&mut *tx).await?;
                }
                tx.commit().await?;
                Ok(())
            }

            async fn git_map_put(&self, repo: RepoId, entries: &[(ChangesetId, [u8; 20])]) -> StoreResult<()> {
                let _w = self.write_lock().await;
                let mut tx = self.pool.begin_with(Self::BEGIN_WRITE).await?;
                for chunk in entries.chunks(INSERT_CHUNK) {
                    let q = format!(
                        "INSERT INTO git_map (repo_id, cs_id, git_sha1) VALUES {} ON CONFLICT DO NOTHING",
                        values(1, chunk.len(), 3)
                    );
                    let mut query = sqlx::query(&q);
                    for (id, sha) in chunk {
                        query = query.bind(repo.0).bind(id.as_bytes().as_slice()).bind(sha.as_slice());
                    }
                    query.execute(&mut *tx).await?;
                }
                tx.commit().await?;
                Ok(())
            }

            async fn git_to_cs(&self, repo: RepoId, shas: &[[u8; 20]]) -> StoreResult<Vec<Option<ChangesetId>>> {
                let mut found = std::collections::HashMap::new();
                for chunk in shas.chunks(IN_CHUNK) {
                    let q = format!(
                        "SELECT git_sha1, cs_id FROM git_map WHERE repo_id = $1 AND git_sha1 IN ({})",
                        placeholders(2, chunk.len())
                    );
                    let mut query = sqlx::query(&q).bind(repo.0);
                    for s in chunk {
                        query = query.bind(s.as_slice());
                    }
                    for r in query.fetch_all(&self.pool).await? {
                        found.insert(arr::<20>(r.try_get::<&[u8], _>(0)?, "sha1")?, cs_id(r.try_get::<&[u8], _>(1)?)?);
                    }
                }
                Ok(ordered(shas, &found))
            }

            async fn cs_to_git(&self, repo: RepoId, ids: &[ChangesetId]) -> StoreResult<Vec<Option<[u8; 20]>>> {
                let mut found = std::collections::HashMap::new();
                for chunk in ids.chunks(IN_CHUNK) {
                    let q = format!(
                        "SELECT cs_id, git_sha1 FROM git_map WHERE repo_id = $1 AND cs_id IN ({})",
                        placeholders(2, chunk.len())
                    );
                    let mut query = sqlx::query(&q).bind(repo.0);
                    for id in chunk {
                        query = query.bind(id.as_bytes().as_slice());
                    }
                    for r in query.fetch_all(&self.pool).await? {
                        found.insert(cs_id(r.try_get::<&[u8], _>(0)?)?, arr::<20>(r.try_get::<&[u8], _>(1)?, "sha1")?);
                    }
                }
                Ok(ordered(ids, &found))
            }

            async fn backup_to(&self, path: &std::path::Path) -> StoreResult<()> {
                Self::backup_impl(&self.pool, path).await
            }

            async fn changeset_count(&self, repo: RepoId) -> StoreResult<u64> {
                let n: i64 = sqlx::query("SELECT COUNT(*) FROM changesets WHERE repo_id = $1")
                    .bind(repo.0)
                    .fetch_one(&self.pool)
                    .await?
                    .try_get(0)?;
                Ok(n as u64)
            }


            async fn git_trees_get(&self, ids: &[flux_core::TreeId]) -> StoreResult<Vec<Option<[u8; 20]>>> {
                let mut found = std::collections::HashMap::new();
                for chunk in ids.chunks(IN_CHUNK) {
                    let q = format!("SELECT tree_id, git_sha FROM git_trees WHERE tree_id IN ({})", placeholders(1, chunk.len()));
                    let mut query = sqlx::query(&q);
                    for id in chunk {
                        query = query.bind(id.as_bytes().as_slice());
                    }
                    for r in query.fetch_all(&self.pool).await? {
                        let t = flux_core::TreeId::from_slice(r.try_get::<&[u8], _>(0)?).map_err(|e| StoreError::Corrupt(e.to_string()))?;
                        found.insert(t, arr::<20>(r.try_get::<&[u8], _>(1)?, "sha1")?);
                    }
                }
                Ok(ordered(ids, &found))
            }

            async fn git_trees_put(&self, pairs: &[(flux_core::TreeId, [u8; 20])]) -> StoreResult<()> {
                let _w = self.write_lock().await;
                let mut tx = self.pool.begin_with(Self::BEGIN_WRITE).await?;
                for chunk in pairs.chunks(INSERT_CHUNK) {
                    let q = format!("INSERT INTO git_trees (tree_id, git_sha) VALUES {} ON CONFLICT DO NOTHING", values(1, chunk.len(), 2));
                    let mut query = sqlx::query(&q);
                    for (t, sha) in chunk {
                        query = query.bind(t.as_bytes().as_slice()).bind(sha.as_slice());
                    }
                    query.execute(&mut *tx).await?;
                }
                tx.commit().await?;
                Ok(())
            }

            async fn git_tree_by_sha(&self, sha: &[u8; 20]) -> StoreResult<Option<flux_core::TreeId>> {
                let row = sqlx::query("SELECT tree_id FROM git_trees WHERE git_sha = $1 LIMIT 1")
                    .bind(sha.as_slice())
                    .fetch_optional(&self.pool)
                    .await?;
                row.map(|r| flux_core::TreeId::from_slice(r.try_get::<&[u8], _>(0)?).map_err(|e| StoreError::Corrupt(e.to_string())))
                    .transpose()
            }

            async fn git_commits_put(&self, repo: RepoId, pairs: &[(ChangesetId, [u8; 20])]) -> StoreResult<()> {
                let _w = self.write_lock().await;
                let mut tx = self.pool.begin_with(Self::BEGIN_WRITE).await?;
                for chunk in pairs.chunks(INSERT_CHUNK) {
                    let q = format!(
                        "INSERT INTO git_commits (repo_id, git_sha, cs_id) VALUES {} ON CONFLICT DO NOTHING",
                        values(1, chunk.len(), 3)
                    );
                    let mut query = sqlx::query(&q);
                    for (c, sha) in chunk {
                        query = query.bind(repo.0).bind(sha.as_slice()).bind(c.as_bytes().as_slice());
                    }
                    query.execute(&mut *tx).await?;
                }
                tx.commit().await?;
                Ok(())
            }

            async fn git_commits_lookup(&self, repo: RepoId, shas: &[[u8; 20]]) -> StoreResult<Vec<Option<ChangesetId>>> {
                let mut found = std::collections::HashMap::new();
                for chunk in shas.chunks(IN_CHUNK) {
                    let q = format!(
                        "SELECT git_sha, cs_id FROM git_commits WHERE repo_id = $1 AND git_sha IN ({})",
                        placeholders(2, chunk.len())
                    );
                    let mut query = sqlx::query(&q).bind(repo.0);
                    for s in chunk {
                        query = query.bind(s.as_slice());
                    }
                    for r in query.fetch_all(&self.pool).await? {
                        found.insert(arr::<20>(r.try_get::<&[u8], _>(0)?, "sha1")?, cs_id(r.try_get::<&[u8], _>(1)?)?);
                    }
                }
                Ok(ordered(shas, &found))
            }

            async fn git_commits_of(&self, repo: RepoId, ids: &[ChangesetId]) -> StoreResult<Vec<Option<[u8; 20]>>> {
                let mut found = std::collections::HashMap::new();
                for chunk in ids.chunks(IN_CHUNK) {
                    let q = format!(
                        "SELECT cs_id, git_sha FROM git_commits WHERE repo_id = $1 AND cs_id IN ({})",
                        placeholders(2, chunk.len())
                    );
                    let mut query = sqlx::query(&q).bind(repo.0);
                    for id in chunk {
                        query = query.bind(id.as_bytes().as_slice());
                    }
                    for r in query.fetch_all(&self.pool).await? {
                        found.insert(cs_id(r.try_get::<&[u8], _>(0)?)?, arr::<20>(r.try_get::<&[u8], _>(1)?, "sha1")?);
                    }
                }
                Ok(ordered(ids, &found))
            }

            async fn git_pack_get(&self, repo: RepoId) -> StoreResult<Option<GitPack>> {
                let Some(r) = sqlx::query("SELECT content_id, objects, tips, created_ms FROM git_packs WHERE repo_id = $1")
                    .bind(repo.0)
                    .fetch_optional(&self.pool)
                    .await?
                else {
                    return Ok(None);
                };
                let tips: &[u8] = r.try_get(2)?;
                if tips.len() % 32 != 0 {
                    return Err(StoreError::Corrupt("git_packs.tips length".into()));
                }
                Ok(Some(GitPack {
                    content: ContentId::from_bytes(arr::<32>(r.try_get::<&[u8], _>(0)?, "content id")?),
                    objects: r.try_get::<i64, _>(1)? as u32,
                    tips: tips.chunks(32).map(cs_id).collect::<StoreResult<_>>()?,
                    created_ms: r.try_get(3)?,
                }))
            }

            async fn git_pack_put(&self, repo: RepoId, pack: &GitPack) -> StoreResult<()> {
                let tips: Vec<u8> = pack.tips.iter().flat_map(|t| *t.as_bytes()).collect();
                sqlx::query(
                    "INSERT INTO git_packs (repo_id, content_id, objects, tips, created_ms) VALUES ($1, $2, $3, $4, $5) \
                     ON CONFLICT (repo_id) DO UPDATE SET content_id = excluded.content_id, objects = excluded.objects, \
                     tips = excluded.tips, created_ms = excluded.created_ms",
                )
                .bind(repo.0)
                .bind(pack.content.as_bytes().as_slice())
                .bind(pack.objects as i64)
                .bind(tips)
                .bind(pack.created_ms)
                .execute(&self.pool)
                .await?;
                Ok(())
            }

            async fn content_by_sha1(&self, shas: &[[u8; 20]]) -> StoreResult<Vec<Option<ContentId>>> {
                let mut found = std::collections::HashMap::new();
                for chunk in shas.chunks(IN_CHUNK) {
                    let q = format!("SELECT sha1, content_id FROM content_meta WHERE sha1 IN ({})", placeholders(1, chunk.len()));
                    let mut query = sqlx::query(&q);
                    for s in chunk {
                        query = query.bind(s.as_slice());
                    }
                    for r in query.fetch_all(&self.pool).await? {
                        let c = ContentId::from_slice(r.try_get::<&[u8], _>(1)?).map_err(|e| StoreError::Corrupt(e.to_string()))?;
                        found.insert(arr::<20>(r.try_get::<&[u8], _>(0)?, "sha1")?, c);
                    }
                }
                Ok(ordered(shas, &found))
            }

            async fn graph_tail(&self, repo: RepoId, after: LogSeq, limit: u32) -> StoreResult<Vec<GraphRow>> {
                let rows = sqlx::query(
                    "SELECT log_seq, cs_id, gen FROM changesets WHERE repo_id = $1 AND log_seq > $2 ORDER BY log_seq LIMIT $3",
                )
                .bind(repo.0)
                .bind(after)
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await?;
                let mut out: Vec<GraphRow> = Vec::with_capacity(rows.len());
                for r in rows {
                    out.push(GraphRow {
                        seq: r.try_get(0)?,
                        id: cs_id(r.try_get::<&[u8], _>(1)?)?,
                        gen_num: r.try_get::<i64, _>(2)? as u64,
                        parents: vec![],
                    });
                }
                // Parents by child id in batches: always an index lookup on
                // (repo_id, child), whatever the planner's statistics say.
                let pos: std::collections::HashMap<ChangesetId, usize> =
                    out.iter().enumerate().map(|(i, r)| (r.id, i)).collect();
                let mut edges: Vec<(usize, i32, ChangesetId)> = Vec::new();
                let ids: Vec<ChangesetId> = out.iter().map(|r| r.id).collect();
                for chunk in ids.chunks(IN_CHUNK) {
                    let q = format!(
                        "SELECT child, parent, idx FROM changeset_parents WHERE repo_id = $1 AND child IN ({})",
                        placeholders(2, chunk.len())
                    );
                    let mut query = sqlx::query(&q).bind(repo.0);
                    for id in chunk {
                        query = query.bind(id.as_bytes().as_slice());
                    }
                    for r in query.fetch_all(&self.pool).await? {
                        let child = cs_id(r.try_get::<&[u8], _>(0)?)?;
                        if let Some(&i) = pos.get(&child) {
                            edges.push((i, r.try_get(2)?, cs_id(r.try_get::<&[u8], _>(1)?)?));
                        }
                    }
                }
                edges.sort_by_key(|e| (e.0, e.1));
                for (i, _, p) in edges {
                    out[i].parents.push(p);
                }
                Ok(out)
            }

            async fn ref_log(&self, repo: RepoId, before: Option<i64>, limit: u32) -> StoreResult<Vec<RefLogRow>> {
                let rows = sqlx::query(
                    "SELECT seq, name, old_cs, new_cs, actor, reason, ts FROM ref_log \
                     WHERE repo_id = $1 AND seq < $2 ORDER BY seq DESC LIMIT $3",
                )
                .bind(repo.0)
                .bind(before.unwrap_or(i64::MAX))
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await?;
                rows.into_iter()
                    .map(|r| {
                        let opt = |v: Option<Vec<u8>>| v.map(|b| cs_id(&b)).transpose();
                        Ok(RefLogRow {
                            seq: r.try_get(0)?,
                            name: r.try_get(1)?,
                            old: opt(r.try_get(2)?)?,
                            new: opt(r.try_get(3)?)?,
                            actor: r.try_get(4)?,
                            reason: r.try_get(5)?,
                            ts_ms: r.try_get(6)?,
                        })
                    })
                    .collect()
            }

            async fn create_user(&self, name: &str, is_admin: bool) -> StoreResult<User> {
                let row = sqlx::query(
                    "INSERT INTO users (name, is_admin, created_at) VALUES ($1, $2, $3) ON CONFLICT (name) DO NOTHING RETURNING user_id",
                )
                .bind(name)
                .bind(is_admin)
                .bind(now_ms())
                .fetch_optional(&self.pool)
                .await?;
                match row {
                    Some(r) => Ok(User { id: r.try_get(0)?, name: name.to_string(), is_admin }),
                    None => Err(StoreError::Backend(anyhow::anyhow!("user {name:?} already exists"))),
                }
            }

            async fn get_user(&self, name: &str) -> StoreResult<Option<User>> {
                let row = sqlx::query("SELECT user_id, name, is_admin FROM users WHERE name = $1")
                    .bind(name)
                    .fetch_optional(&self.pool)
                    .await?;
                row.map(|r| Ok(User { id: r.try_get(0)?, name: r.try_get(1)?, is_admin: r.try_get(2)? })).transpose()
            }

            async fn list_users(&self) -> StoreResult<Vec<User>> {
                let rows = sqlx::query("SELECT user_id, name, is_admin FROM users ORDER BY name").fetch_all(&self.pool).await?;
                rows.into_iter().map(|r| Ok(User { id: r.try_get(0)?, name: r.try_get(1)?, is_admin: r.try_get(2)? })).collect()
            }

            async fn add_token(&self, user_id: i64, token_hash: &[u8; 32], label: &str) -> StoreResult<()> {
                sqlx::query("INSERT INTO tokens (token_hash, user_id, label, created_at) VALUES ($1, $2, $3, $4)")
                    .bind(token_hash.as_slice())
                    .bind(user_id)
                    .bind(label)
                    .bind(now_ms())
                    .execute(&self.pool)
                    .await?;
                Ok(())
            }

            async fn user_for_token(&self, token_hash: &[u8; 32]) -> StoreResult<Option<User>> {
                let row = sqlx::query(
                    "SELECT u.user_id, u.name, u.is_admin FROM tokens t JOIN users u ON u.user_id = t.user_id WHERE t.token_hash = $1",
                )
                .bind(token_hash.as_slice())
                .fetch_optional(&self.pool)
                .await?;
                row.map(|r| Ok(User { id: r.try_get(0)?, name: r.try_get(1)?, is_admin: r.try_get(2)? })).transpose()
            }

            async fn revoke_tokens(&self, user_id: i64) -> StoreResult<u64> {
                Ok(sqlx::query("DELETE FROM tokens WHERE user_id = $1").bind(user_id).execute(&self.pool).await?.rows_affected())
            }

            async fn create_account(&self, a: &NewAccount) -> StoreResult<User> {
                let _w = self.write_lock().await;
                let mut tx = self.pool.begin_with(Self::BEGIN_WRITE).await?;
                let taken: Option<i64> = sqlx::query_scalar("SELECT org_id FROM orgs WHERE name = $1").bind(&a.name).fetch_optional(&mut *tx).await?;
                if taken.is_some() {
                    return Err(StoreError::Conflict(format!("the name {:?} is taken", a.name)));
                }
                if let Some(email) = &a.email {
                    let used: Option<i64> =
                        sqlx::query_scalar("SELECT user_id FROM users WHERE email = $1").bind(email).fetch_optional(&mut *tx).await?;
                    if used.is_some() {
                        return Err(StoreError::Conflict("an account with that email already exists".into()));
                    }
                }
                let row = sqlx::query(
                    "INSERT INTO users (name, is_admin, created_at, email, display_name, password_hash) VALUES ($1, $2, $3, $4, $5, $6) \
                     ON CONFLICT (name) DO NOTHING RETURNING user_id",
                )
                .bind(&a.name)
                .bind(a.is_admin)
                .bind(now_ms())
                .bind(&a.email)
                .bind(&a.display_name)
                .bind(&a.password_hash)
                .fetch_optional(&mut *tx)
                .await?;
                let Some(r) = row else { return Err(StoreError::Conflict(format!("the name {:?} is taken", a.name))) };
                tx.commit().await?;
                Ok(User { id: r.try_get(0)?, name: a.name.clone(), is_admin: a.is_admin })
            }

            async fn get_account(&self, name_or_email: &str) -> StoreResult<Option<(Account, Option<String>)>> {
                let row = sqlx::query(
                    "SELECT user_id, name, is_admin, email, display_name, disabled, created_at, password_hash FROM users \
                     WHERE name = $1 OR email = $1 ORDER BY (name = $1) DESC LIMIT 1",
                )
                .bind(name_or_email)
                .fetch_optional(&self.pool)
                .await?;
                row.map(|r| Ok((account(&r)?, r.try_get(7)?))).transpose()
            }

            async fn get_account_by_id(&self, user_id: i64) -> StoreResult<Option<Account>> {
                let row = sqlx::query("SELECT user_id, name, is_admin, email, display_name, disabled, created_at FROM users WHERE user_id = $1")
                    .bind(user_id)
                    .fetch_optional(&self.pool)
                    .await?;
                row.map(|r| account(&r)).transpose()
            }

            async fn update_account(&self, user_id: i64, u: &AccountUpdate) -> StoreResult<()> {
                let _w = self.write_lock().await;
                let mut tx = self.pool.begin_with(Self::BEGIN_WRITE).await?;
                if let Some(d) = &u.display_name {
                    sqlx::query("UPDATE users SET display_name = $1 WHERE user_id = $2").bind(d).bind(user_id).execute(&mut *tx).await?;
                }
                if let Some(e) = &u.email {
                    if let Some(email) = e {
                        let used: Option<i64> = sqlx::query_scalar("SELECT user_id FROM users WHERE email = $1 AND user_id <> $2")
                            .bind(email)
                            .bind(user_id)
                            .fetch_optional(&mut *tx)
                            .await?;
                        if used.is_some() {
                            return Err(StoreError::Conflict("an account with that email already exists".into()));
                        }
                    }
                    sqlx::query("UPDATE users SET email = $1 WHERE user_id = $2").bind(e).bind(user_id).execute(&mut *tx).await?;
                }
                if let Some(p) = &u.password_hash {
                    sqlx::query("UPDATE users SET password_hash = $1 WHERE user_id = $2").bind(p).bind(user_id).execute(&mut *tx).await?;
                }
                if let Some(a) = u.is_admin {
                    sqlx::query("UPDATE users SET is_admin = $1 WHERE user_id = $2").bind(a).bind(user_id).execute(&mut *tx).await?;
                }
                if let Some(d) = u.disabled {
                    sqlx::query("UPDATE users SET disabled = $1 WHERE user_id = $2").bind(d).bind(user_id).execute(&mut *tx).await?;
                }
                tx.commit().await?;
                Ok(())
            }

            async fn list_accounts(&self) -> StoreResult<Vec<Account>> {
                let rows = sqlx::query("SELECT user_id, name, is_admin, email, display_name, disabled, created_at FROM users ORDER BY name")
                    .fetch_all(&self.pool)
                    .await?;
                rows.iter().map(account).collect()
            }

            async fn user_count(&self) -> StoreResult<i64> {
                Ok(sqlx::query_scalar("SELECT COUNT(*) FROM users").fetch_one(&self.pool).await?)
            }

            async fn add_token_ext(
                &self,
                user_id: i64,
                token_hash: &[u8; 32],
                label: &str,
                kind: &str,
                scopes: &str,
                expires_at: Option<i64>,
            ) -> StoreResult<()> {
                sqlx::query(
                    "INSERT INTO tokens (token_hash, user_id, label, created_at, kind, scopes, expires_at) VALUES ($1, $2, $3, $4, $5, $6, $7)",
                )
                .bind(token_hash.as_slice())
                .bind(user_id)
                .bind(label)
                .bind(now_ms())
                .bind(kind)
                .bind(scopes)
                .bind(expires_at)
                .execute(&self.pool)
                .await?;
                Ok(())
            }

            async fn token_auth(&self, token_hash: &[u8; 32], now: i64) -> StoreResult<Option<TokenAuth>> {
                let row = sqlx::query(
                    "SELECT u.user_id, u.name, u.is_admin, t.kind, t.scopes FROM tokens t JOIN users u ON u.user_id = t.user_id \
                     WHERE t.token_hash = $1 AND (t.expires_at IS NULL OR t.expires_at > $2) AND u.disabled = $3",
                )
                .bind(token_hash.as_slice())
                .bind(now)
                .bind(false)
                .fetch_optional(&self.pool)
                .await?;
                row.map(|r| {
                    Ok(TokenAuth {
                        user: User { id: r.try_get(0)?, name: r.try_get(1)?, is_admin: r.try_get(2)? },
                        kind: r.try_get(3)?,
                        scopes: r.try_get(4)?,
                    })
                })
                .transpose()
            }

            async fn touch_token(&self, token_hash: &[u8; 32], now: i64) -> StoreResult<()> {
                sqlx::query("UPDATE tokens SET last_used = $1 WHERE token_hash = $2").bind(now).bind(token_hash.as_slice()).execute(&self.pool).await?;
                Ok(())
            }

            async fn list_tokens(&self, user_id: i64) -> StoreResult<Vec<TokenInfo>> {
                let rows = sqlx::query(
                    "SELECT token_hash, label, kind, scopes, created_at, expires_at, last_used FROM tokens WHERE user_id = $1 ORDER BY created_at DESC",
                )
                .bind(user_id)
                .fetch_all(&self.pool)
                .await?;
                rows.into_iter()
                    .map(|r| {
                        let h: &[u8] = r.try_get(0)?;
                        Ok(TokenInfo {
                            id: hex::encode(&h[..8.min(h.len())]),
                            label: r.try_get(1)?,
                            kind: r.try_get(2)?,
                            scopes: r.try_get(3)?,
                            created_at: r.try_get(4)?,
                            expires_at: r.try_get(5)?,
                            last_used: r.try_get(6)?,
                        })
                    })
                    .collect()
            }

            async fn revoke_token(&self, user_id: i64, id: &str) -> StoreResult<bool> {
                let hashes: Vec<Vec<u8>> =
                    sqlx::query_scalar("SELECT token_hash FROM tokens WHERE user_id = $1").bind(user_id).fetch_all(&self.pool).await?;
                for h in hashes {
                    if hex::encode(&h[..8.min(h.len())]) == id {
                        sqlx::query("DELETE FROM tokens WHERE token_hash = $1").bind(&h).execute(&self.pool).await?;
                        return Ok(true);
                    }
                }
                Ok(false)
            }

            async fn revoke_token_hash(&self, token_hash: &[u8; 32]) -> StoreResult<()> {
                sqlx::query("DELETE FROM tokens WHERE token_hash = $1").bind(token_hash.as_slice()).execute(&self.pool).await?;
                Ok(())
            }

            async fn create_org(&self, name: &str, display_name: &str, owner_user_id: i64) -> StoreResult<Org> {
                let _w = self.write_lock().await;
                let mut tx = self.pool.begin_with(Self::BEGIN_WRITE).await?;
                let taken: Option<i64> = sqlx::query_scalar("SELECT user_id FROM users WHERE name = $1").bind(name).fetch_optional(&mut *tx).await?;
                if taken.is_some() {
                    return Err(StoreError::Conflict(format!("the name {name:?} is taken")));
                }
                let now = now_ms();
                let row = sqlx::query(
                    "INSERT INTO orgs (name, display_name, created_at) VALUES ($1, $2, $3) ON CONFLICT (name) DO NOTHING RETURNING org_id",
                )
                .bind(name)
                .bind(display_name)
                .bind(now)
                .fetch_optional(&mut *tx)
                .await?;
                let Some(r) = row else { return Err(StoreError::Conflict(format!("the name {name:?} is taken"))) };
                let id: i64 = r.try_get(0)?;
                sqlx::query("INSERT INTO org_members (org_id, user_id, role) VALUES ($1, $2, 'owner')")
                    .bind(id)
                    .bind(owner_user_id)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                Ok(Org {
                    id,
                    name: name.to_string(),
                    display_name: display_name.to_string(),
                    description: String::new(),
                    base_role: "read".into(),
                    created_at: now,
                })
            }

            async fn get_org(&self, name: &str) -> StoreResult<Option<Org>> {
                let row = sqlx::query("SELECT org_id, name, display_name, description, base_role, created_at FROM orgs WHERE name = $1")
                    .bind(name)
                    .fetch_optional(&self.pool)
                    .await?;
                row.map(|r| org(&r, 0)).transpose()
            }

            async fn get_org_by_id(&self, org_id: i64) -> StoreResult<Option<Org>> {
                let row = sqlx::query("SELECT org_id, name, display_name, description, base_role, created_at FROM orgs WHERE org_id = $1")
                    .bind(org_id)
                    .fetch_optional(&self.pool)
                    .await?;
                row.map(|r| org(&r, 0)).transpose()
            }

            async fn list_orgs(&self) -> StoreResult<Vec<Org>> {
                let rows = sqlx::query("SELECT org_id, name, display_name, description, base_role, created_at FROM orgs ORDER BY name")
                    .fetch_all(&self.pool)
                    .await?;
                rows.iter().map(|r| org(r, 0)).collect()
            }

            async fn orgs_of_user(&self, user_id: i64) -> StoreResult<Vec<(Org, String)>> {
                let rows = sqlx::query(
                    "SELECT o.org_id, o.name, o.display_name, o.description, o.base_role, o.created_at, m.role FROM orgs o \
                     JOIN org_members m ON m.org_id = o.org_id WHERE m.user_id = $1 ORDER BY o.name",
                )
                .bind(user_id)
                .fetch_all(&self.pool)
                .await?;
                rows.iter().map(|r| Ok((org(r, 0)?, r.try_get(6)?))).collect()
            }

            async fn update_org(&self, org_id: i64, display_name: &str, description: &str, base_role: &str) -> StoreResult<()> {
                sqlx::query("UPDATE orgs SET display_name = $1, description = $2, base_role = $3 WHERE org_id = $4")
                    .bind(display_name)
                    .bind(description)
                    .bind(base_role)
                    .bind(org_id)
                    .execute(&self.pool)
                    .await?;
                Ok(())
            }

            async fn org_members(&self, org_id: i64) -> StoreResult<Vec<(User, String)>> {
                let rows = sqlx::query(
                    "SELECT u.user_id, u.name, u.is_admin, m.role FROM org_members m JOIN users u ON u.user_id = m.user_id \
                     WHERE m.org_id = $1 ORDER BY u.name",
                )
                .bind(org_id)
                .fetch_all(&self.pool)
                .await?;
                rows.into_iter()
                    .map(|r| Ok((User { id: r.try_get(0)?, name: r.try_get(1)?, is_admin: r.try_get(2)? }, r.try_get(3)?)))
                    .collect()
            }

            async fn set_org_member(&self, org_id: i64, user_id: i64, role: Option<&str>) -> StoreResult<()> {
                let _w = self.write_lock().await;
                let mut tx = self.pool.begin_with(Self::BEGIN_WRITE).await?;
                match role {
                    Some(role) => {
                        sqlx::query(
                            "INSERT INTO org_members (org_id, user_id, role) VALUES ($1, $2, $3) \
                             ON CONFLICT (org_id, user_id) DO UPDATE SET role = excluded.role",
                        )
                        .bind(org_id)
                        .bind(user_id)
                        .bind(role)
                        .execute(&mut *tx)
                        .await?;
                    }
                    None => {
                        sqlx::query("DELETE FROM org_members WHERE org_id = $1 AND user_id = $2").bind(org_id).bind(user_id).execute(&mut *tx).await?;
                        sqlx::query("DELETE FROM team_members WHERE user_id = $1 AND team_id IN (SELECT team_id FROM teams WHERE org_id = $2)")
                            .bind(user_id)
                            .bind(org_id)
                            .execute(&mut *tx)
                            .await?;
                    }
                }
                tx.commit().await?;
                Ok(())
            }

            async fn create_team(&self, org_id: i64, name: &str, description: &str) -> StoreResult<Team> {
                let row = sqlx::query(
                    "INSERT INTO teams (org_id, name, description, created_at) VALUES ($1, $2, $3, $4) \
                     ON CONFLICT (org_id, name) DO NOTHING RETURNING team_id",
                )
                .bind(org_id)
                .bind(name)
                .bind(description)
                .bind(now_ms())
                .fetch_optional(&self.pool)
                .await?;
                let Some(r) = row else { return Err(StoreError::Conflict(format!("a team named {name:?} already exists"))) };
                Ok(Team { id: r.try_get(0)?, org_id, name: name.to_string(), description: description.to_string() })
            }

            async fn delete_team(&self, team_id: i64) -> StoreResult<()> {
                let _w = self.write_lock().await;
                let mut tx = self.pool.begin_with(Self::BEGIN_WRITE).await?;
                sqlx::query("DELETE FROM team_members WHERE team_id = $1").bind(team_id).execute(&mut *tx).await?;
                sqlx::query("DELETE FROM repo_grants WHERE kind = 'team' AND subject_id = $1").bind(team_id).execute(&mut *tx).await?;
                sqlx::query("DELETE FROM teams WHERE team_id = $1").bind(team_id).execute(&mut *tx).await?;
                tx.commit().await?;
                Ok(())
            }

            async fn list_teams(&self, org_id: i64) -> StoreResult<Vec<Team>> {
                let rows = sqlx::query("SELECT team_id, org_id, name, description FROM teams WHERE org_id = $1 ORDER BY name")
                    .bind(org_id)
                    .fetch_all(&self.pool)
                    .await?;
                rows.into_iter()
                    .map(|r| Ok(Team { id: r.try_get(0)?, org_id: r.try_get(1)?, name: r.try_get(2)?, description: r.try_get(3)? }))
                    .collect()
            }

            async fn team_members(&self, team_id: i64) -> StoreResult<Vec<User>> {
                let rows = sqlx::query(
                    "SELECT u.user_id, u.name, u.is_admin FROM team_members m JOIN users u ON u.user_id = m.user_id WHERE m.team_id = $1 ORDER BY u.name",
                )
                .bind(team_id)
                .fetch_all(&self.pool)
                .await?;
                rows.into_iter().map(|r| Ok(User { id: r.try_get(0)?, name: r.try_get(1)?, is_admin: r.try_get(2)? })).collect()
            }

            async fn set_team_member(&self, team_id: i64, user_id: i64, member: bool) -> StoreResult<()> {
                let q = if member {
                    "INSERT INTO team_members (team_id, user_id) VALUES ($1, $2) ON CONFLICT (team_id, user_id) DO NOTHING"
                } else {
                    "DELETE FROM team_members WHERE team_id = $1 AND user_id = $2"
                };
                sqlx::query(q).bind(team_id).bind(user_id).execute(&self.pool).await?;
                Ok(())
            }

            async fn create_repo_owned(
                &self,
                name: &str,
                owner_kind: &str,
                owner_id: i64,
                visibility: &str,
                signed_in_role: &str,
            ) -> StoreResult<RepoId> {
                let row = sqlx::query(
                    "INSERT INTO repos (name, config, created_at, owner_kind, owner_id, visibility, signed_in_role) \
                     VALUES ($1, '{}', $2, $3, $4, $5, $6) ON CONFLICT (name) DO NOTHING RETURNING repo_id",
                )
                .bind(name)
                .bind(now_ms())
                .bind(owner_kind)
                .bind(owner_id)
                .bind(visibility)
                .bind(signed_in_role)
                .fetch_optional(&self.pool)
                .await?;
                match row {
                    Some(r) => Ok(RepoId(r.try_get::<i64, _>(0)?)),
                    None => Err(StoreError::RepoExists(name.to_string())),
                }
            }

            async fn repo_access(&self, repo: RepoId) -> StoreResult<Option<RepoAccess>> {
                let row = sqlx::query("SELECT owner_kind, owner_id, visibility, signed_in_role FROM repos WHERE repo_id = $1")
                    .bind(repo.0)
                    .fetch_optional(&self.pool)
                    .await?;
                row.map(|r| {
                    Ok(RepoAccess { owner_kind: r.try_get(0)?, owner_id: r.try_get(1)?, visibility: r.try_get(2)?, signed_in_role: r.try_get(3)? })
                })
                .transpose()
            }

            async fn set_repo_visibility(&self, repo: RepoId, visibility: &str, signed_in_role: &str) -> StoreResult<()> {
                sqlx::query("UPDATE repos SET visibility = $1, signed_in_role = $2 WHERE repo_id = $3")
                    .bind(visibility)
                    .bind(signed_in_role)
                    .bind(repo.0)
                    .execute(&self.pool)
                    .await?;
                Ok(())
            }

            async fn set_repo_grant(&self, repo: RepoId, kind: &str, subject_id: i64, role: Option<&str>) -> StoreResult<()> {
                match role {
                    Some(role) => {
                        sqlx::query(
                            "INSERT INTO repo_grants (repo_id, kind, subject_id, role) VALUES ($1, $2, $3, $4) \
                             ON CONFLICT (repo_id, kind, subject_id) DO UPDATE SET role = excluded.role",
                        )
                        .bind(repo.0)
                        .bind(kind)
                        .bind(subject_id)
                        .bind(role)
                        .execute(&self.pool)
                        .await?;
                    }
                    None => {
                        sqlx::query("DELETE FROM repo_grants WHERE repo_id = $1 AND kind = $2 AND subject_id = $3")
                            .bind(repo.0)
                            .bind(kind)
                            .bind(subject_id)
                            .execute(&self.pool)
                            .await?;
                    }
                }
                Ok(())
            }

            async fn repo_grants(&self, repo: RepoId) -> StoreResult<Vec<Grant>> {
                let rows = sqlx::query(
                    "SELECT g.kind, g.subject_id, COALESCE(u.name, t.name, ''), g.role FROM repo_grants g \
                     LEFT JOIN users u ON g.kind = 'user' AND u.user_id = g.subject_id \
                     LEFT JOIN teams t ON g.kind = 'team' AND t.team_id = g.subject_id \
                     WHERE g.repo_id = $1 ORDER BY g.kind, 3",
                )
                .bind(repo.0)
                .fetch_all(&self.pool)
                .await?;
                rows.into_iter()
                    .map(|r| Ok(Grant { kind: r.try_get(0)?, subject_id: r.try_get(1)?, subject_name: r.try_get(2)?, role: r.try_get(3)? }))
                    .collect()
            }

            async fn team_repos(&self, team_id: i64) -> StoreResult<Vec<(RepoId, String)>> {
                let rows = sqlx::query("SELECT repo_id, role FROM repo_grants WHERE kind = 'team' AND subject_id = $1 ORDER BY repo_id")
                    .bind(team_id)
                    .fetch_all(&self.pool)
                    .await?;
                rows.into_iter().map(|r| Ok((RepoId(r.try_get(0)?), r.try_get(1)?))).collect()
            }

            async fn authz_facts(&self, repo: RepoId, user_id: i64) -> StoreResult<AuthzFacts> {
                let org = sqlx::query(
                    "SELECT o.base_role, m.role FROM repos r JOIN orgs o ON r.owner_kind = 'org' AND o.org_id = r.owner_id \
                     LEFT JOIN org_members m ON m.org_id = o.org_id AND m.user_id = $2 WHERE r.repo_id = $1",
                )
                .bind(repo.0)
                .bind(user_id)
                .fetch_optional(&self.pool)
                .await?;
                let grants: Vec<String> = sqlx::query_scalar(
                    "SELECT role FROM repo_grants WHERE repo_id = $1 AND ((kind = 'user' AND subject_id = $2) OR \
                     (kind = 'team' AND subject_id IN (SELECT team_id FROM team_members WHERE user_id = $2)))",
                )
                .bind(repo.0)
                .bind(user_id)
                .fetch_all(&self.pool)
                .await?;
                let (org_base_role, org_role) = match org {
                    Some(r) => (Some(r.try_get::<String, _>(0)?), r.try_get::<Option<String>, _>(1)?),
                    None => (None, None),
                };
                Ok(AuthzFacts { org_role, org_base_role, grants })
            }

            async fn audit_append(&self, actor: &str, action: &str, target: &str, detail: &str) -> StoreResult<()> {
                sqlx::query("INSERT INTO audit_log (ts, actor, action, target, detail) VALUES ($1, $2, $3, $4, $5)")
                    .bind(now_ms())
                    .bind(actor)
                    .bind(action)
                    .bind(target)
                    .bind(detail)
                    .execute(&self.pool)
                    .await?;
                Ok(())
            }

            async fn audit_list(&self, before: Option<i64>, limit: u32) -> StoreResult<Vec<AuditEntry>> {
                let rows = sqlx::query(
                    "SELECT seq, ts, actor, action, target, detail FROM audit_log WHERE seq < $1 ORDER BY seq DESC LIMIT $2",
                )
                .bind(before.unwrap_or(i64::MAX))
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await?;
                rows.into_iter()
                    .map(|r| {
                        Ok(AuditEntry {
                            seq: r.try_get(0)?,
                            ts: r.try_get(1)?,
                            actor: r.try_get(2)?,
                            action: r.try_get(3)?,
                            target: r.try_get(4)?,
                            detail: r.try_get(5)?,
                        })
                    })
                    .collect()
            }

            async fn create_review(
                &self,
                repo: RepoId,
                title: &str,
                body: &str,
                author: &str,
                head: ChangesetId,
                target: &[u8],
            ) -> StoreResult<Review> {
                let _w = self.write_lock().await;
                let mut tx = self.pool.begin_with(Self::BEGIN_WRITE).await?;
                Self::lock_log(&mut tx, repo).await?;
                // Reviews and issues share one number space.
                let number: i64 = sqlx::query(
                    "SELECT COALESCE(MAX(n), 0) + 1 FROM (SELECT MAX(number) AS n FROM reviews WHERE repo_id = $1 \
                     UNION ALL SELECT MAX(number) AS n FROM issues WHERE repo_id = $1) AS t",
                )
                    .bind(repo.0)
                    .fetch_one(&mut *tx)
                    .await?
                    .try_get(0)?;
                let now = now_ms();
                sqlx::query(
                    "INSERT INTO reviews (repo_id, number, title, body, author, head, target, state, landed, created_at, updated_at) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, 'open', NULL, $8, $8)",
                )
                .bind(repo.0)
                .bind(number)
                .bind(title)
                .bind(body)
                .bind(author)
                .bind(head.as_bytes().as_slice())
                .bind(target)
                .bind(now)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(Review {
                    number,
                    title: title.into(),
                    body: body.into(),
                    author: author.into(),
                    head,
                    target: target.to_vec(),
                    state: "open".into(),
                    landed: None,
                    created_ms: now,
                    updated_ms: now,
                })
            }

            async fn get_review(&self, repo: RepoId, number: i64) -> StoreResult<Option<Review>> {
                let row = sqlx::query(
                    "SELECT number, title, body, author, head, target, state, landed, created_at, updated_at \
                     FROM reviews WHERE repo_id = $1 AND number = $2",
                )
                .bind(repo.0)
                .bind(number)
                .fetch_optional(&self.pool)
                .await?;
                row.map(review_row).transpose()
            }

            async fn list_reviews(&self, repo: RepoId, state: Option<&str>, limit: u32) -> StoreResult<Vec<Review>> {
                let rows = match state {
                    Some(st) => sqlx::query(
                        "SELECT number, title, body, author, head, target, state, landed, created_at, updated_at \
                         FROM reviews WHERE repo_id = $1 AND state = $2 ORDER BY number DESC LIMIT $3",
                    )
                    .bind(repo.0)
                    .bind(st)
                    .bind(limit as i64)
                    .fetch_all(&self.pool)
                    .await?,
                    None => sqlx::query(
                        "SELECT number, title, body, author, head, target, state, landed, created_at, updated_at \
                         FROM reviews WHERE repo_id = $1 ORDER BY number DESC LIMIT $2",
                    )
                    .bind(repo.0)
                    .bind(limit as i64)
                    .fetch_all(&self.pool)
                    .await?,
                };
                rows.into_iter().map(review_row).collect()
            }

            async fn update_review(&self, repo: RepoId, number: i64, u: ReviewUpdate) -> StoreResult<Option<Review>> {
                let _w = self.write_lock().await;
                let mut tx = self.pool.begin_with(Self::BEGIN_WRITE).await?;
                let row = sqlx::query(
                    "SELECT number, title, body, author, head, target, state, landed, created_at, updated_at \
                     FROM reviews WHERE repo_id = $1 AND number = $2",
                )
                .bind(repo.0)
                .bind(number)
                .fetch_optional(&mut *tx)
                .await?;
                let Some(row) = row else { return Ok(None) };
                let mut r = review_row(row)?;
                if let Some(t) = u.title {
                    r.title = t;
                }
                if let Some(b) = u.body {
                    r.body = b;
                }
                if let Some(h) = u.head {
                    r.head = h;
                }
                if let Some(s) = u.state {
                    r.state = s;
                }
                if let Some(l) = u.landed {
                    r.landed = Some(l);
                }
                r.updated_ms = now_ms();
                sqlx::query(
                    "UPDATE reviews SET title = $3, body = $4, head = $5, state = $6, landed = $7, updated_at = $8 \
                     WHERE repo_id = $1 AND number = $2",
                )
                .bind(repo.0)
                .bind(number)
                .bind(&r.title)
                .bind(&r.body)
                .bind(r.head.as_bytes().as_slice())
                .bind(&r.state)
                .bind(r.landed.map(|l| l.as_bytes().to_vec()))
                .bind(r.updated_ms)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(Some(r))
            }

            async fn add_review_event(
                &self,
                repo: RepoId,
                number: i64,
                kind: &str,
                author: &str,
                body: &str,
                cs: Option<ChangesetId>,
                path: Option<&[u8]>,
                line: Option<i64>,
            ) -> StoreResult<i64> {
                let row = sqlx::query(
                    "INSERT INTO review_events (repo_id, number, kind, author, body, cs_id, path, line, ts) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING seq",
                )
                .bind(repo.0)
                .bind(number)
                .bind(kind)
                .bind(author)
                .bind(body)
                .bind(cs.map(|c| c.as_bytes().to_vec()))
                .bind(path)
                .bind(line)
                .bind(now_ms())
                .fetch_one(&self.pool)
                .await?;
                Ok(row.try_get(0)?)
            }

            async fn review_events(&self, repo: RepoId, number: i64) -> StoreResult<Vec<ReviewEvent>> {
                let rows = sqlx::query(
                    "SELECT seq, kind, author, body, cs_id, path, line, ts FROM review_events \
                     WHERE repo_id = $1 AND number = $2 ORDER BY seq",
                )
                .bind(repo.0)
                .bind(number)
                .fetch_all(&self.pool)
                .await?;
                rows.into_iter()
                    .map(|r| {
                        Ok(ReviewEvent {
                            seq: r.try_get(0)?,
                            kind: r.try_get(1)?,
                            author: r.try_get(2)?,
                            body: r.try_get(3)?,
                            cs: r.try_get::<Option<Vec<u8>>, _>(4)?.map(|b| cs_id(&b)).transpose()?,
                            path: r.try_get(5)?,
                            line: r.try_get(6)?,
                            ts_ms: r.try_get(7)?,
                        })
                    })
                    .collect()
            }
        }
    };
}

sql_store!(SqliteStore, sqlx::SqlitePool);
sql_store!(PostgresStore, sqlx::PgPool);

impl SqliteStore {
    /// Write transactions take SQLite's write lock up front: a deferred
    /// transaction that reads first fails at once (SQLITE_BUSY, no busy
    /// wait) if another connection is writing when it upgrades.
    const BEGIN_WRITE: &'static str = "BEGIN IMMEDIATE";

    pub async fn open(url: &str) -> anyhow::Result<Self> {
        use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
        use std::str::FromStr;
        let in_memory = url.contains(":memory:");
        let opts = SqliteConnectOptions::from_str(url)?
            .create_if_missing(true)
            .journal_mode(if in_memory { SqliteJournalMode::Memory } else { SqliteJournalMode::Wal })
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(std::time::Duration::from_secs(30))
            .foreign_keys(false)
            .pragma("cache_size", "-65536")
            .pragma("mmap_size", "1073741824")
            .pragma("temp_store", "memory");
        let pool = SqlitePoolOptions::new()
            // An in-memory database exists per connection; pin it to one.
            .max_connections(if in_memory { 1 } else { 8 })
            .connect_with(opts)
            .await?;
        sqlx::migrate!("./migrations/sqlite").run(&pool).await?;
        Ok(Self { pool, writer: Some(Default::default()) })
    }

    async fn backup_impl(pool: &sqlx::SqlitePool, path: &std::path::Path) -> StoreResult<()> {
        if path.exists() {
            return Err(StoreError::Backend(anyhow::anyhow!("{} already exists", path.display())));
        }
        sqlx::query("VACUUM INTO $1").bind(path.to_string_lossy().into_owned()).execute(pool).await?;
        Ok(())
    }

    /// SQLite serializes writers; `BEGIN` is deferred, so issue a write first
    /// to acquire the RESERVED lock before reading the log head.
    async fn lock_log(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, repo: RepoId) -> StoreResult<()> {
        sqlx::query("UPDATE repos SET repo_id = repo_id WHERE repo_id = $1").bind(repo.0).execute(&mut **tx).await?;
        Ok(())
    }

    async fn lock_ref(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, repo: RepoId, _name: &[u8]) -> StoreResult<()> {
        Self::lock_log(tx, repo).await
    }
}

impl PostgresStore {
    const BEGIN_WRITE: &'static str = "BEGIN";

    pub async fn open(url: &str) -> anyhow::Result<Self> {
        let pool = sqlx::postgres::PgPoolOptions::new().max_connections(32).connect(url).await?;
        sqlx::migrate!("./migrations/postgres").run(&pool).await?;
        Ok(Self { pool, writer: None })
    }

    async fn backup_impl(_pool: &sqlx::PgPool, _path: &std::path::Path) -> StoreResult<()> {
        Err(StoreError::Backend(anyhow::anyhow!(
            "Postgres metadata is backed up with pg_dump or point-in-time recovery, not flux-server backup"
        )))
    }

    /// Serialize log-sequence assignment per repo (SPEC §5.1.2). The lock is
    /// transaction-scoped and released on commit.
    async fn lock_log(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, repo: RepoId) -> StoreResult<()> {
        sqlx::query("SELECT pg_advisory_xact_lock($1, 0)").bind(repo.0 as i32).execute(&mut **tx).await?;
        Ok(())
    }

    async fn lock_ref(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, repo: RepoId, name: &[u8]) -> StoreResult<()> {
        // Key the lock on (repo, hash(name)) so unrelated refs don't contend.
        let h = fnv1a_lock_key(name);
        sqlx::query("SELECT pg_advisory_xact_lock($1, $2)").bind(repo.0 as i32).bind(h).execute(&mut **tx).await?;
        Ok(())
    }
}

/// Cheap stable 32-bit hash for advisory lock keys (never 0, which is reserved
/// for the log lock).
fn fnv1a_lock_key(name: &[u8]) -> i32 {
    let mut h: u32 = 0x811c9dc5;
    for b in name {
        h ^= *b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    if h == 0 { 1 } else { h as i32 }
}
