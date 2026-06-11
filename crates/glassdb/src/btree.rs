//! The B+tree — the "filing system" that finds one row among millions in a
//! handful of page reads.
//!
//! Layout: internal nodes hold separator keys and child page ids; leaf nodes
//! hold (key, row bytes) cells and a `next` pointer to their right sibling,
//! so range scans walk the leaf chain without re-descending. Separator
//! convention: child[i] covers keys k with keys[i-1] <= k < keys[i].
//!
//! Simplifications (documented, deliberate):
//! - Keys are i64 (the table's INTEGER PRIMARY KEY or a hidden rowid).
//! - Deletes are lazy: cells are removed but nodes are never merged. Real
//!   engines rebalance; SQLite mostly doesn't either (it has a vacuum).
//! - Rows are capped at `MAX_VALUE_LEN` bytes; overflow pages are future work.
//!   The cap is chosen so a single size-based split always produces two
//!   nodes that fit — provably, not hopefully.

use std::ops::Bound;

use crate::errors::{DbError, DbResult};
use crate::json::J;
use crate::pager::{Pager, PAGE_SIZE, PT_INTERNAL, PT_LEAF};
use crate::types::Reader;

/// Max row payload. With cell overhead (8-byte key + 4-byte length) every
/// cell is <= ~1312 bytes ~= PAGE_SIZE/3, which guarantees that splitting an
/// overfull leaf at its size midpoint always yields two fitting halves.
pub const MAX_VALUE_LEN: usize = 1300;

const LEAF_HEADER: usize = 1 + 2 + 4; // type, n_cells, next
const CELL_OVERHEAD: usize = 8 + 4; // key, value length
const INTERNAL_HEADER: usize = 1 + 2; // type, n_keys

#[derive(Debug)]
enum Node {
    Internal {
        keys: Vec<i64>,
        children: Vec<u32>,
    },
    Leaf {
        cells: Vec<(i64, Vec<u8>)>,
        next: u32,
    },
}

impl Node {
    fn empty_leaf() -> Node {
        Node::Leaf {
            cells: Vec::new(),
            next: 0,
        }
    }

    fn encoded_len(&self) -> usize {
        match self {
            Node::Internal { keys, children } => {
                INTERNAL_HEADER + 4 * children.len() + 8 * keys.len()
            }
            Node::Leaf { cells, .. } => {
                LEAF_HEADER
                    + cells
                        .iter()
                        .map(|(_, v)| CELL_OVERHEAD + v.len())
                        .sum::<usize>()
            }
        }
    }

    fn encode(&self) -> Vec<u8> {
        debug_assert!(self.encoded_len() <= PAGE_SIZE, "node overflows page");
        let mut out = Vec::with_capacity(self.encoded_len());
        match self {
            Node::Internal { keys, children } => {
                out.push(PT_INTERNAL);
                out.extend_from_slice(&(keys.len() as u16).to_le_bytes());
                for c in children {
                    out.extend_from_slice(&c.to_le_bytes());
                }
                for k in keys {
                    out.extend_from_slice(&k.to_le_bytes());
                }
            }
            Node::Leaf { cells, next } => {
                out.push(PT_LEAF);
                out.extend_from_slice(&(cells.len() as u16).to_le_bytes());
                out.extend_from_slice(&next.to_le_bytes());
                for (k, v) in cells {
                    out.extend_from_slice(&k.to_le_bytes());
                    out.extend_from_slice(&(v.len() as u32).to_le_bytes());
                    out.extend_from_slice(v);
                }
            }
        }
        out
    }

    fn decode(bytes: &[u8]) -> DbResult<Node> {
        let mut r = Reader::new(bytes);
        match r.read_u8()? {
            PT_INTERNAL => {
                let n = r.read_u16()? as usize;
                let mut children = Vec::with_capacity(n + 1);
                for _ in 0..n + 1 {
                    children.push(r.read_u32()?);
                }
                let mut keys = Vec::with_capacity(n);
                for _ in 0..n {
                    keys.push(r.read_i64()?);
                }
                Ok(Node::Internal { keys, children })
            }
            PT_LEAF => {
                let n = r.read_u16()? as usize;
                let next = r.read_u32()?;
                let mut cells = Vec::with_capacity(n);
                for _ in 0..n {
                    let key = r.read_i64()?;
                    let len = r.read_u32()? as usize;
                    if len > PAGE_SIZE {
                        return Err(DbError::corruption("leaf cell length is impossible"));
                    }
                    cells.push((key, r.read_bytes(len)?.to_vec()));
                }
                Ok(Node::Leaf { cells, next })
            }
            t => Err(DbError::corruption(format!(
                "expected a B+tree page, found page type {t}"
            ))),
        }
    }
}

/// Where to descend in an internal node for `key`:
/// first child whose range can contain it.
fn child_index(keys: &[i64], key: i64) -> usize {
    keys.partition_point(|&k| k <= key)
}

/// Result of inserting into a subtree: the subtree split, producing a new
/// right sibling that the parent must adopt.
struct Split {
    sep: i64,
    right: u32,
}

pub struct BTree {
    pub root: u32,
}

impl BTree {
    /// Allocate a new empty tree (a single empty leaf).
    pub fn create(pager: &mut Pager) -> DbResult<BTree> {
        let root = pager.allocate()?;
        pager.put(root, Node::empty_leaf().encode());
        Ok(BTree { root })
    }

    /// Encoded empty leaf — used to bootstrap page 1 (the catalog root)
    /// before any Pager exists.
    pub fn empty_leaf_bytes() -> Vec<u8> {
        Node::empty_leaf().encode()
    }

    pub fn get(&self, pager: &mut Pager, key: i64) -> DbResult<Option<Vec<u8>>> {
        let mut pid = self.root;
        loop {
            let node = Node::decode(&pager.get(pid)?)?;
            match node {
                Node::Internal { keys, children } => {
                    pid = children[child_index(&keys, key)];
                }
                Node::Leaf { cells, .. } => {
                    return Ok(match cells.binary_search_by_key(&key, |c| c.0) {
                        Ok(i) => Some(cells[i].1.clone()),
                        Err(_) => None,
                    });
                }
            }
        }
    }

    /// Insert or replace. Returns true if an existing key was replaced.
    pub fn insert(&mut self, pager: &mut Pager, key: i64, value: &[u8]) -> DbResult<bool> {
        if value.len() > MAX_VALUE_LEN {
            return Err(DbError::unsupported(format!(
                "row is {} bytes; this engine caps rows at {MAX_VALUE_LEN} bytes \
                 (overflow pages are future work)",
                value.len()
            )));
        }
        let (replaced, split) = Self::insert_into(pager, self.root, key, value)?;
        if let Some(split) = split {
            // The root itself split: grow the tree by one level.
            let new_root = pager.allocate()?;
            let node = Node::Internal {
                keys: vec![split.sep],
                children: vec![self.root, split.right],
            };
            pager.put(new_root, node.encode());
            self.root = new_root;
        }
        Ok(replaced)
    }

    fn insert_into(
        pager: &mut Pager,
        pid: u32,
        key: i64,
        value: &[u8],
    ) -> DbResult<(bool, Option<Split>)> {
        let node = Node::decode(&pager.get(pid)?)?;
        match node {
            Node::Leaf { mut cells, next } => {
                let replaced = match cells.binary_search_by_key(&key, |c| c.0) {
                    Ok(i) => {
                        cells[i].1 = value.to_vec();
                        true
                    }
                    Err(i) => {
                        cells.insert(i, (key, value.to_vec()));
                        false
                    }
                };
                let node = Node::Leaf { cells, next };
                if node.encoded_len() <= PAGE_SIZE {
                    pager.put(pid, node.encode());
                    return Ok((replaced, None));
                }
                // Overfull: split at the size midpoint so both halves fit.
                let (cells, next) = match node {
                    Node::Leaf { cells, next } => (cells, next),
                    _ => unreachable!(),
                };
                let total: usize = cells.iter().map(|(_, v)| CELL_OVERHEAD + v.len()).sum();
                let mut acc = 0usize;
                let mut split_at = cells.len() - 1; // never leave right empty
                for (i, (_, v)) in cells.iter().enumerate() {
                    acc += CELL_OVERHEAD + v.len();
                    if acc >= total / 2 && i + 1 < cells.len() {
                        split_at = i + 1;
                        break;
                    }
                }
                let mut left_cells = cells;
                let right_cells = left_cells.split_off(split_at);
                let sep = right_cells[0].0;
                let right_pid = pager.allocate()?;
                let right = Node::Leaf {
                    cells: right_cells,
                    next,
                };
                let left = Node::Leaf {
                    cells: left_cells,
                    next: right_pid,
                };
                pager.put(right_pid, right.encode());
                pager.put(pid, left.encode());
                Ok((
                    replaced,
                    Some(Split {
                        sep,
                        right: right_pid,
                    }),
                ))
            }
            Node::Internal {
                mut keys,
                mut children,
            } => {
                let i = child_index(&keys, key);
                let (replaced, child_split) = Self::insert_into(pager, children[i], key, value)?;
                let Some(child_split) = child_split else {
                    return Ok((replaced, None));
                };
                keys.insert(i, child_split.sep);
                children.insert(i + 1, child_split.right);
                let node = Node::Internal { keys, children };
                if node.encoded_len() <= PAGE_SIZE {
                    pager.put(pid, node.encode());
                    return Ok((replaced, None));
                }
                // Split the internal node; the middle key moves UP.
                let (mut keys, mut children) = match node {
                    Node::Internal { keys, children } => (keys, children),
                    _ => unreachable!(),
                };
                let mid = keys.len() / 2;
                let sep = keys[mid];
                let right_keys = keys.split_off(mid + 1);
                keys.pop(); // drop the promoted separator
                let right_children = children.split_off(mid + 1);
                let right_pid = pager.allocate()?;
                pager.put(
                    right_pid,
                    Node::Internal {
                        keys: right_keys,
                        children: right_children,
                    }
                    .encode(),
                );
                pager.put(pid, Node::Internal { keys, children }.encode());
                Ok((
                    replaced,
                    Some(Split {
                        sep,
                        right: right_pid,
                    }),
                ))
            }
        }
    }

    /// Remove a key. Lazy: nodes are never merged. Returns true if found.
    pub fn delete(&mut self, pager: &mut Pager, key: i64) -> DbResult<bool> {
        let mut pid = self.root;
        loop {
            let node = Node::decode(&pager.get(pid)?)?;
            match node {
                Node::Internal { keys, children } => {
                    pid = children[child_index(&keys, key)];
                }
                Node::Leaf { mut cells, next } => {
                    return match cells.binary_search_by_key(&key, |c| c.0) {
                        Ok(i) => {
                            cells.remove(i);
                            pager.put(pid, Node::Leaf { cells, next }.encode());
                            Ok(true)
                        }
                        Err(_) => Ok(false),
                    };
                }
            }
        }
    }

    /// Range scan in key order. Calls `f(key, value)` for each cell in
    /// [lo, hi]; `f` returns false to stop early (e.g. LIMIT).
    pub fn scan<F>(
        &self,
        pager: &mut Pager,
        lo: Bound<i64>,
        hi: Bound<i64>,
        f: &mut F,
    ) -> DbResult<()>
    where
        F: FnMut(i64, &[u8]) -> DbResult<bool>,
    {
        // Descend to the leftmost leaf that could contain `lo`.
        let mut pid = self.root;
        let mut leaf = loop {
            let node = Node::decode(&pager.get(pid)?)?;
            match node {
                Node::Internal { keys, children } => {
                    pid = match lo {
                        Bound::Unbounded => children[0],
                        Bound::Included(k) | Bound::Excluded(k) => children[child_index(&keys, k)],
                    };
                }
                leaf @ Node::Leaf { .. } => break leaf,
            }
        };
        // Walk the leaf chain.
        loop {
            let (cells, next) = match leaf {
                Node::Leaf { cells, next } => (cells, next),
                _ => unreachable!(),
            };
            for (key, value) in &cells {
                let after_lo = match lo {
                    Bound::Unbounded => true,
                    Bound::Included(l) => *key >= l,
                    Bound::Excluded(l) => *key > l,
                };
                if !after_lo {
                    continue;
                }
                let before_hi = match hi {
                    Bound::Unbounded => true,
                    Bound::Included(h) => *key <= h,
                    Bound::Excluded(h) => *key < h,
                };
                if !before_hi {
                    return Ok(()); // keys are sorted: nothing further matches
                }
                if !f(*key, value)? {
                    return Ok(());
                }
            }
            if next == 0 {
                return Ok(());
            }
            leaf = Node::decode(&pager.get(next)?)?;
        }
    }

    /// Largest key in the tree (used to seed auto-increment rowids).
    pub fn max_key(&self, pager: &mut Pager) -> DbResult<Option<i64>> {
        let mut pid = self.root;
        loop {
            let node = Node::decode(&pager.get(pid)?)?;
            match node {
                Node::Internal { children, .. } => pid = *children.last().unwrap(),
                Node::Leaf { cells, .. } => {
                    if let Some((k, _)) = cells.last() {
                        return Ok(Some(*k));
                    }
                    // Rightmost leaf is empty (lazy deletes). Rare: fall back
                    // to a full scan for the true maximum.
                    let mut max = None;
                    self.scan(pager, Bound::Unbounded, Bound::Unbounded, &mut |k, _| {
                        max = Some(k);
                        Ok(true)
                    })?;
                    return Ok(max);
                }
            }
        }
    }

    /// Free every page in the tree (DROP TABLE).
    pub fn free_all(&self, pager: &mut Pager) -> DbResult<()> {
        let mut stack = vec![self.root];
        while let Some(pid) = stack.pop() {
            let node = Node::decode(&pager.get(pid)?)?;
            if let Node::Internal { children, .. } = node {
                stack.extend(children);
            }
            pager.free(pid)?;
        }
        Ok(())
    }

    /// JSON snapshot of the tree's shape for the visualizer. `budget` caps
    /// how many nodes are expanded so giant trees stay drawable.
    pub fn layout(&self, pager: &mut Pager, budget: usize) -> DbResult<J> {
        let mut budget = budget;
        self.layout_node(pager, self.root, &mut budget)
    }

    fn layout_node(&self, pager: &mut Pager, pid: u32, budget: &mut usize) -> DbResult<J> {
        if *budget == 0 {
            return Ok(J::O(vec![
                ("pid".into(), J::I(pid as i64)),
                ("kind".into(), J::s("omitted")),
            ]));
        }
        *budget -= 1;
        const KEY_CAP: usize = 12;
        let node = Node::decode(&pager.get(pid)?)?;
        match node {
            Node::Internal { keys, children } => {
                let shown: Vec<J> = keys.iter().take(KEY_CAP).map(|&k| J::I(k)).collect();
                let mut kids = Vec::new();
                for c in &children {
                    kids.push(self.layout_node(pager, *c, budget)?);
                }
                Ok(J::O(vec![
                    ("pid".into(), J::I(pid as i64)),
                    ("kind".into(), J::s("internal")),
                    ("n_keys".into(), J::I(keys.len() as i64)),
                    ("keys".into(), J::A(shown)),
                    ("truncated".into(), J::B(keys.len() > KEY_CAP)),
                    ("children".into(), J::A(kids)),
                ]))
            }
            Node::Leaf { cells, next } => {
                let shown: Vec<J> = cells.iter().take(KEY_CAP).map(|(k, _)| J::I(*k)).collect();
                Ok(J::O(vec![
                    ("pid".into(), J::I(pid as i64)),
                    ("kind".into(), J::s("leaf")),
                    ("n_keys".into(), J::I(cells.len() as i64)),
                    ("keys".into(), J::A(shown)),
                    ("truncated".into(), J::B(cells.len() > KEY_CAP)),
                    ("next".into(), J::I(next as i64)),
                ]))
            }
        }
    }
}
