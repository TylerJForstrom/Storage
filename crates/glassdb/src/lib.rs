//! # GlassDB
//!
//! A tiny, crash-safe SQL database engine you can watch think.
//!
//! - **Pager** ([`pager`]): 4 KiB pages, buffer pool with LRU eviction.
//! - **Write-ahead log** ([`wal`]): CRC-checked frames; committed
//!   transactions survive power loss, uncommitted ones vanish.
//! - **B+tree** ([`btree`]): point lookups, range scans over a leaf chain.
//! - **SQL** ([`sql`]): hand-rolled lexer/parser with position-carrying
//!   errors.
//! - **Planner** ([`planner`]): chooses PK lookup / range scan / full scan,
//!   and explains itself.
//! - **Tracing** ([`trace`]): every page read and WAL write is observable —
//!   this powers the browser visualizer.
//!
//! Zero dependencies. No unsafe code. Every byte on disk is written by code
//! in this crate.
//!
//! ```
//! use glassdb::Database;
//!
//! let mut db = Database::open_memory().unwrap();
//! db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)").unwrap();
//! db.execute("INSERT INTO t (name) VALUES ('hello'), ('world')").unwrap();
//! let r = db.execute("SELECT name FROM t WHERE id = 2").unwrap();
//! assert_eq!(r.rows[0][0].to_string(), "world");
//! ```

#![forbid(unsafe_code)]

pub mod btree;
pub mod catalog;
pub mod crc;
pub mod db;
pub mod errors;
pub mod executor;
pub mod json;
pub mod pager;
pub mod planner;
pub mod rng;
pub mod sql;
pub mod storage;
pub mod trace;
pub mod types;
pub mod wal;

pub use db::{Database, QueryResult};
pub use errors::{DbError, DbResult, ErrorKind};
pub use types::Value;
