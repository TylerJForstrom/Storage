//! The write-ahead log — how GlassDB survives power loss.
//!
//! Commit protocol (the order is everything):
//!
//!   1. Append every dirty page's full image to the WAL as a frame.
//!   2. Append a commit record.
//!   3. `sync` the WAL.            <- the transaction is durable HERE
//!   4. Write the pages into the main database file.
//!   5. (eventually) checkpoint: sync the database file, reset the WAL.
//!
//! If the machine dies at any point, recovery on the next open replays every
//! frame that has a valid checksum *and* a following commit record, and
//! ignores everything after the first torn/invalid frame. Committed
//! transactions always survive; uncommitted ones always vanish. Replaying a
//! frame twice is harmless because frames are full page images (idempotent).
//!
//! Frame layout:
//!   [lsn u64][page_id u32][flag u8][data_len u32][data][crc32 u32]
//! flag 0 = page image, flag 1 = commit record (data_len 0).
//! The CRC covers everything before it in the frame.

use crate::crc::crc32;
use crate::errors::{DbError, DbResult};
use crate::pager::PAGE_SIZE;
use crate::storage::Storage;
use crate::trace::{SharedTrace, TraceEvent};

const WAL_MAGIC: &[u8; 8] = b"GLASSWAL";
const WAL_HEADER_LEN: u64 = 16;
const FRAME_HEADER_LEN: usize = 8 + 4 + 1 + 4;

const FLAG_PAGE: u8 = 0;
const FLAG_COMMIT: u8 = 1;

/// Checkpoint when the WAL holds this many frames. Small enough to watch
/// checkpoints happen in the demo, big enough to batch real work.
const CHECKPOINT_FRAMES: u64 = 256;

pub struct Wal {
    storage: Box<dyn Storage>,
    write_off: u64,
    next_lsn: u64,
    pub frames_since_checkpoint: u64,
    trace: SharedTrace,
}

impl Wal {
    pub fn open(mut storage: Box<dyn Storage>, trace: SharedTrace) -> DbResult<Wal> {
        if storage.len() < WAL_HEADER_LEN {
            // Brand new (or torn before the header finished — in which case
            // no commit can exist, so rewriting the header is safe).
            let mut header = [0u8; WAL_HEADER_LEN as usize];
            header[0..8].copy_from_slice(WAL_MAGIC);
            header[8..12].copy_from_slice(&1u32.to_le_bytes());
            storage.truncate(0)?;
            storage.write_at(0, &header)?;
            storage.sync()?;
        } else {
            let mut magic = [0u8; 8];
            storage.read_at(0, &mut magic)?;
            if &magic != WAL_MAGIC {
                return Err(DbError::corruption("WAL file has bad magic"));
            }
        }
        let write_off = storage.len();
        Ok(Wal {
            storage,
            write_off,
            next_lsn: 1,
            frames_since_checkpoint: 0,
            trace,
        })
    }

    fn append_frame(&mut self, pid: u32, flag: u8, data: &[u8]) -> DbResult<u64> {
        let lsn = self.next_lsn;
        self.next_lsn += 1;
        let mut frame = Vec::with_capacity(FRAME_HEADER_LEN + data.len() + 4);
        frame.extend_from_slice(&lsn.to_le_bytes());
        frame.extend_from_slice(&pid.to_le_bytes());
        frame.push(flag);
        frame.extend_from_slice(&(data.len() as u32).to_le_bytes());
        frame.extend_from_slice(data);
        let crc = crc32(&frame);
        frame.extend_from_slice(&crc.to_le_bytes());
        self.storage.write_at(self.write_off, &frame)?;
        self.write_off += frame.len() as u64;
        self.frames_since_checkpoint += 1;
        Ok(lsn)
    }

    /// Step 1 of commit: append one dirty page image.
    pub fn append_page(&mut self, pid: u32, data: &[u8]) -> DbResult<u64> {
        let lsn = self.append_frame(pid, FLAG_PAGE, data)?;
        let mut t = self.trace.borrow_mut();
        t.stats.wal_frames += 1;
        t.emit(TraceEvent::WalFrame { lsn, pid });
        Ok(lsn)
    }

    /// Step 2 of commit: append the commit record.
    pub fn append_commit(&mut self) -> DbResult<u64> {
        let lsn = self.append_frame(0, FLAG_COMMIT, &[])?;
        self.trace.borrow_mut().emit(TraceEvent::WalCommit { lsn });
        Ok(lsn)
    }

    /// Step 3 of commit: make the log durable.
    pub fn sync(&mut self) -> DbResult<()> {
        self.storage.sync()?;
        let mut t = self.trace.borrow_mut();
        t.stats.wal_syncs += 1;
        t.emit(TraceEvent::WalSync);
        Ok(())
    }

    pub fn should_checkpoint(&self) -> bool {
        self.frames_since_checkpoint >= CHECKPOINT_FRAMES
    }

    /// Reset the log to empty (after the database file has been synced).
    pub fn reset(&mut self) -> DbResult<()> {
        let frames = self.frames_since_checkpoint;
        self.storage.truncate(WAL_HEADER_LEN)?;
        self.storage.sync()?;
        self.write_off = WAL_HEADER_LEN;
        self.frames_since_checkpoint = 0;
        self.trace
            .borrow_mut()
            .emit(TraceEvent::WalCheckpoint { frames });
        Ok(())
    }

    /// Crash recovery: replay committed transactions into the database file.
    /// Called once, on open, before the pager reads anything.
    pub fn recover(&mut self, db: &mut dyn Storage) -> DbResult<(u64, u64)> {
        let total = self.storage.len();
        if total <= WAL_HEADER_LEN {
            return Ok((0, 0));
        }
        let mut buf = vec![0u8; (total - WAL_HEADER_LEN) as usize];
        self.storage.read_at(WAL_HEADER_LEN, &mut buf)?;

        let mut pos = 0usize;
        let mut pending: Vec<(u32, Vec<u8>)> = Vec::new();
        let mut frames = 0u64;
        let mut txns = 0u64;
        let mut max_lsn = 0u64;
        let mut applied_any = false;

        loop {
            if pos + FRAME_HEADER_LEN + 4 > buf.len() {
                break; // not even a full header + crc left: torn tail
            }
            let lsn = u64::from_le_bytes(buf[pos..pos + 8].try_into().unwrap());
            let pid = u32::from_le_bytes(buf[pos + 8..pos + 12].try_into().unwrap());
            let flag = buf[pos + 12];
            let len = u32::from_le_bytes(buf[pos + 13..pos + 17].try_into().unwrap()) as usize;
            // A page frame carries exactly one page; anything else is torn
            // or garbage, and nothing after it can be trusted.
            let len_ok = match flag {
                FLAG_PAGE => len == PAGE_SIZE,
                FLAG_COMMIT => len == 0,
                _ => false,
            };
            if !len_ok || pos + FRAME_HEADER_LEN + len + 4 > buf.len() {
                break;
            }
            let body_end = pos + FRAME_HEADER_LEN + len;
            let stored_crc = u32::from_le_bytes(buf[body_end..body_end + 4].try_into().unwrap());
            if crc32(&buf[pos..body_end]) != stored_crc {
                break; // torn frame: stop trusting the log here
            }
            frames += 1;
            max_lsn = max_lsn.max(lsn);
            if flag == FLAG_COMMIT {
                // This transaction fully made it to the log: replay it.
                for (page_id, data) in pending.drain(..) {
                    db.write_at(page_id as u64 * PAGE_SIZE as u64, &data)?;
                    applied_any = true;
                }
                txns += 1;
            } else {
                pending.push((pid, buf[pos + FRAME_HEADER_LEN..body_end].to_vec()));
            }
            pos = body_end + 4;
        }
        // `pending` now holds frames from a transaction that never committed
        // — dropped on the floor, which is exactly what we want.

        if applied_any {
            db.sync()?;
        }
        // Reset the log: everything replayable has been applied and synced.
        self.storage.truncate(WAL_HEADER_LEN)?;
        self.storage.sync()?;
        self.write_off = WAL_HEADER_LEN;
        self.frames_since_checkpoint = 0;
        self.next_lsn = max_lsn + 1;

        if frames > 0 {
            self.trace
                .borrow_mut()
                .emit(TraceEvent::WalRecovery { frames, txns });
        }
        Ok((frames, txns))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MemStorage;
    use crate::trace::new_shared;

    fn page_of(byte: u8) -> Vec<u8> {
        vec![byte; PAGE_SIZE]
    }

    #[test]
    fn committed_frames_replay_into_db() {
        let trace = new_shared();
        let mut wal = Wal::open(Box::new(MemStorage::new()), trace.clone()).unwrap();
        wal.append_page(3, &page_of(0xAA)).unwrap();
        wal.append_commit().unwrap();
        wal.sync().unwrap();

        let mut db = MemStorage::new();
        // Recovery uses a fresh Wal over the same storage in real life, but
        // calling it directly on this one exercises the same path.
        let (frames, txns) = wal.recover(&mut db).unwrap();
        assert_eq!((frames, txns), (2, 1));
        let mut buf = vec![0u8; PAGE_SIZE];
        db.read_at(3 * PAGE_SIZE as u64, &mut buf).unwrap();
        assert_eq!(buf, page_of(0xAA));
    }

    #[test]
    fn uncommitted_frames_are_ignored() {
        let trace = new_shared();
        let mut wal = Wal::open(Box::new(MemStorage::new()), trace).unwrap();
        wal.append_page(3, &page_of(0xBB)).unwrap();
        // no commit record, no sync — like a crash mid-transaction
        let mut db = MemStorage::new();
        let (_, txns) = wal.recover(&mut db).unwrap();
        assert_eq!(txns, 0);
        assert_eq!(db.len(), 0); // nothing was written
    }

    #[test]
    fn torn_tail_stops_recovery_cleanly() {
        let trace = new_shared();
        let mut wal = Wal::open(Box::new(MemStorage::new()), trace).unwrap();
        wal.append_page(1, &page_of(0x11)).unwrap();
        wal.append_commit().unwrap();
        // Simulate a torn extra frame: garbage appended after the commit.
        let off = wal.write_off;
        wal.storage
            .write_at(off, &[0xDE, 0xAD, 0xBE, 0xEF])
            .unwrap();
        wal.write_off += 4;

        let mut db = MemStorage::new();
        let (frames, txns) = wal.recover(&mut db).unwrap();
        assert_eq!((frames, txns), (2, 1)); // good frames applied, garbage ignored
    }
}
