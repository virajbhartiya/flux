//! In-memory commit graph per repository.
//!
//! Every changeset's id, generation and parents are loaded once (in log
//! order, so parents always precede children) and kept as dense integer
//! indices. Log walks, ancestry checks, merge bases and ahead/behind counts
//! then run without touching the metadata store. New changesets are appended
//! as they are created; changesets written by other server instances are
//! picked up lazily by tailing the log when an unknown id is requested.
//!
//! Memory is ~60 bytes per changeset, so this comfortably covers repos with
//! tens of millions of changesets (SPEC §6.4's skip lists take over beyond).

use std::collections::{BinaryHeap, HashMap};

use flux_core::ChangesetId;
use flux_store::{GraphRow, LogSeq, MetaStore, RepoId, StoreResult};

#[derive(Default)]
pub struct Graph {
    ids: Vec<ChangesetId>,
    gens: Vec<u64>,
    /// Parent indices: `parents[par_start[i]..par_start[i+1]]`.
    par_start: Vec<u32>,
    parents: Vec<u32>,
    index: HashMap<ChangesetId, u32>,
    pub loaded_seq: LogSeq,
}

impl Graph {
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn get(&self, id: &ChangesetId) -> Option<u32> {
        self.index.get(id).copied()
    }

    pub fn id(&self, i: u32) -> ChangesetId {
        self.ids[i as usize]
    }

    pub fn gen_of(&self, i: u32) -> u64 {
        self.gens[i as usize]
    }

    pub fn parents(&self, i: u32) -> &[u32] {
        let i = i as usize;
        let end = if i + 1 < self.par_start.len() { self.par_start[i + 1] as usize } else { self.parents.len() };
        &self.parents[self.par_start[i] as usize..end]
    }

    /// Append rows in log order. Rows whose parents are unknown are skipped
    /// (cannot happen for a consistent log, but never corrupt the graph).
    pub fn extend(&mut self, rows: &[GraphRow]) {
        for r in rows {
            self.loaded_seq = self.loaded_seq.max(r.seq);
            if self.index.contains_key(&r.id) {
                continue;
            }
            let ps: Option<Vec<u32>> = r.parents.iter().map(|p| self.index.get(p).copied()).collect();
            let Some(ps) = ps else {
                tracing::warn!(id = %r.id, "graph: parent missing, skipping");
                continue;
            };
            let i = self.ids.len() as u32;
            self.ids.push(r.id);
            self.gens.push(r.gen_num);
            self.par_start.push(self.parents.len() as u32);
            self.parents.extend(ps);
            self.index.insert(r.id, i);
        }
    }

    pub async fn load_tail(&mut self, meta: &dyn MetaStore, repo: RepoId) -> StoreResult<usize> {
        let mut added = 0;
        loop {
            let rows = meta.graph_tail(repo, self.loaded_seq, 50_000).await?;
            if rows.is_empty() {
                return Ok(added);
            }
            added += rows.len();
            self.extend(&rows);
        }
    }

    /// Ancestors of `starts` (inclusive), newest generation first, skipping
    /// anything reachable from `hide` (git's `hide..start`).
    pub fn walk(&self, starts: &[u32], hide: &[u32], first_parent: bool) -> Walk<'_> {
        let mut w = Walk { g: self, heap: BinaryHeap::new(), seen: HashMap::new(), first_parent, visible: 0 };
        for &h in hide {
            w.push(h, true);
        }
        for &s in starts {
            w.push(s, false);
        }
        w
    }

    /// Is `a` an ancestor of (or equal to) `b`?
    pub fn is_ancestor(&self, a: u32, b: u32) -> bool {
        if a == b {
            return true;
        }
        let ga = self.gen_of(a);
        if ga >= self.gen_of(b) {
            return false;
        }
        let mut stack = vec![b];
        let mut seen = vec![false; self.ids.len()];
        while let Some(x) = stack.pop() {
            for &p in self.parents(x) {
                if p == a {
                    return true;
                }
                if !seen[p as usize] && self.gen_of(p) > ga {
                    seen[p as usize] = true;
                    stack.push(p);
                }
            }
        }
        false
    }

    /// Best common ancestors (generation-ordered search from both sides).
    pub fn merge_base(&self, a: u32, b: u32) -> Option<u32> {
        if self.is_ancestor(a, b) {
            return Some(a);
        }
        if self.is_ancestor(b, a) {
            return Some(b);
        }
        // Colour-propagating walk in descending generation (as git does).
        const A: u8 = 1;
        const B: u8 = 2;
        let mut color: HashMap<u32, u8> = HashMap::new();
        let mut heap: BinaryHeap<(u64, u32)> = BinaryHeap::new();
        color.insert(a, A);
        color.insert(b, B);
        heap.push((self.gen_of(a), a));
        heap.push((self.gen_of(b), b));
        while let Some((_, x)) = heap.pop() {
            let c = color[&x];
            if c == A | B {
                return Some(x);
            }
            for &p in self.parents(x) {
                let e = color.entry(p).or_insert(0);
                if *e | c != *e {
                    *e |= c;
                    heap.push((self.gen_of(p), p));
                }
            }
        }
        None
    }

    /// `(ahead, behind)`: changesets in `a` not in `b`, and vice versa.
    pub fn ahead_behind(&self, a: u32, b: u32) -> (usize, usize) {
        (self.walk(&[a], &[b], false).count(), self.walk(&[b], &[a], false).count())
    }
}

/// Generation-ordered ancestor walk with hide-set subtraction. Generations
/// strictly decrease along parent edges, so a node reachable from a hidden
/// node is always marked hidden before it is popped.
pub struct Walk<'a> {
    g: &'a Graph,
    heap: BinaryHeap<(u64, u32)>,
    /// Queued or emitted nodes; `true` = hidden.
    seen: HashMap<u32, bool>,
    first_parent: bool,
    /// Visible nodes currently queued; the walk ends when only hidden remain.
    visible: usize,
}

impl Walk<'_> {
    fn push(&mut self, i: u32, hidden: bool) {
        match self.seen.get(&i).copied() {
            Some(true) => {}
            Some(false) => {
                if hidden {
                    // Queued as visible, now hidden (it can't have been popped yet).
                    self.seen.insert(i, true);
                    self.visible -= 1;
                }
            }
            None => {
                self.seen.insert(i, hidden);
                if !hidden {
                    self.visible += 1;
                }
                self.heap.push((self.g.gen_of(i), i));
            }
        }
    }
}

impl Iterator for Walk<'_> {
    type Item = u32;
    fn next(&mut self) -> Option<u32> {
        while self.visible > 0 {
            let (_, i) = self.heap.pop()?;
            let hidden = self.seen[&i];
            if !hidden {
                self.visible -= 1;
            }
            let ps = self.g.parents(i);
            let ps = if self.first_parent && !hidden { &ps[..ps.len().min(1)] } else { ps };
            for &p in ps {
                self.push(p, hidden);
            }
            if !hidden {
                return Some(i);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> ChangesetId {
        ChangesetId::hash(&[n])
    }

    /// 1 ← 2 ← 3 ← 5(merge 3,4)
    ///  ↖ 4 ←────────┘
    fn graph() -> Graph {
        let rows = vec![
            GraphRow { seq: 1, id: id(1), gen_num: 1, parents: vec![] },
            GraphRow { seq: 2, id: id(2), gen_num: 2, parents: vec![id(1)] },
            GraphRow { seq: 3, id: id(3), gen_num: 3, parents: vec![id(2)] },
            GraphRow { seq: 4, id: id(4), gen_num: 2, parents: vec![id(1)] },
            GraphRow { seq: 5, id: id(5), gen_num: 4, parents: vec![id(3), id(4)] },
        ];
        let mut g = Graph::default();
        g.extend(&rows);
        g
    }

    #[test]
    fn walks_and_ancestry() {
        let g = graph();
        let i = |n| g.get(&id(n)).unwrap();
        let order: Vec<ChangesetId> = g.walk(&[i(5)], &[], false).map(|x| g.id(x)).collect();
        assert_eq!(order.len(), 5);
        assert_eq!(order[0], id(5));
        assert_eq!(*order.last().unwrap(), id(1));
        let fp: Vec<ChangesetId> = g.walk(&[i(5)], &[], true).map(|x| g.id(x)).collect();
        assert_eq!(fp, vec![id(5), id(3), id(2), id(1)]);
        // 3..5 = {5, 4}
        let mut range: Vec<ChangesetId> = g.walk(&[i(5)], &[i(3)], false).map(|x| g.id(x)).collect();
        range.sort();
        let mut want = vec![id(5), id(4)];
        want.sort();
        assert_eq!(range, want);
        assert!(g.is_ancestor(i(4), i(5)));
        assert!(!g.is_ancestor(i(4), i(3)));
        assert_eq!(g.merge_base(i(3), i(4)), Some(i(1)));
        assert_eq!(g.merge_base(i(2), i(5)), Some(i(2)));
        assert_eq!(g.ahead_behind(i(3), i(4)), (2, 1));
    }
}
