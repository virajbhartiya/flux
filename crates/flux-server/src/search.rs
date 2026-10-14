//! Code search and fuzzy file finding over a revision, server-side.
//!
//! Search streams every candidate file through the content cache and scans
//! them in parallel with a compiled `regex::bytes` matcher (literal patterns
//! are escaped, so they hit the regex engine's memchr fast paths). File lists
//! are cached per root tree id, which is immutable.

use std::sync::Arc;

use flux_core::{ContentId, RepoPath, TreeId};
use flux_derive::walk_tree;
use flux_store::RepoId;
use futures::StreamExt;
use rayon::prelude::*;

use crate::core::{Flux, FluxError, FluxResult};

/// Files larger than this are not searched.
const MAX_SEARCH_FILE: u64 = 2 << 20;

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub path: RepoPath,
    /// 1-based.
    pub line_number: u32,
    pub line: Vec<u8>,
    /// Byte ranges of the match within `line`.
    pub spans: Vec<(u32, u32)>,
}

#[derive(Debug, Clone, Default)]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
    pub files_scanned: usize,
    pub truncated: bool,
}

pub type FileList = Arc<Vec<(RepoPath, ContentId, u64)>>;

impl Flux {
    /// Every regular file under `root` (cached by tree id).
    pub async fn file_list(&self, root: TreeId) -> FluxResult<FileList> {
        if let Some(l) = self.file_lists.get(&root) {
            return Ok(l);
        }
        let all = |_: &RepoPath, _: bool| true;
        let files = walk_tree(&self.blobs, root, &all).await?;
        let list: Vec<(RepoPath, ContentId, u64)> =
            files.into_iter().filter_map(|f| f.content_id.map(|c| (f.path, c, f.size))).collect();
        let list = Arc::new(list);
        self.file_lists.insert(root, list.clone());
        Ok(list)
    }

    pub async fn search(
        &self,
        repo: RepoId,
        root: TreeId,
        pattern: &str,
        regex: bool,
        case_insensitive: bool,
        prefix: Option<&RepoPath>,
        limit: usize,
    ) -> FluxResult<SearchResult> {
        let _ = repo;
        if pattern.is_empty() {
            return Ok(SearchResult::default());
        }
        let src = if regex { pattern.to_string() } else { regex::escape(pattern) };
        let re = regex::bytes::RegexBuilder::new(&src)
            .case_insensitive(case_insensitive)
            .multi_line(true)
            .size_limit(8 << 20)
            .build()
            .map_err(|e| FluxError::InvalidArgument(format!("bad pattern: {e}")))?;
        let files = self.file_list(root).await?;
        let candidates: Vec<(RepoPath, ContentId)> = files
            .iter()
            .filter(|(p, _, size)| *size <= MAX_SEARCH_FILE && prefix.is_none_or(|pre| p.starts_with(pre)))
            .map(|(p, c, _)| (p.clone(), *c))
            .collect();
        // Fetch concurrently (content cache, then storage).
        let contents: Vec<FluxResult<Option<(RepoPath, bytes::Bytes)>>> = futures::stream::iter(candidates)
            .map(|(p, c)| {
                let blobs = self.blobs.clone();
                async move {
                    let data = blobs.get_content_bytes(&c).await?;
                    Ok(data.map(|d| (p, d)))
                }
            })
            .buffered(64)
            .collect()
            .await;
        let mut docs = Vec::with_capacity(contents.len());
        for c in contents {
            if let Some(d) = c? {
                docs.push(d);
            }
        }
        let scanned = docs.len();
        let re = Arc::new(re);
        let result = tokio::task::spawn_blocking(move || {
            let per_file: Vec<Vec<SearchHit>> = docs
                .par_iter()
                .map(|(path, data)| {
                    if data[..data.len().min(8192)].contains(&0) {
                        return vec![];
                    }
                    let mut hits = Vec::new();
                    let mut last_line_start = usize::MAX;
                    for m in re.find_iter(data) {
                        let start = data[..m.start()].iter().rposition(|b| *b == b'\n').map(|i| i + 1).unwrap_or(0);
                        let end = data[m.start()..].iter().position(|b| *b == b'\n').map(|i| m.start() + i).unwrap_or(data.len());
                        let span = ((m.start() - start) as u32, (m.end().min(end) - start) as u32);
                        if start == last_line_start {
                            if let Some(h) = hits.last_mut() {
                                let h: &mut SearchHit = h;
                                h.spans.push(span);
                            }
                            continue;
                        }
                        last_line_start = start;
                        let line_number = data[..start].iter().filter(|b| **b == b'\n').count() as u32 + 1;
                        let mut line = data[start..end].to_vec();
                        line.truncate(1000);
                        hits.push(SearchHit { path: path.clone(), line_number, line, spans: vec![span] });
                        if hits.len() >= 100 {
                            break;
                        }
                    }
                    hits
                })
                .collect();
            let mut hits: Vec<SearchHit> = per_file.into_iter().flatten().collect();
            hits.sort_by(|a, b| a.path.cmp(&b.path).then(a.line_number.cmp(&b.line_number)));
            let truncated = hits.len() > limit;
            hits.truncate(limit);
            SearchResult { hits, files_scanned: scanned, truncated }
        })
        .await
        .map_err(|e| FluxError::Internal(e.into()))?;
        Ok(result)
    }

    /// Fuzzy path finder: subsequence match scored by contiguity, word
    /// boundaries and basename hits (like editors' "go to file").
    pub async fn find_files(&self, root: TreeId, query: &str, limit: usize) -> FluxResult<Vec<RepoPath>> {
        let files = self.file_list(root).await?;
        if query.is_empty() {
            return Ok(files.iter().take(limit).map(|f| f.0.clone()).collect());
        }
        let q: Vec<u8> = query.bytes().filter(|b| *b != b' ').map(|b| b.to_ascii_lowercase()).collect();
        let mut scored: Vec<(i64, &RepoPath)> =
            files.par_iter().filter_map(|(p, _, _)| fuzzy_score(p.as_bytes(), &q).map(|s| (s, p))).collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.as_bytes().len().cmp(&b.1.as_bytes().len())).then(a.1.cmp(b.1)));
        Ok(scored.into_iter().take(limit).map(|(_, p)| p.clone()).collect())
    }
}

/// Score `path` against lowercase query `q`; `None` if not a subsequence.
pub fn fuzzy_score(path: &[u8], q: &[u8]) -> Option<i64> {
    let base_start = path.iter().rposition(|b| *b == b'/').map(|i| i + 1).unwrap_or(0);
    let mut score = 0i64;
    let mut qi = 0;
    let mut prev: Option<usize> = None;
    for (i, &c) in path.iter().enumerate() {
        if qi == q.len() {
            break;
        }
        if c.to_ascii_lowercase() == q[qi] {
            let mut s = 1;
            if prev == Some(i.wrapping_sub(1)) {
                s += 5; // contiguous
            }
            if i == 0 || matches!(path[i - 1], b'/' | b'_' | b'-' | b'.') {
                s += 4; // word start
            }
            if i >= base_start {
                s += 2; // in basename
            }
            score += s;
            prev = Some(i);
            qi += 1;
        }
    }
    (qi == q.len()).then_some(score * 100 - path.len() as i64)
}

#[cfg(test)]
mod tests {
    use super::fuzzy_score;

    #[test]
    fn fuzzy_prefers_basename_and_contiguous() {
        let a = fuzzy_score(b"src/server/main.rs", b"main").unwrap();
        let b = fuzzy_score(b"src/my_app/config/init.rs", b"main").unwrap_or(i64::MIN);
        assert!(a > b);
        assert!(fuzzy_score(b"abc", b"abd").is_none());
    }
}
