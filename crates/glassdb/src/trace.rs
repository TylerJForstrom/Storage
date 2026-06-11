//! The instrumentation layer — what makes GlassDB a *glass* database.
//!
//! Every layer of the engine (pager, WAL, B+tree, executor) emits trace
//! events into a shared sink while a statement runs. The CLI can print them
//! and the web visualizer animates them: pages lighting up as they're read,
//! WAL frames appending, checkpoints firing.

use std::cell::RefCell;
use std::rc::Rc;

use crate::json::J;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageKind {
    Header,
    Internal,
    Leaf,
    Free,
    Unknown,
}

impl PageKind {
    pub fn name(self) -> &'static str {
        match self {
            PageKind::Header => "header",
            PageKind::Internal => "internal",
            PageKind::Leaf => "leaf",
            PageKind::Free => "free",
            PageKind::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone)]
pub enum TraceEvent {
    /// A logical page read. `cached` tells you whether the buffer pool
    /// already had it (no disk touch) or it came from storage.
    PageRead {
        pid: u32,
        kind: PageKind,
        cached: bool,
    },
    /// A page written back to the database file (during commit).
    PageWrite {
        pid: u32,
    },
    PageAlloc {
        pid: u32,
    },
    PageFree {
        pid: u32,
    },
    /// A page image appended to the write-ahead log.
    WalFrame {
        lsn: u64,
        pid: u32,
    },
    /// A commit record appended to the WAL — the moment a transaction
    /// becomes durable (after the following sync).
    WalCommit {
        lsn: u64,
    },
    WalSync,
    /// WAL applied to the main file and reset.
    WalCheckpoint {
        frames: u64,
    },
    /// Crash recovery ran at startup.
    WalRecovery {
        frames: u64,
        txns: u64,
    },
    Note {
        text: String,
    },
}

impl TraceEvent {
    pub fn to_json(&self) -> J {
        match self {
            TraceEvent::PageRead { pid, kind, cached } => J::O(vec![
                ("type".into(), J::s("page_read")),
                ("pid".into(), J::I(*pid as i64)),
                ("kind".into(), J::s(kind.name())),
                ("cached".into(), J::B(*cached)),
            ]),
            TraceEvent::PageWrite { pid } => J::O(vec![
                ("type".into(), J::s("page_write")),
                ("pid".into(), J::I(*pid as i64)),
            ]),
            TraceEvent::PageAlloc { pid } => J::O(vec![
                ("type".into(), J::s("page_alloc")),
                ("pid".into(), J::I(*pid as i64)),
            ]),
            TraceEvent::PageFree { pid } => J::O(vec![
                ("type".into(), J::s("page_free")),
                ("pid".into(), J::I(*pid as i64)),
            ]),
            TraceEvent::WalFrame { lsn, pid } => J::O(vec![
                ("type".into(), J::s("wal_frame")),
                ("lsn".into(), J::I(*lsn as i64)),
                ("pid".into(), J::I(*pid as i64)),
            ]),
            TraceEvent::WalCommit { lsn } => J::O(vec![
                ("type".into(), J::s("wal_commit")),
                ("lsn".into(), J::I(*lsn as i64)),
            ]),
            TraceEvent::WalSync => J::O(vec![("type".into(), J::s("wal_sync"))]),
            TraceEvent::WalCheckpoint { frames } => J::O(vec![
                ("type".into(), J::s("wal_checkpoint")),
                ("frames".into(), J::I(*frames as i64)),
            ]),
            TraceEvent::WalRecovery { frames, txns } => J::O(vec![
                ("type".into(), J::s("wal_recovery")),
                ("frames".into(), J::I(*frames as i64)),
                ("txns".into(), J::I(*txns as i64)),
            ]),
            TraceEvent::Note { text } => J::O(vec![
                ("type".into(), J::s("note")),
                ("text".into(), J::s(text.clone())),
            ]),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub pages_read: u64,
    pub cache_hits: u64,
    pub pages_written: u64,
    pub wal_frames: u64,
    pub wal_syncs: u64,
    pub rows_scanned: u64,
    pub rows_returned: u64,
    pub elapsed_micros: u64,
}

impl Stats {
    pub fn to_json(&self) -> J {
        J::O(vec![
            ("pages_read".into(), J::I(self.pages_read as i64)),
            ("cache_hits".into(), J::I(self.cache_hits as i64)),
            ("pages_written".into(), J::I(self.pages_written as i64)),
            ("wal_frames".into(), J::I(self.wal_frames as i64)),
            ("wal_syncs".into(), J::I(self.wal_syncs as i64)),
            ("rows_scanned".into(), J::I(self.rows_scanned as i64)),
            ("rows_returned".into(), J::I(self.rows_returned as i64)),
            ("elapsed_micros".into(), J::I(self.elapsed_micros as i64)),
        ])
    }
}

/// Cap on stored events per statement so a huge scan can't eat all memory.
/// Past the cap we keep counting (`dropped`) but stop storing.
const EVENT_CAP: usize = 20_000;

#[derive(Default)]
pub struct Trace {
    pub events: Vec<TraceEvent>,
    pub dropped: u64,
    pub stats: Stats,
}

impl Trace {
    /// Reset for a new statement.
    pub fn begin(&mut self) {
        self.events.clear();
        self.dropped = 0;
        self.stats = Stats::default();
    }

    pub fn emit(&mut self, event: TraceEvent) {
        if self.events.len() < EVENT_CAP {
            self.events.push(event);
        } else {
            self.dropped += 1;
        }
    }
}

pub type SharedTrace = Rc<RefCell<Trace>>;

pub fn new_shared() -> SharedTrace {
    Rc::new(RefCell::new(Trace::default()))
}
