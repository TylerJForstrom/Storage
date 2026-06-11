//! End-to-end SQL tests. These double as documentation: each test shows a
//! behavior the engine promises, including the ones where it promises to
//! refuse loudly instead of guessing.

use glassdb::storage::SimDisk;
use glassdb::{Database, Value};

fn db() -> Database {
    Database::open_memory().unwrap()
}

fn ints(r: &glassdb::QueryResult, col: usize) -> Vec<i64> {
    r.rows
        .iter()
        .map(|row| match row[col] {
            Value::Int(i) => i,
            ref v => panic!("expected Int, got {v:?}"),
        })
        .collect()
}

#[test]
fn create_insert_select_roundtrip() {
    let mut db = db();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)")
        .unwrap();
    db.execute("INSERT INTO users VALUES (1, 'Ada', 36), (2, 'Grace', 45)")
        .unwrap();
    let r = db
        .execute("SELECT name, age FROM users ORDER BY id")
        .unwrap();
    assert_eq!(r.columns, vec!["name", "age"]);
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.rows[0][0], Value::Text("Ada".into()));
    assert_eq!(r.rows[1][1], Value::Int(45));
}

#[test]
fn pk_lookup_reads_one_row_full_scan_reads_all() {
    let mut db = db();
    db.seed_demo().unwrap(); // 400 rows

    // WHERE id = N goes through the B+tree: exactly one row is touched.
    let r = db.execute("SELECT * FROM users WHERE id = 250").unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.stats.rows_scanned, 1, "pk lookup must not scan the table");

    // A non-indexed predicate has to look at everything.
    let r = db.execute("SELECT * FROM users WHERE age = 30").unwrap();
    assert_eq!(r.stats.rows_scanned, 400, "non-pk predicate must full-scan");

    // And a pk range only walks the matching slice of the leaf chain.
    let r = db.execute("SELECT * FROM users WHERE id > 390").unwrap();
    assert_eq!(r.rows.len(), 10);
    assert!(
        r.stats.rows_scanned <= 11,
        "range scan touched {} rows, expected ~10",
        r.stats.rows_scanned
    );
}

#[test]
fn explain_names_the_access_path() {
    let mut db = db();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)")
        .unwrap();
    let r = db.execute("EXPLAIN SELECT * FROM t WHERE id = 7").unwrap();
    let text = r.plan.unwrap();
    assert!(text.contains("PRIMARY KEY LOOKUP"), "got plan: {text}");

    let r = db.execute("EXPLAIN SELECT * FROM t WHERE x = 7").unwrap();
    assert!(r.plan.unwrap().contains("FULL SCAN"));

    let r = db
        .execute("EXPLAIN SELECT * FROM t WHERE id > 10 AND id < 5")
        .unwrap();
    assert!(r.plan.unwrap().contains("NO ROWS"));
}

#[test]
fn limit_stops_the_scan_early() {
    let mut db = db();
    db.seed_demo().unwrap();
    let r = db.execute("SELECT * FROM users LIMIT 5").unwrap();
    assert_eq!(r.rows.len(), 5);
    assert_eq!(
        r.stats.rows_scanned, 5,
        "LIMIT without ORDER BY must stop early"
    );
}

#[test]
fn order_by_and_aggregates() {
    let mut db = db();
    db.execute("CREATE TABLE s (id INTEGER PRIMARY KEY, v INTEGER)")
        .unwrap();
    db.execute("INSERT INTO s (v) VALUES (30), (10), (20), (NULL)")
        .unwrap();

    let r = db
        .execute("SELECT v FROM s ORDER BY v DESC LIMIT 2")
        .unwrap();
    assert_eq!(ints(&r, 0), vec![30, 20]);

    let r = db
        .execute("SELECT COUNT(*), COUNT(v), SUM(v), AVG(v), MIN(v), MAX(v) FROM s")
        .unwrap();
    assert_eq!(r.rows[0][0], Value::Int(4)); // COUNT(*) counts rows
    assert_eq!(r.rows[0][1], Value::Int(3)); // COUNT(v) skips NULL
    assert_eq!(r.rows[0][2], Value::Int(60));
    assert_eq!(r.rows[0][3], Value::Real(20.0));
    assert_eq!(r.rows[0][4], Value::Int(10));
    assert_eq!(r.rows[0][5], Value::Int(30));
}

#[test]
fn update_and_delete_with_planner() {
    let mut db = db();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')")
        .unwrap();

    let r = db.execute("UPDATE t SET v = 'B!' WHERE id = 2").unwrap();
    assert_eq!(r.message.as_deref(), Some("1 row(s) updated"));
    let r = db.execute("SELECT v FROM t WHERE id = 2").unwrap();
    assert_eq!(r.rows[0][0], Value::Text("B!".into()));

    db.execute("DELETE FROM t WHERE id < 3").unwrap();
    let r = db.execute("SELECT id FROM t").unwrap();
    assert_eq!(ints(&r, 0), vec![3]);
}

#[test]
fn auto_assigned_primary_keys() {
    let mut db = db();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
        .unwrap();
    db.execute("INSERT INTO t (v) VALUES ('a'), ('b')").unwrap(); // omitted pk
    db.execute("INSERT INTO t VALUES (10, 'c')").unwrap(); // explicit pk
    db.execute("INSERT INTO t (v) VALUES ('d')").unwrap(); // continues after 10
    let r = db.execute("SELECT id FROM t ORDER BY id").unwrap();
    assert_eq!(ints(&r, 0), vec![1, 2, 10, 11]);
}

#[test]
fn duplicate_primary_key_is_refused() {
    let mut db = db();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
    let err = db.execute("INSERT INTO t VALUES (1, 'again')").unwrap_err();
    assert!(err.message.contains("UNIQUE constraint failed"), "{err}");
    // And the failed statement left nothing behind.
    let r = db.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.rows[0][0], Value::Int(1));
}

#[test]
fn type_errors_refuse_to_guess() {
    let mut db = db();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, age INTEGER)")
        .unwrap();
    let err = db
        .execute("INSERT INTO t VALUES (1, 'twenty')")
        .unwrap_err();
    assert!(err.message.contains("age"), "{err}");
    assert!(err.message.contains("INTEGER"), "{err}");
}

#[test]
fn syntax_errors_point_at_the_spot() {
    let mut db = db();
    let err = db.execute("SELEC * FROM t").unwrap_err();
    assert_eq!(err.position, Some(0));
    let err = db.execute("SELECT * FROM t WHERE").unwrap_err();
    assert!(err.position.is_some());
    assert!(err.message.contains("expected an expression"), "{err}");
}

#[test]
fn transactions_commit_and_roll_back() {
    let mut db = db();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
        .unwrap();

    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO t VALUES (1, 'kept')").unwrap();
    db.execute("COMMIT").unwrap();

    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO t VALUES (2, 'discarded')").unwrap();
    db.execute("ROLLBACK").unwrap();

    let r = db.execute("SELECT id FROM t").unwrap();
    assert_eq!(ints(&r, 0), vec![1]);
}

#[test]
fn data_survives_a_clean_reopen() {
    let disk = SimDisk::new(7);
    {
        let mut db =
            Database::open_with(Box::new(disk.open("db")), Box::new(disk.open("wal"))).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .unwrap();
        db.execute("INSERT INTO t VALUES (42, 'persisted')")
            .unwrap();
    } // drop = clean close
    let mut db =
        Database::open_with(Box::new(disk.open("db")), Box::new(disk.open("wal"))).unwrap();
    let r = db.execute("SELECT v FROM t WHERE id = 42").unwrap();
    assert_eq!(r.rows[0][0], Value::Text("persisted".into()));
}

#[test]
fn hidden_rowid_tables_work() {
    let mut db = db();
    db.execute("CREATE TABLE log (msg TEXT)").unwrap(); // no PRIMARY KEY at all
    db.execute("INSERT INTO log VALUES ('first'), ('second')")
        .unwrap();
    let r = db.execute("SELECT msg FROM log").unwrap();
    assert_eq!(r.rows.len(), 2);
}

#[test]
fn drop_table_frees_and_forgets() {
    let mut db = db();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1)").unwrap();
    db.execute("DROP TABLE t").unwrap();
    assert!(db.execute("SELECT * FROM t").is_err());
    // Recreating reuses the name without complaint.
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)")
        .unwrap();
    let r = db.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.rows[0][0], Value::Int(0));
}

#[test]
fn expressions_in_select_and_where() {
    let mut db = db();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, price REAL, qty INTEGER)")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 9.5, 3), (2, 2.0, 10)")
        .unwrap();
    let r = db
        .execute("SELECT id, price * qty AS total FROM t WHERE price * qty > 20 ")
        .unwrap();
    assert_eq!(r.columns, vec!["id", "total"]);
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][1], Value::Real(28.5));
}

#[test]
fn errors_abort_the_open_transaction() {
    let mut db = db();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)")
        .unwrap();
    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO t VALUES (1)").unwrap();
    let _ = db.execute("INSERT INTO t VALUES (1)").unwrap_err(); // duplicate
                                                                 // The whole transaction is gone, and we're back in auto-commit mode.
    let r = db.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.rows[0][0], Value::Int(0));
    assert!(
        db.execute("COMMIT").is_err(),
        "transaction should be closed"
    );
}
