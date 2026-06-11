# GlassDB

**A SQL database you can see through.**

A tiny, crash-safe SQL database engine written from scratch in Rust — pager,
write-ahead log, B+tree, SQL parser, and query planner — with a glass wall in
front of it: every page read, every WAL frame, every fsync is observable. The
web demo runs the *real engine* compiled to WebAssembly and animates what it
does while your query executes.

- **Zero dependencies** in the core crate. The B+tree, the WAL, CRC32, the
  JSON encoder, even the PRNG the tests use — all written here.
- **`#![forbid(unsafe_code)]`** — the compiler proves there's no unsafe.
- **Crash-tested at every disk operation** (more below — this is the part
  I'm proudest of).

```
$ glassdb
glassdb> .seed
seeded table 'users' with 400 rows

glassdb> SELECT name, age, city FROM users WHERE id = 250;
name         age  city
-----------  ---  --------
Kira Brooks   20  New York
(1 row)
· 2 page reads (2 cached) · 1 rows scanned · 90 µs

glassdb> EXPLAIN SELECT * FROM users WHERE id = 250;
SELECT on 'users'
  access: PRIMARY KEY LOOKUP on users (id = 250) — one descent, O(log n)
  filter: id = 250 (re-checked on every row the access path yields)
```

Same data, no index help: `WHERE age = 30` reads all 13 pages and scans 400
rows. The stats line makes the planner's decision *visible* — that's the
whole point of the project.

## Architecture

```
                 SQL text
                    │
   ┌────────────────▼─────────────────┐
   │  lexer → parser → AST            │  sql/        position-carrying errors
   ├──────────────────────────────────┤
   │  planner                         │  planner.rs  pk lookup / range / scan
   ├──────────────────────────────────┤
   │  executor                        │  db.rs       transactions, aggregates
   ├────────────┬─────────────────────┤
   │  B+tree    │  catalog (a B+tree) │  btree.rs    splits, leaf-chain scans
   ├────────────┴─────────────────────┤
   │  pager: 4 KiB pages, buffer pool │  pager.rs    LRU eviction, freelist
   ├──────────────────────────────────┤
   │  write-ahead log                 │  wal.rs      CRC frames, recovery
   ├──────────────────────────────────┤
   │  Storage trait                   │  storage.rs  file / memory / SimDisk
   └──────────────────────────────────┘
          every layer emits trace events → trace.rs → CLI & visualizer
```

### How a commit works (and why a crash can't corrupt it)

1. Modified pages collect in the buffer pool — the database file is untouched.
2. On commit, every dirty page's image is appended to the WAL, then a commit
   record, then **fsync**. *This* is the durability point.
3. Only then are the pages written into the database file.
4. Periodically a checkpoint syncs the database file and resets the WAL.

If power dies at any moment, the next open replays every WAL frame that has a
valid CRC32 *and* a commit record after it, and ignores the torn tail.
Committed transactions always survive; uncommitted ones vanish completely.

## How it's tested

**Differential fuzzing** (`tests/btree_fuzz.rs`) — thousands of random
inserts, deletes, gets and range scans across many seeds, with Rust's
`BTreeMap` as the oracle. Any divergence is a bug; every failure replays
deterministically from its seed.

**Crash injection at every disk operation** (`tests/crash_recovery.rs`) —
the same idea FoundationDB and TigerBeetle made famous, in miniature. A
simulated disk (`SimDisk`) cuts power after exactly N operations, and the
suite runs the full workload for *every* N. The crash model is deliberately
nasty: unsynced writes may individually survive, vanish, or tear mid-write,
in any combination — which is what real hardware is allowed to do. After
every crash the database is reopened and the ACID contract is asserted:

1. every acknowledged transaction is fully present (durability)
2. nothing from unfinished transactions remains (atomicity)
3. the in-flight commit appears completely or not at all
4. recovery is idempotent, and the engine accepts new writes afterwards

**End-to-end SQL tests** (`tests/sql_end_to_end.rs`) — behavior as
documentation, including proof-by-stats that the planner works
(`WHERE id = …` scans 1 row; `WHERE age = …` scans 400).

```
cargo test --workspace     # all of the above, ~5s
```

## Try it

```
# REPL on a real file (crash-safe — kill it mid-write and reopen)
cargo run -p glassdb-cli -- mydata.db

# throwaway in-memory session
cargo run -p glassdb-cli -- :memory:
```

REPL extras: `.seed`, `.tables`, `.schema <t>`, `.trace on` (log every page
read / WAL write per statement), `.tree <t>` (dump the B+tree shape).

### The browser visualizer

```
wasm-pack build crates/glassdb-wasm --target web --release --out-dir ../../web/pkg
cd web && python -m http.server 8092
```

Open http://localhost:8092 — the engine boots in WASM, seeds itself, and the
right-hand panels animate page I/O, the WAL, and the B+tree (pages visited by
your last query glow).

### As a library

```rust
use glassdb::Database;

let mut db = Database::open_file("app.db")?;
db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")?;
db.execute("INSERT INTO t (name) VALUES ('hello')")?;
let r = db.execute("SELECT * FROM t WHERE id = 1")?;
println!("{:?} — {} pages read", r.rows, r.stats.pages_read);
```

## SQL supported

`CREATE TABLE` (INTEGER/REAL/TEXT/BOOLEAN, one `INTEGER PRIMARY KEY`),
`DROP TABLE`, multi-row `INSERT` (omitted pk auto-assigns),
`SELECT` (expressions, `WHERE`, `ORDER BY`, `LIMIT`, aliases,
`COUNT/SUM/AVG/MIN/MAX`), `UPDATE`, `DELETE`,
`BEGIN`/`COMMIT`/`ROLLBACK`, `EXPLAIN`.

## Honest limitations (v1)

Deliberate scope cuts, not oversights:

- one connection, one writer — no concurrency control yet
- the only index is the primary key; no secondary indexes
- no JOIN or GROUP BY yet
- rows are capped at 1300 bytes (no overflow pages); the cap is chosen so a
  node split provably always fits
- deletes are lazy: B+tree nodes are never merged (space is reclaimed on
  `DROP TABLE`)

Roadmap, roughly in order: secondary indexes (`CREATE INDEX`), GROUP BY,
JOIN (nested-loop first), overflow pages, MVCC snapshots.

## License

MIT
