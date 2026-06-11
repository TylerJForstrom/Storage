//! Deterministic crash-injection testing, FoundationDB/TigerBeetle style.
//!
//! A fixed workload of transactions runs against a simulated disk that cuts
//! power after N operations — and we run it for EVERY N from 0 to "the
//! whole workload", across several RNG seeds. After each crash we reopen
//! the database (which runs WAL recovery) and assert the ACID contract:
//!
//!   1. every transaction that reported success is fully there      (D)
//!   2. no transaction that didn't commit left anything behind      (A)
//!   3. the in-flight commit appears either completely or not at all (A)
//!   4. recovery is idempotent: reopening again changes nothing
//!   5. the recovered database still accepts new writes
//!
//! The crash model is harsher than "the file just stops": unsynced writes
//! may individually survive, vanish, or tear in half, in any combination —
//! which is what real disks are allowed to do.

use std::collections::HashSet;

use glassdb::storage::SimDisk;
use glassdb::{Database, DbResult, Value};

fn open_db(disk: &SimDisk) -> DbResult<Database> {
    Database::open_with(Box::new(disk.open("db")), Box::new(disk.open("wal")))
}

#[derive(Default)]
struct Expectation {
    /// Keys whose insert (and no later delete) was acknowledged: MUST exist.
    must_present: HashSet<i64>,
    /// Keys that were rolled back or whose delete was acknowledged: MUST NOT exist.
    must_absent: HashSet<i64>,
    /// Keys whose fate is legitimately unknown (a commit was in flight when
    /// the power died). MAY exist.
    unknown: HashSet<i64>,
    /// If the in-flight commit covered several keys, they must appear
    /// atomically: all or none.
    atomic_unit: Option<HashSet<i64>>,
}

/// Runs the workload until it finishes or the disk dies. Returns what must
/// be true of the database afterwards.
fn run_workload(disk: &SimDisk) -> Expectation {
    let mut exp = Expectation::default();
    let Ok(mut db) = open_db(disk) else {
        return exp;
    };

    if db
        .execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
        .is_err()
    {
        return exp;
    }

    for i in 0..30i64 {
        let base = i * 100;
        match i % 7 {
            // A transaction we explicitly roll back: keys must never appear.
            3 => {
                if db.execute("BEGIN").is_err() {
                    return exp;
                }
                for k in base..base + 3 {
                    exp.must_absent.insert(k);
                    if db
                        .execute(&format!("INSERT INTO t VALUES ({k}, 'x')"))
                        .is_err()
                    {
                        return exp; // never committed: stays must_absent
                    }
                }
                if db.execute("ROLLBACK").is_err() {
                    return exp;
                }
            }
            // A multi-statement transaction: atomic by definition.
            5 => {
                if db.execute("BEGIN").is_err() {
                    return exp;
                }
                let keys: HashSet<i64> = (base..base + 2).collect();
                for k in base..base + 2 {
                    if db
                        .execute(&format!("INSERT INTO t VALUES ({k}, 'y')"))
                        .is_err()
                    {
                        // Crash before COMMIT: nothing may survive.
                        exp.must_absent.extend(keys);
                        return exp;
                    }
                }
                if db.execute("COMMIT").is_ok() {
                    exp.must_present.extend(keys.iter().copied());
                } else {
                    // The commit itself was interrupted: all-or-nothing.
                    exp.unknown.extend(keys.iter().copied());
                    exp.atomic_unit = Some(keys);
                    return exp;
                }
            }
            // Delete a key committed two rounds ago.
            6 => {
                let target = (i - 2) * 100;
                let was_present = exp.must_present.contains(&target);
                if db
                    .execute(&format!("DELETE FROM t WHERE id = {target}"))
                    .is_ok()
                {
                    if was_present {
                        exp.must_present.remove(&target);
                        exp.must_absent.insert(target);
                    }
                } else {
                    if was_present {
                        exp.must_present.remove(&target);
                        exp.unknown.insert(target); // delete may or may not have landed
                    }
                    return exp;
                }
            }
            // Plain auto-committed single inserts.
            _ => {
                if db
                    .execute(&format!("INSERT INTO t VALUES ({base}, 'z')"))
                    .is_ok()
                {
                    exp.must_present.insert(base);
                } else {
                    exp.unknown.insert(base);
                    return exp;
                }
            }
        }
    }
    exp
}

fn found_keys(db: &mut Database) -> HashSet<i64> {
    match db.execute("SELECT id FROM t") {
        Ok(r) => r
            .rows
            .iter()
            .map(|row| match row[0] {
                Value::Int(k) => k,
                ref v => panic!("non-integer id {v:?}"),
            })
            .collect(),
        // Table may legitimately not exist if we crashed during CREATE.
        Err(_) => HashSet::new(),
    }
}

fn verify(disk: &SimDisk, exp: &Expectation, label: &str) {
    let mut db = open_db(disk).unwrap_or_else(|e| panic!("{label}: recovery failed: {e}"));
    let found = found_keys(&mut db);

    for k in &exp.must_present {
        assert!(
            found.contains(k),
            "{label}: durability violated — committed key {k} is GONE"
        );
    }
    for k in &exp.must_absent {
        assert!(
            !found.contains(k),
            "{label}: atomicity violated — rolled-back/deleted key {k} EXISTS"
        );
    }
    for k in &found {
        assert!(
            exp.must_present.contains(k) || exp.unknown.contains(k),
            "{label}: phantom key {k} appeared from nowhere"
        );
    }
    if let Some(unit) = &exp.atomic_unit {
        let present: HashSet<i64> = unit.intersection(&found).copied().collect();
        assert!(
            present.is_empty() || present == *unit,
            "{label}: TORN transaction — only {present:?} of {unit:?} survived"
        );
    }

    // Recovery must be idempotent…
    drop(db);
    let mut db = open_db(disk).unwrap_or_else(|e| panic!("{label}: second open failed: {e}"));
    assert_eq!(
        found_keys(&mut db),
        found,
        "{label}: reopening changed the data"
    );

    // …and the database must still be fully usable.
    if db.execute("SELECT 1 FROM t LIMIT 1").is_ok() {
        db.execute("INSERT INTO t VALUES (999999, 'post-recovery write')")
            .unwrap_or_else(|e| panic!("{label}: post-recovery insert failed: {e}"));
        let r = db.execute("SELECT v FROM t WHERE id = 999999").unwrap();
        assert_eq!(r.rows.len(), 1, "{label}: post-recovery row not found");
    }
}

#[test]
fn crash_free_workload_baseline() {
    let disk = SimDisk::new(0xBEEF);
    let exp = run_workload(&disk);
    assert!(!exp.must_present.is_empty(), "workload committed nothing?");
    assert!(exp.unknown.is_empty(), "no crash was scheduled");
    verify(&disk, &exp, "baseline");
}

#[test]
fn committed_transactions_survive_a_crash_at_every_disk_operation() {
    // How many disk ops does the full workload perform?
    let probe = SimDisk::new(0xBEEF);
    let _ = run_workload(&probe);
    let total_ops = probe.ops_performed();
    assert!(total_ops > 100, "workload too small to be interesting");

    for seed in [11u64, 22, 33] {
        for crash_at in 0..total_ops + 5 {
            let disk = SimDisk::new(seed.wrapping_mul(1_000_003).wrapping_add(crash_at));
            disk.set_crash_after(crash_at);
            let exp = run_workload(&disk);
            disk.restart();
            verify(
                &disk,
                &exp,
                &format!("seed {seed}, crash after op {crash_at}"),
            );
        }
    }
}
