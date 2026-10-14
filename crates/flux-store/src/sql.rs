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

macro_rules! sql_store {
    ($name:ident, $pool:ty) => {
        #[derive(Clone)]
        pub struct $name {
            pool: $pool,
        }

        impl $name {
            pub fn pool(&self) -> &$pool {
                &self.pool
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

            async fn put_changesets(&self, repo: RepoId, batch: &[NewChangeset]) -> StoreResult<Vec<LogSeq>> {
                let mut tx = self.pool.begin().await?;
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
                let mut tx = self.pool.begin().await?;
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
                let mut tx = self.pool.begin().await?;
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
                let mut tx = self.pool.begin().await?;
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
                let mut tx = self.pool.begin().await?;
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

            async fn create_review(
                &self,
                repo: RepoId,
                title: &str,
                body: &str,
                author: &str,
                head: ChangesetId,
                target: &[u8],
            ) -> StoreResult<Review> {
                let mut tx = self.pool.begin().await?;
                Self::lock_log(&mut tx, repo).await?;
                let number: i64 = sqlx::query("SELECT COALESCE(MAX(number), 0) + 1 FROM reviews WHERE repo_id = $1")
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
                let mut tx = self.pool.begin().await?;
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
        Ok(Self { pool })
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
    pub async fn open(url: &str) -> anyhow::Result<Self> {
        let pool = sqlx::postgres::PgPoolOptions::new().max_connections(32).connect(url).await?;
        sqlx::migrate!("./migrations/postgres").run(&pool).await?;
        Ok(Self { pool })
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
