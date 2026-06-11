//! The pager: fixed-size pages plus an in-memory buffer pool.
//!
//! The database file is an array of 4096-byte pages. Page 0 is the header;
//! every other page belongs to a B+tree (internal/leaf), the freelist, or is
//! unused. Higher layers never touch the file directly — they ask the pager
//! for pages by id, modify copies, and `put` them back. Dirty pages live in
//! the buffer pool until commit, when the database (via the WAL) writes them
//! out. That "no dirty page reaches the database file before its WAL frame
//! is durable" rule is the entire crash-safety story.

use std::collections::{HashMap, HashSet};

use crate::errors::{DbError, DbResult};
use crate::storage::Storage;
use crate::trace::{PageKind, SharedTrace, TraceEvent};

pub const PAGE_SIZE: usize = 4096;
const MAGIC: &[u8; 8] = b"GLASSDB1";
const VERSION: u32 = 1;

/// How many pages the buffer pool may hold before evicting clean ones.
/// 256 pages = 1 MiB. Dirty pages are pinned and never evicted mid-commit.
const DEFAULT_CACHE_PAGES: usize = 256;

/// Page type tags (first byte of every non-header page).
pub const PT_INTERNAL: u8 = 1;
pub const PT_LEAF: u8 = 2;
pub const PT_FREE: u8 = 3;

#[derive(Debug, Clone)]
pub struct DbHeader {
    pub page_count: u32,
    pub freelist_head: u32, // 0 = empty (page 0 is the header, never free)
    pub catalog_root: u32,
}

impl DbHeader {
    fn encode(&self) -> Vec<u8> {
        let mut page = vec![0u8; PAGE_SIZE];
        page[0..8].copy_from_slice(MAGIC);
        page[8..12].copy_from_slice(&VERSION.to_le_bytes());
        page[12..16].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
        page[16..20].copy_from_slice(&self.page_count.to_le_bytes());
        page[20..24].copy_from_slice(&self.freelist_head.to_le_bytes());
        page[24..28].copy_from_slice(&self.catalog_root.to_le_bytes());
        page
    }

    fn decode(page: &[u8]) -> DbResult<DbHeader> {
        if &page[0..8] != MAGIC {
            return Err(DbError::corruption(
                "not a GlassDB file (bad magic in header page)",
            ));
        }
        let version = u32::from_le_bytes(page[8..12].try_into().unwrap());
        if version != VERSION {
            return Err(DbError::corruption(format!(
                "file version {version} not supported (expected {VERSION})"
            )));
        }
        let page_size = u32::from_le_bytes(page[12..16].try_into().unwrap());
        if page_size as usize != PAGE_SIZE {
            return Err(DbError::corruption(format!(
                "file uses {page_size}-byte pages, this build uses {PAGE_SIZE}"
            )));
        }
        Ok(DbHeader {
            page_count: u32::from_le_bytes(page[16..20].try_into().unwrap()),
            freelist_head: u32::from_le_bytes(page[20..24].try_into().unwrap()),
            catalog_root: u32::from_le_bytes(page[24..28].try_into().unwrap()),
        })
    }
}

pub struct Pager {
    storage: Box<dyn Storage>,
    cache: HashMap<u32, Vec<u8>>,
    /// Approximate LRU order: most recently used at the back.
    lru: Vec<u32>,
    dirty: HashSet<u32>,
    pub header: DbHeader,
    pub header_dirty: bool,
    trace: SharedTrace,
    cache_limit: usize,
}

impl Pager {
    /// Initialize a brand-new database file: catalog root page + header.
    /// Written directly (not via WAL) — the file is empty, so there is
    /// nothing a crash could corrupt yet.
    ///
    /// Order matters: page 1 is written and SYNCED before the header. Disks
    /// may persist unsynced writes in any order, so a valid header must be
    /// the *proof* that everything else already made it. If power dies
    /// mid-bootstrap the header stays invalid and the next open simply
    /// re-bootstraps (see `Database::open_with`).
    pub fn bootstrap(storage: &mut dyn Storage, catalog_root_page: &[u8]) -> DbResult<()> {
        let mut page = vec![0u8; PAGE_SIZE];
        page[..catalog_root_page.len()].copy_from_slice(catalog_root_page);
        storage.write_at(PAGE_SIZE as u64, &page)?;
        storage.sync()?;
        let header = DbHeader {
            page_count: 2,
            freelist_head: 0,
            catalog_root: 1,
        };
        storage.write_at(0, &header.encode())?;
        storage.sync()?;
        Ok(())
    }

    /// Does the file start with a valid header page?
    pub fn header_valid(storage: &mut dyn Storage) -> bool {
        if storage.len() < PAGE_SIZE as u64 {
            return false;
        }
        let mut page0 = vec![0u8; PAGE_SIZE];
        if storage.read_at(0, &mut page0).is_err() {
            return false;
        }
        DbHeader::decode(&page0).is_ok()
    }

    /// Open an existing database file (call `bootstrap` first if empty).
    pub fn open(mut storage: Box<dyn Storage>, trace: SharedTrace) -> DbResult<Pager> {
        if storage.len() < PAGE_SIZE as u64 {
            return Err(DbError::corruption(
                "database file is smaller than one page and not empty",
            ));
        }
        let mut page0 = vec![0u8; PAGE_SIZE];
        storage.read_at(0, &mut page0)?;
        let header = DbHeader::decode(&page0)?;
        Ok(Pager {
            storage,
            cache: HashMap::new(),
            lru: Vec::new(),
            dirty: HashSet::new(),
            header,
            header_dirty: false,
            trace,
            cache_limit: DEFAULT_CACHE_PAGES,
        })
    }

    fn kind_of(pid: u32, data: &[u8]) -> PageKind {
        if pid == 0 {
            return PageKind::Header;
        }
        match data.first() {
            Some(&PT_INTERNAL) => PageKind::Internal,
            Some(&PT_LEAF) => PageKind::Leaf,
            Some(&PT_FREE) => PageKind::Free,
            _ => PageKind::Unknown,
        }
    }

    fn touch(&mut self, pid: u32) {
        if let Some(i) = self.lru.iter().position(|&p| p == pid) {
            self.lru.remove(i);
        }
        self.lru.push(pid);
    }

    /// Evict clean pages if the pool is over its limit. Dirty pages are
    /// pinned: they may only leave through a commit (or rollback).
    fn evict_if_needed(&mut self) {
        while self.cache.len() > self.cache_limit {
            let candidate = self.lru.iter().position(|pid| !self.dirty.contains(pid));
            match candidate {
                Some(i) => {
                    let pid = self.lru.remove(i);
                    self.cache.remove(&pid);
                }
                None => break, // everything is dirty; nothing evictable
            }
        }
    }

    /// Logical page read. Returns a copy the caller may freely modify
    /// (call `put` to make the modification real).
    pub fn get(&mut self, pid: u32) -> DbResult<Vec<u8>> {
        if let Some(data) = self.cache.get(&pid) {
            let data = data.clone();
            let kind = Self::kind_of(pid, &data);
            {
                let mut t = self.trace.borrow_mut();
                t.stats.pages_read += 1;
                t.stats.cache_hits += 1;
                t.emit(TraceEvent::PageRead {
                    pid,
                    kind,
                    cached: true,
                });
            }
            self.touch(pid);
            return Ok(data);
        }
        let mut data = vec![0u8; PAGE_SIZE];
        self.storage
            .read_at(pid as u64 * PAGE_SIZE as u64, &mut data)?;
        let kind = Self::kind_of(pid, &data);
        {
            let mut t = self.trace.borrow_mut();
            t.stats.pages_read += 1;
            t.emit(TraceEvent::PageRead {
                pid,
                kind,
                cached: false,
            });
        }
        self.cache.insert(pid, data.clone());
        self.touch(pid);
        self.evict_if_needed();
        Ok(data)
    }

    /// Replace a page's contents in the buffer pool and mark it dirty.
    /// Nothing touches the file until commit.
    pub fn put(&mut self, pid: u32, mut data: Vec<u8>) {
        data.resize(PAGE_SIZE, 0);
        self.cache.insert(pid, data);
        self.dirty.insert(pid);
        self.touch(pid);
        self.evict_if_needed();
    }

    /// Hand out a page id: reuse the freelist if possible, else grow the file.
    pub fn allocate(&mut self) -> DbResult<u32> {
        let pid = if self.header.freelist_head != 0 {
            let pid = self.header.freelist_head;
            let page = self.get(pid)?;
            self.header.freelist_head = u32::from_le_bytes(page[1..5].try_into().unwrap());
            pid
        } else {
            let pid = self.header.page_count;
            self.header.page_count += 1;
            pid
        };
        self.header_dirty = true;
        self.trace.borrow_mut().emit(TraceEvent::PageAlloc { pid });
        Ok(pid)
    }

    /// Return a page to the freelist (used by DROP TABLE).
    pub fn free(&mut self, pid: u32) -> DbResult<()> {
        let mut page = vec![0u8; PAGE_SIZE];
        page[0] = PT_FREE;
        page[1..5].copy_from_slice(&self.header.freelist_head.to_le_bytes());
        self.put(pid, page);
        self.header.freelist_head = pid;
        self.header_dirty = true;
        self.trace.borrow_mut().emit(TraceEvent::PageFree { pid });
        Ok(())
    }

    pub fn has_dirty(&self) -> bool {
        !self.dirty.is_empty() || self.header_dirty
    }

    /// Everything that must be made durable to commit the current
    /// transaction: each dirty page, plus the header page if it changed.
    /// Sorted by page id for deterministic WAL contents.
    pub fn commit_set(&self) -> Vec<(u32, Vec<u8>)> {
        let mut pages: Vec<(u32, Vec<u8>)> = self
            .dirty
            .iter()
            .map(|&pid| (pid, self.cache[&pid].clone()))
            .collect();
        if self.header_dirty {
            pages.push((0, self.header.encode()));
        }
        pages.sort_by_key(|(pid, _)| *pid);
        pages
    }

    /// Write the committed pages into the database file and clear dirty
    /// state. Only called after the WAL holds these pages durably.
    pub fn apply_commit(&mut self, pages: &[(u32, Vec<u8>)]) -> DbResult<()> {
        for (pid, data) in pages {
            self.storage
                .write_at(*pid as u64 * PAGE_SIZE as u64, data)?;
            let mut t = self.trace.borrow_mut();
            t.stats.pages_written += 1;
            t.emit(TraceEvent::PageWrite { pid: *pid });
        }
        self.dirty.clear();
        self.header_dirty = false;
        Ok(())
    }

    /// Throw away every uncommitted change: drop dirty pages from the pool
    /// and re-read the header from disk.
    pub fn rollback(&mut self) -> DbResult<()> {
        for pid in self.dirty.drain() {
            self.cache.remove(&pid);
            if let Some(i) = self.lru.iter().position(|&p| p == pid) {
                self.lru.remove(i);
            }
        }
        let mut page0 = vec![0u8; PAGE_SIZE];
        self.storage.read_at(0, &mut page0)?;
        self.header = DbHeader::decode(&page0)?;
        self.header_dirty = false;
        Ok(())
    }

    pub fn sync_storage(&mut self) -> DbResult<()> {
        self.storage.sync()?;
        Ok(())
    }

    pub fn cached_pages(&self) -> usize {
        self.cache.len()
    }
}
