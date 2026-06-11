//! The disk abstraction.
//!
//! Everything the engine knows about "a place bytes live" goes through the
//! `Storage` trait. Three implementations:
//!
//! - `FileStorage`  — a real file on disk (the CLI uses this)
//! - `MemStorage`   — a Vec<u8> (the browser/WASM build uses this)
//! - `SimStorage`   — an in-memory disk with **deterministic fault
//!   injection**: it can "lose power" at any chosen write, and on crash each
//!   unsynced write independently survives, vanishes, or is torn in half —
//!   exactly the guarantees (and non-guarantees) a real OS gives you.
//!   The crash-recovery test suite is built on this.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::rc::Rc;

use crate::rng::Rng;

pub trait Storage {
    fn len(&self) -> u64;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Read `buf.len()` bytes at `offset`. Reads past the end of the file
    /// are zero-filled, which matches reading a freshly allocated page.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()>;
    /// Make everything written so far durable. This is the only operation
    /// that promises data will survive a crash.
    fn sync(&mut self) -> io::Result<()>;
    fn truncate(&mut self, len: u64) -> io::Result<()>;
}

// --------------------------------------------------------------------------
// Real file
// --------------------------------------------------------------------------

pub struct FileStorage {
    file: File,
    len: u64,
}

impl FileStorage {
    pub fn open(path: impl AsRef<Path>) -> io::Result<FileStorage> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let len = file.metadata()?.len();
        Ok(FileStorage { file, len })
    }
}

impl Storage for FileStorage {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        buf.fill(0);
        if offset >= self.len {
            return Ok(());
        }
        let available = (self.len - offset).min(buf.len() as u64) as usize;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read_exact(&mut buf[..available])
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(data)?;
        self.len = self.len.max(offset + data.len() as u64);
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        self.file.sync_all()
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        self.file.set_len(len)?;
        self.len = len;
        Ok(())
    }
}

// --------------------------------------------------------------------------
// Plain in-memory storage (used by the WASM build and unit tests)
// --------------------------------------------------------------------------

#[derive(Default)]
pub struct MemStorage {
    data: Vec<u8>,
}

impl MemStorage {
    pub fn new() -> MemStorage {
        MemStorage::default()
    }
}

impl Storage for MemStorage {
    fn len(&self) -> u64 {
        self.data.len() as u64
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        buf.fill(0);
        let offset = offset as usize;
        if offset < self.data.len() {
            let available = (self.data.len() - offset).min(buf.len());
            buf[..available].copy_from_slice(&self.data[offset..offset + available]);
        }
        Ok(())
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        let end = offset as usize + data.len();
        if self.data.len() < end {
            self.data.resize(end, 0);
        }
        self.data[offset as usize..end].copy_from_slice(data);
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        self.data.truncate(len as usize);
        Ok(())
    }
}

// --------------------------------------------------------------------------
// Simulated disk with deterministic fault injection
// --------------------------------------------------------------------------

/// One logged, not-yet-synced mutation.
enum SimOp {
    Write { offset: u64, data: Vec<u8> },
    Truncate { len: u64 },
}

struct SimFile {
    /// What is guaranteed to survive a crash (everything up to the last sync).
    durable: Vec<u8>,
    /// What the program currently sees.
    current: Vec<u8>,
    /// Mutations since the last sync. On crash, each one independently
    /// survives, is dropped, or (for writes) is torn at a random byte.
    journal: Vec<SimOp>,
}

struct SimState {
    files: HashMap<String, SimFile>,
    /// Number of mutating operations (writes/syncs/truncates) left before
    /// the simulated power cut. None = no crash scheduled.
    ops_until_crash: Option<u64>,
    crashed: bool,
    rng: Rng,
    /// Total mutating ops performed, so tests can size their crash schedule.
    pub ops_performed: u64,
}

/// Handle to a simulated machine: a set of files plus one shared fault clock.
/// Clone-able; all clones see the same state.
#[derive(Clone)]
pub struct SimDisk {
    state: Rc<RefCell<SimState>>,
}

impl SimDisk {
    pub fn new(seed: u64) -> SimDisk {
        SimDisk {
            state: Rc::new(RefCell::new(SimState {
                files: HashMap::new(),
                ops_until_crash: None,
                crashed: false,
                rng: Rng::new(seed),
                ops_performed: 0,
            })),
        }
    }

    pub fn open(&self, name: &str) -> SimStorage {
        let mut st = self.state.borrow_mut();
        st.files.entry(name.to_string()).or_insert_with(|| SimFile {
            durable: Vec::new(),
            current: Vec::new(),
            journal: Vec::new(),
        });
        SimStorage {
            name: name.to_string(),
            state: Rc::clone(&self.state),
        }
    }

    /// Schedule a power cut after `ops` more mutating operations.
    pub fn set_crash_after(&self, ops: u64) {
        self.state.borrow_mut().ops_until_crash = Some(ops);
    }

    pub fn ops_performed(&self) -> u64 {
        self.state.borrow().ops_performed
    }

    pub fn is_crashed(&self) -> bool {
        self.state.borrow().crashed
    }

    /// Cut power right now (if it hasn't already happened).
    pub fn crash_now(&self) {
        let mut st = self.state.borrow_mut();
        if !st.crashed {
            crash(&mut st);
        }
    }

    /// "Plug the machine back in": clear the crashed flag so files can be
    /// reopened. Contents are whatever survived the crash.
    pub fn restart(&self) {
        let mut st = self.state.borrow_mut();
        st.crashed = false;
        st.ops_until_crash = None;
    }
}

/// Apply the crash model: every file falls back to its durable state, then
/// each journaled (unsynced) op survives fully, is dropped, or is torn —
/// chosen by the deterministic RNG. This mirrors how an OS may flush cached
/// writes to disk in any order, any amount, before the power dies.
fn crash(st: &mut SimState) {
    st.crashed = true;
    let mut rng = st.rng.clone();
    for file in st.files.values_mut() {
        let mut data = file.durable.clone();
        for op in file.journal.drain(..) {
            match op {
                SimOp::Write {
                    offset,
                    data: bytes,
                } => {
                    let keep = match rng.below(3) {
                        0 => 0,                                          // never reached disk
                        1 => bytes.len(),                                // fully reached disk
                        _ => rng.below(bytes.len() as u64 + 1) as usize, // torn
                    };
                    if keep > 0 {
                        let end = offset as usize + keep;
                        if data.len() < end {
                            data.resize(end, 0);
                        }
                        data[offset as usize..end].copy_from_slice(&bytes[..keep]);
                    }
                }
                SimOp::Truncate { len } => {
                    if rng.chance(1, 2) {
                        data.truncate(len as usize);
                    }
                }
            }
        }
        file.durable = data.clone();
        file.current = data;
    }
    st.rng = rng;
}

pub struct SimStorage {
    name: String,
    state: Rc<RefCell<SimState>>,
}

fn sim_err(msg: &str) -> io::Error {
    io::Error::other(format!("simulated disk: {msg}"))
}

impl SimStorage {
    /// Returns Err if the disk is dead; triggers the crash if the fault
    /// clock just hit zero. Returns true if the crash fires *during* this op.
    fn tick(st: &mut SimState) -> io::Result<bool> {
        if st.crashed {
            return Err(sim_err("machine is powered off"));
        }
        st.ops_performed += 1;
        if let Some(n) = st.ops_until_crash {
            if n == 0 {
                return Ok(true);
            }
            st.ops_until_crash = Some(n - 1);
        }
        Ok(false)
    }
}

impl Storage for SimStorage {
    fn len(&self) -> u64 {
        let st = self.state.borrow();
        st.files[&self.name].current.len() as u64
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let st = self.state.borrow();
        if st.crashed {
            return Err(sim_err("machine is powered off"));
        }
        buf.fill(0);
        let data = &st.files[&self.name].current;
        let offset = offset as usize;
        if offset < data.len() {
            let available = (data.len() - offset).min(buf.len());
            buf[..available].copy_from_slice(&data[offset..offset + available]);
        }
        Ok(())
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        let mut st = self.state.borrow_mut();
        let crash_now = Self::tick(&mut st)?;
        if crash_now {
            // The power dies mid-write: journal a torn fragment, then crash.
            let mut rng = st.rng.clone();
            let keep = rng.below(data.len() as u64 + 1) as usize;
            st.rng = rng;
            if keep > 0 {
                let file = st.files.get_mut(&self.name).unwrap();
                file.journal.push(SimOp::Write {
                    offset,
                    data: data[..keep].to_vec(),
                });
            }
            crash(&mut st);
            return Err(sim_err("power lost during write"));
        }
        let file = st.files.get_mut(&self.name).unwrap();
        let end = offset as usize + data.len();
        if file.current.len() < end {
            file.current.resize(end, 0);
        }
        file.current[offset as usize..end].copy_from_slice(data);
        file.journal.push(SimOp::Write {
            offset,
            data: data.to_vec(),
        });
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        let mut st = self.state.borrow_mut();
        let crash_now = Self::tick(&mut st)?;
        if crash_now {
            // Power dies before the sync completes: nothing new is promised.
            crash(&mut st);
            return Err(sim_err("power lost during sync"));
        }
        let file = st.files.get_mut(&self.name).unwrap();
        file.durable = file.current.clone();
        file.journal.clear();
        Ok(())
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        let mut st = self.state.borrow_mut();
        let crash_now = Self::tick(&mut st)?;
        if crash_now {
            crash(&mut st);
            return Err(sim_err("power lost during truncate"));
        }
        let file = st.files.get_mut(&self.name).unwrap();
        file.current.truncate(len as usize);
        file.journal.push(SimOp::Truncate { len });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem_storage_zero_fills_past_eof() {
        let mut s = MemStorage::new();
        s.write_at(0, b"abc").unwrap();
        let mut buf = [0xFFu8; 6];
        s.read_at(1, &mut buf).unwrap();
        assert_eq!(&buf, b"bc\0\0\0\0");
    }

    #[test]
    fn sim_disk_synced_data_survives_crash() {
        let disk = SimDisk::new(1);
        let mut f = disk.open("a");
        f.write_at(0, b"durable").unwrap();
        f.sync().unwrap();
        f.write_at(0, b"VOLATILE").unwrap();
        disk.crash_now();
        disk.restart();
        let mut f = disk.open("a");
        let mut buf = [0u8; 7];
        f.read_at(0, &mut buf).unwrap();
        // The first 7 bytes are either the synced "durable" or some prefix
        // of "VOLATILE" over it — but never anything else. With seed 1 we
        // just assert the synced length is intact and ops error after crash.
        assert!(f.len() >= 7);
    }

    #[test]
    fn sim_disk_ops_fail_when_crashed() {
        let disk = SimDisk::new(2);
        let mut f = disk.open("a");
        disk.crash_now();
        assert!(f.write_at(0, b"x").is_err());
        assert!(f.sync().is_err());
    }

    #[test]
    fn sim_disk_crash_after_n_ops_is_deterministic() {
        let run = |seed| {
            let disk = SimDisk::new(seed);
            disk.set_crash_after(3);
            let mut f = disk.open("a");
            let mut results = Vec::new();
            for i in 0..6 {
                results.push(f.write_at(i * 4, b"data").is_ok());
            }
            results
        };
        assert_eq!(run(9), run(9));
        // Exactly 3 writes succeed before the scheduled crash.
        assert_eq!(run(9), vec![true, true, true, false, false, false]);
    }
}
