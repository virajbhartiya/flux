//! Conversions between protobuf messages and `flux-core` types.

use std::collections::BTreeMap;

use flux_core::{ChangesetId, ContentId, FileChange, FileKind, RepoPath, Timestamp, TreeId};
use tonic::Status;

use crate::v1;

pub fn bad(msg: impl std::fmt::Display) -> Status {
    Status::invalid_argument(msg.to_string())
}

pub fn cs_id(b: &[u8]) -> Result<ChangesetId, Status> {
    ChangesetId::from_slice(b).map_err(bad)
}

pub fn content_id(b: &[u8]) -> Result<ContentId, Status> {
    ContentId::from_slice(b).map_err(bad)
}

pub fn tree_id(b: &[u8]) -> Result<TreeId, Status> {
    TreeId::from_slice(b).map_err(bad)
}

pub fn repo_path(b: &[u8]) -> Result<RepoPath, Status> {
    RepoPath::new(b).map_err(bad)
}

/// Optional path: empty bytes mean "root".
pub fn opt_repo_path(b: &[u8]) -> Result<Option<RepoPath>, Status> {
    if b.is_empty() { Ok(None) } else { repo_path(b).map(Some) }
}

pub fn kind_to_pb(kind: &FileKind) -> (i32, Vec<u8>) {
    match kind {
        FileKind::Regular => (v1::FileKind::Regular as i32, vec![]),
        FileKind::Executable => (v1::FileKind::Executable as i32, vec![]),
        FileKind::Symlink => (v1::FileKind::Symlink as i32, vec![]),
        FileKind::GitLink(sha) => (v1::FileKind::Gitlink as i32, sha.to_vec()),
        FileKind::GitMode(_) => kind_to_pb(&kind.normalized()),
    }
}

pub fn kind_from_pb(kind: i32, gitlink: &[u8]) -> Result<FileKind, Status> {
    match v1::FileKind::try_from(kind) {
        Ok(v1::FileKind::Regular) => Ok(FileKind::Regular),
        Ok(v1::FileKind::Executable) => Ok(FileKind::Executable),
        Ok(v1::FileKind::Symlink) => Ok(FileKind::Symlink),
        Ok(v1::FileKind::Gitlink) => Ok(FileKind::GitLink(gitlink.try_into().map_err(|_| bad("gitlink_sha1 must be 20 bytes"))?)),
        _ => Err(bad(format!("invalid file kind {kind}"))),
    }
}

fn ts_to_pb(t: &Timestamp) -> v1::Timestamp {
    v1::Timestamp { unix_ms: t.unix_ms, tz_offset_min: t.tz_offset_min as i32 }
}

fn ts_from_pb(t: Option<&v1::Timestamp>) -> Result<Timestamp, Status> {
    let t = t.ok_or_else(|| bad("missing timestamp"))?;
    Ok(Timestamp {
        unix_ms: t.unix_ms,
        tz_offset_min: i16::try_from(t.tz_offset_min).map_err(|_| bad("tz_offset_min out of range"))?,
    })
}

pub fn changeset_to_pb(cs: &flux_core::Changeset) -> v1::Changeset {
    v1::Changeset {
        parents: cs.parents.iter().map(|p| p.as_bytes().to_vec()).collect(),
        author: cs.author.clone(),
        author_time: Some(ts_to_pb(&cs.author_time)),
        committer: cs.committer.clone(),
        committer_time: cs.committer_time.as_ref().map(ts_to_pb),
        message: cs.message.clone(),
        file_changes: cs
            .file_changes
            .iter()
            .map(|(path, change)| match change {
                FileChange::Deletion => v1::FileChange { path: path.as_bytes().to_vec(), deleted: true, ..Default::default() },
                FileChange::Retain => v1::FileChange { path: path.as_bytes().to_vec(), retain: true, ..Default::default() },
                FileChange::Change { content_id, size, kind, copy_from } => {
                    let git_mode = match kind {
                        FileKind::GitMode(m) => *m,
                        _ => 0,
                    };
                    let (kind, gitlink_sha1) = kind_to_pb(kind);
                    v1::FileChange {
                        git_mode,
                        path: path.as_bytes().to_vec(),
                        deleted: false,
                        content_id: content_id.as_bytes().to_vec(),
                        size: *size,
                        kind,
                        gitlink_sha1,
                        copy_from: copy_from
                            .as_ref()
                            .map(|(p, i)| v1::CopyFrom { path: p.as_bytes().to_vec(), parent_index: *i }),
                        retain: false,
                    }
                }
            })
            .collect(),
        extra: cs.extra.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
    }
}

pub fn changeset_from_pb(pb: &v1::Changeset) -> Result<flux_core::Changeset, Status> {
    let mut file_changes = BTreeMap::new();
    for fc in &pb.file_changes {
        let path = repo_path(&fc.path)?;
        let change = if fc.deleted && fc.retain {
            return Err(bad(format!("{path}: deleted and retain are exclusive")));
        } else if fc.deleted {
            FileChange::Deletion
        } else if fc.retain {
            FileChange::Retain
        } else {
            let kind = match fc.git_mode {
                0 => kind_from_pb(fc.kind, &fc.gitlink_sha1)?,
                m if m & !0o777 == 0o100000 => FileKind::from_git_file_mode(m),
                m => return Err(bad(format!("{path}: invalid git_mode {m:o}"))),
            };
            let content_id = match kind {
                // Gitlinks carry no content; normalize to the empty id.
                FileKind::GitLink(_) => ContentId::empty(),
                _ => content_id(&fc.content_id)?,
            };
            FileChange::Change {
                content_id,
                size: fc.size,
                kind,
                copy_from: fc.copy_from.as_ref().map(|c| repo_path(&c.path).map(|p| (p, c.parent_index))).transpose()?,
            }
        };
        if file_changes.insert(path.clone(), change).is_some() {
            return Err(bad(format!("duplicate file change for {path}")));
        }
    }
    let committer_time = match (&pb.committer, &pb.committer_time) {
        (Some(_), t) => Some(ts_from_pb(t.as_ref())?),
        (None, None) => None,
        (None, Some(_)) => return Err(bad("committer_time without committer")),
    };
    Ok(flux_core::Changeset {
        parents: pb.parents.iter().map(|p| cs_id(p)).collect::<Result<_, _>>()?,
        author: pb.author.clone(),
        author_time: ts_from_pb(pb.author_time.as_ref())?,
        committer: pb.committer.clone(),
        committer_time,
        message: pb.message.clone(),
        file_changes,
        extra: pb.extra.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changeset_roundtrip() {
        let mut fc = BTreeMap::new();
        fc.insert(RepoPath::try_from("a").unwrap(), FileChange::Deletion);
        fc.insert(
            RepoPath::try_from("b").unwrap(),
            FileChange::Change {
                content_id: ContentId::hash(b"b"),
                size: 1,
                kind: FileKind::Executable,
                copy_from: Some((RepoPath::try_from("a").unwrap(), 0)),
            },
        );
        fc.insert(
            RepoPath::try_from("sub").unwrap(),
            FileChange::Change { content_id: ContentId::empty(), size: 0, kind: FileKind::GitLink([3; 20]), copy_from: None },
        );
        fc.insert(
            RepoPath::try_from("old").unwrap(),
            FileChange::Change { content_id: ContentId::hash(b"o"), size: 1, kind: FileKind::GitMode(0o100664), copy_from: None },
        );
        let cs = flux_core::Changeset {
            parents: vec![ChangesetId::hash(b"p")],
            author: b"a".to_vec(),
            author_time: Timestamp { unix_ms: 5, tz_offset_min: -60 },
            committer: Some(b"c".to_vec()),
            committer_time: Some(Timestamp { unix_ms: 6, tz_offset_min: 0 }),
            message: b"m".to_vec(),
            file_changes: fc,
            extra: BTreeMap::from([("k".into(), b"v".to_vec())]),
        };
        assert_eq!(changeset_from_pb(&changeset_to_pb(&cs)).unwrap(), cs);
    }
}
