//! The Database: ties the pager, WAL, B+trees, catalog, planner, and
//! executor together behind one method: `execute(sql)`.
//!
//! Transaction model: auto-commit per statement, or explicit
//! BEGIN ... COMMIT / ROLLBACK. Any error aborts the open transaction
//! (the simplest rule that can never leave half a statement behind).

use std::collections::HashMap;
use std::ops::Bound;

use crate::btree::BTree;
use crate::catalog;
use crate::errors::{DbError, DbResult};
use crate::executor::{self, AggKind, AggState};
use crate::json::J;
use crate::pager::Pager;
use crate::planner::{self, Access, Plan};
use crate::rng::Rng;
use crate::sql::ast::{Expr, SelectExpr, SelectStmt, Statement};
use crate::sql::parser;
use crate::storage::{FileStorage, MemStorage, Storage};
use crate::trace::{new_shared, SharedTrace, Stats, TraceEvent};
use crate::types::{coerce, decode_row, encode_row, ColType, Column, TableSchema, Value};
use crate::wal::Wal;

#[derive(Debug)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub message: Option<String>,
    /// Human-readable plan (always present for SELECT/UPDATE/DELETE).
    pub plan: Option<String>,
    /// Machine-readable access path for the visualizer.
    pub plan_access: Option<J>,
    pub stats: Stats,
    pub trace: Vec<TraceEvent>,
    pub trace_dropped: u64,
}

impl QueryResult {
    fn message(msg: impl Into<String>) -> QueryResult {
        QueryResult {
            columns: Vec::new(),
            rows: Vec::new(),
            message: Some(msg.into()),
            plan: None,
            plan_access: None,
            stats: Stats::default(),
            trace: Vec::new(),
            trace_dropped: 0,
        }
    }

    pub fn to_json(&self) -> String {
        J::O(vec![
            (
                "columns".into(),
                J::A(self.columns.iter().map(|c| J::s(c.clone())).collect()),
            ),
            (
                "rows".into(),
                J::A(
                    self.rows
                        .iter()
                        .map(|r| J::A(r.iter().map(|v| v.to_json()).collect()))
                        .collect(),
                ),
            ),
            (
                "message".into(),
                self.message.clone().map(J::S).unwrap_or(J::Null),
            ),
            (
                "plan".into(),
                self.plan.clone().map(J::S).unwrap_or(J::Null),
            ),
            (
                "plan_access".into(),
                self.plan_access.clone().unwrap_or(J::Null),
            ),
            ("stats".into(), self.stats.to_json()),
            (
                "trace".into(),
                J::A(self.trace.iter().map(|e| e.to_json()).collect()),
            ),
            ("trace_dropped".into(), J::I(self.trace_dropped as i64)),
        ])
        .render()
    }
}

struct Timer {
    #[cfg(not(target_arch = "wasm32"))]
    start: std::time::Instant,
}

impl Timer {
    fn start() -> Timer {
        Timer {
            #[cfg(not(target_arch = "wasm32"))]
            start: std::time::Instant::now(),
        }
    }

    fn micros(&self) -> u64 {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.start.elapsed().as_micros() as u64
        }
        #[cfg(target_arch = "wasm32")]
        {
            0
        }
    }
}

pub struct Database {
    pager: Pager,
    wal: Wal,
    trace: SharedTrace,
    /// Lowercased table name -> schema.
    tables: HashMap<String, TableSchema>,
    /// Table id -> next auto-assigned key (lazy, rebuilt after rollback).
    next_key: HashMap<i64, i64>,
    in_txn: bool,
    /// Set when crash recovery replayed something at open: (frames, txns).
    pub recovered: Option<(u64, u64)>,
}

impl Database {
    /// Open against any pair of storages (database file, WAL file).
    /// This is the seam the crash tests use to inject a simulated disk.
    pub fn open_with(
        mut db_storage: Box<dyn Storage>,
        wal_storage: Box<dyn Storage>,
    ) -> DbResult<Database> {
        let trace = new_shared();
        let mut wal = Wal::open(wal_storage, trace.clone())?;
        // Recovery first: if a crash tore the header mid-commit, the WAL
        // holds the good copy and repairs it before we look at anything.
        let (frames, txns) = wal.recover(&mut *db_storage)?;
        if !Pager::header_valid(&mut *db_storage) {
            // Brand-new file, or a bootstrap that lost power partway. Safe
            // to (re)initialize only if nothing was ever committed: a real
            // database always has a valid header after recovery ran.
            if txns == 0 && db_storage.len() <= 2 * crate::pager::PAGE_SIZE as u64 {
                Pager::bootstrap(&mut *db_storage, &BTree::empty_leaf_bytes())?;
            }
            // Otherwise fall through and let Pager::open report corruption.
        }
        let mut pager = Pager::open(db_storage, trace.clone())?;
        let mut tables = HashMap::new();
        for schema in catalog::load_all(&mut pager)? {
            tables.insert(schema.name.to_lowercase(), schema);
        }
        Ok(Database {
            pager,
            wal,
            trace,
            tables,
            next_key: HashMap::new(),
            in_txn: false,
            recovered: if frames > 0 {
                Some((frames, txns))
            } else {
                None
            },
        })
    }

    /// Open (or create) a database file at `path`; the WAL lives next to it
    /// at `path.wal`.
    pub fn open_file(path: &str) -> DbResult<Database> {
        let db = FileStorage::open(path)?;
        let wal = FileStorage::open(format!("{path}.wal"))?;
        Self::open_with(Box::new(db), Box::new(wal))
    }

    /// Fully in-memory database (used by the browser demo and tests).
    pub fn open_memory() -> DbResult<Database> {
        Self::open_with(Box::new(MemStorage::new()), Box::new(MemStorage::new()))
    }

    // --- public API ---------------------------------------------------

    /// Execute exactly one SQL statement.
    pub fn execute(&mut self, sql: &str) -> DbResult<QueryResult> {
        let stmt = parser::parse_one(sql)?;
        self.execute_stmt(&stmt)
    }

    /// Execute a script of ';'-separated statements, stopping at the first
    /// error. Returns one result per completed statement.
    pub fn execute_script(&mut self, sql: &str) -> DbResult<Vec<QueryResult>> {
        let stmts = parser::parse_statements(sql)?;
        let mut results = Vec::with_capacity(stmts.len());
        for stmt in &stmts {
            results.push(self.execute_stmt(stmt)?);
        }
        Ok(results)
    }

    pub fn table_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.tables.values().map(|t| t.name.clone()).collect();
        names.sort();
        names
    }

    pub fn schema_of(&self, table: &str) -> DbResult<&TableSchema> {
        self.tables.get(&table.to_lowercase()).ok_or_else(|| {
            DbError::schema(format!(
                "no table named '{table}' — create it with CREATE TABLE, or .seed for demo data"
            ))
        })
    }

    /// JSON snapshot of a table's B+tree shape, for the visualizer.
    pub fn btree_layout(&mut self, table: &str) -> DbResult<String> {
        let schema = self.schema_of(table)?.clone();
        let tree = BTree { root: schema.root };
        let layout = tree.layout(&mut self.pager, 80)?;
        Ok(J::O(vec![
            ("table".into(), J::s(schema.name.clone())),
            ("root".into(), J::I(schema.root as i64)),
            ("tree".into(), layout),
        ])
        .render())
    }

    /// All table schemas as JSON, for the visualizer's sidebar.
    pub fn schemas_json(&self) -> String {
        let mut tables: Vec<&TableSchema> = self.tables.values().collect();
        tables.sort_by(|a, b| a.name.cmp(&b.name));
        J::A(
            tables
                .iter()
                .map(|t| {
                    J::O(vec![
                        ("name".into(), J::s(t.name.clone())),
                        ("root".into(), J::I(t.root as i64)),
                        (
                            "columns".into(),
                            J::A(
                                t.columns
                                    .iter()
                                    .map(|c| {
                                        J::O(vec![
                                            ("name".into(), J::s(c.name.clone())),
                                            ("type".into(), J::s(c.ty.name())),
                                            ("pk".into(), J::B(c.primary_key)),
                                        ])
                                    })
                                    .collect(),
                            ),
                        ),
                    ])
                })
                .collect(),
        )
        .render()
    }

    /// Create a `users` table with 400 deterministic demo rows.
    pub fn seed_demo(&mut self) -> DbResult<String> {
        if self.tables.contains_key("users") {
            return Err(DbError::schema(
                "table 'users' already exists — DROP TABLE users first",
            ));
        }
        self.execute(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER, \
             city TEXT, score REAL)",
        )?;
        let first = [
            "Ava", "Ben", "Carmen", "Diego", "Elena", "Felix", "Grace", "Hiro", "Iris", "Jonas",
            "Kira", "Liam", "Maya", "Noor", "Owen", "Priya", "Quinn", "Rosa", "Sam", "Tara",
        ];
        let last = [
            "Lee", "Patel", "Garcia", "Kim", "Okafor", "Novak", "Silva", "Chen", "Ali", "Brooks",
        ];
        let cities = [
            "New York", "Ithaca", "Chicago", "Austin", "Seattle", "Boston", "Denver", "Miami",
        ];
        let mut rng = Rng::new(2026);
        self.execute("BEGIN")?;
        const TOTAL: usize = 400;
        const CHUNK: usize = 40;
        for chunk_start in (0..TOTAL).step_by(CHUNK) {
            let mut sql = String::from("INSERT INTO users (name, age, city, score) VALUES ");
            for i in 0..CHUNK {
                if i > 0 {
                    sql.push_str(", ");
                }
                let name = format!(
                    "{} {}",
                    first[rng.below(first.len() as u64) as usize],
                    last[rng.below(last.len() as u64) as usize]
                );
                let age = rng.range_i64(18, 79);
                let city = cities[rng.below(cities.len() as u64) as usize];
                let score = (rng.below(1000) as f64) / 10.0;
                sql.push_str(&format!("('{name}', {age}, '{city}', {score})"));
            }
            let _ = chunk_start;
            self.execute(&sql)?;
        }
        self.execute("COMMIT")?;
        Ok(format!(
            "seeded table 'users' with {TOTAL} rows — try: SELECT * FROM users WHERE id = 250, \
             then EXPLAIN it"
        ))
    }

    // --- statement execution -------------------------------------------

    fn execute_stmt(&mut self, stmt: &Statement) -> DbResult<QueryResult> {
        self.trace.borrow_mut().begin();
        let timer = Timer::start();
        match self.run_statement(stmt) {
            Ok(mut result) => {
                // Auto-commit unless we're inside an explicit transaction.
                if !self.in_txn && self.pager.has_dirty() {
                    if let Err(e) = self.commit_txn() {
                        self.abort();
                        return Err(e);
                    }
                }
                let mut t = self.trace.borrow_mut();
                t.stats.elapsed_micros = timer.micros();
                t.stats.rows_returned = result.rows.len() as u64;
                result.stats = t.stats.clone();
                result.trace = std::mem::take(&mut t.events);
                result.trace_dropped = t.dropped;
                Ok(result)
            }
            Err(e) => {
                self.abort();
                Err(e)
            }
        }
    }

    fn run_statement(&mut self, stmt: &Statement) -> DbResult<QueryResult> {
        match stmt {
            Statement::Select(s) => self.exec_select(s),
            Statement::Insert {
                table,
                columns,
                rows,
            } => self.exec_insert(table, columns.as_deref(), rows),
            Statement::Update {
                table,
                sets,
                where_clause,
            } => self.exec_update(table, sets, where_clause),
            Statement::Delete {
                table,
                where_clause,
            } => self.exec_delete(table, where_clause),
            Statement::CreateTable { name, columns } => self.exec_create_table(name, columns),
            Statement::DropTable { name } => self.exec_drop_table(name),
            Statement::Begin => {
                if self.in_txn {
                    return Err(DbError::schema("a transaction is already open"));
                }
                self.in_txn = true;
                Ok(QueryResult::message("transaction started"))
            }
            Statement::Commit => {
                if !self.in_txn {
                    return Err(DbError::schema("no transaction is open"));
                }
                self.commit_txn()?;
                self.in_txn = false;
                Ok(QueryResult::message("committed"))
            }
            Statement::Rollback => {
                if !self.in_txn {
                    return Err(DbError::schema("no transaction is open"));
                }
                self.rollback_state()?;
                self.in_txn = false;
                Ok(QueryResult::message("rolled back"))
            }
            Statement::Explain(inner) => self.exec_explain(inner),
        }
    }

    // --- commit / rollback machinery ------------------------------------

    /// The WAL commit protocol. See wal.rs for why this exact order.
    fn commit_txn(&mut self) -> DbResult<()> {
        if !self.pager.has_dirty() {
            return Ok(());
        }
        let pages = self.pager.commit_set();
        for (pid, data) in &pages {
            self.wal.append_page(*pid, data)?;
        }
        self.wal.append_commit()?;
        self.wal.sync()?; // <- durability point
        self.pager.apply_commit(&pages)?;
        if self.wal.should_checkpoint() {
            self.pager.sync_storage()?;
            self.wal.reset()?;
        }
        Ok(())
    }

    fn rollback_state(&mut self) -> DbResult<()> {
        self.pager.rollback()?;
        self.tables.clear();
        for schema in catalog::load_all(&mut self.pager)? {
            self.tables.insert(schema.name.to_lowercase(), schema);
        }
        self.next_key.clear();
        Ok(())
    }

    /// After any error: throw away uncommitted state, close the transaction.
    fn abort(&mut self) {
        let _ = self.rollback_state();
        self.in_txn = false;
    }

    // --- helpers ---------------------------------------------------------

    fn schema_cloned(&self, table: &str) -> DbResult<TableSchema> {
        Ok(self.schema_of(table)?.clone())
    }

    /// Persist a possibly-changed B+tree root for a table.
    fn update_root(&mut self, schema: &mut TableSchema, new_root: u32) -> DbResult<()> {
        if schema.root != new_root {
            schema.root = new_root;
            catalog::save(&mut self.pager, schema)?;
            self.tables
                .insert(schema.name.to_lowercase(), schema.clone());
        }
        Ok(())
    }

    /// Walk every row the plan's access path yields, apply the filter, and
    /// hand matches to `sink`. `early_limit` stops the scan once that many
    /// rows matched (only safe when no sort/aggregate runs afterwards).
    fn for_each_match(
        &mut self,
        schema: &TableSchema,
        plan: &Plan,
        early_limit: Option<u64>,
        sink: &mut dyn FnMut(i64, Vec<Value>) -> DbResult<()>,
    ) -> DbResult<()> {
        let tree = BTree { root: schema.root };
        let trace = self.trace.clone();
        let filter = plan.filter.clone();
        let schema = schema.clone();
        let mut matched = 0u64;
        let mut visit = |key: i64, bytes: &[u8]| -> DbResult<bool> {
            trace.borrow_mut().stats.rows_scanned += 1;
            let row = decode_row(bytes)?;
            if let Some(pred) = &filter {
                let keep = executor::eval(pred, &schema, &row)?;
                if !executor::is_true(&keep) {
                    return Ok(true);
                }
            }
            sink(key, row)?;
            matched += 1;
            if let Some(limit) = early_limit {
                if matched >= limit {
                    return Ok(false);
                }
            }
            Ok(true)
        };
        match &plan.access {
            Access::PkLookup(key) => {
                if let Some(bytes) = tree.get(&mut self.pager, *key)? {
                    visit(*key, &bytes)?;
                }
                Ok(())
            }
            Access::PkRange { lo, hi } => tree.scan(&mut self.pager, *lo, *hi, &mut visit),
            Access::FullScan => tree.scan(
                &mut self.pager,
                Bound::Unbounded,
                Bound::Unbounded,
                &mut visit,
            ),
            Access::Nothing => Ok(()),
        }
    }

    /// Next auto-assign key for a table (max existing key + 1, cached).
    fn load_next_key(&mut self, schema: &TableSchema) -> DbResult<i64> {
        if let Some(&n) = self.next_key.get(&schema.id) {
            return Ok(n);
        }
        let tree = BTree { root: schema.root };
        let n = tree.max_key(&mut self.pager)?.map(|k| k + 1).unwrap_or(1);
        Ok(n)
    }

    // --- SELECT -----------------------------------------------------------

    fn exec_select(&mut self, stmt: &SelectStmt) -> DbResult<QueryResult> {
        let schema = self.schema_cloned(&stmt.table)?;
        let plan = planner::plan(&schema, &stmt.where_clause);

        // Aggregate or plain projection?
        let mut has_agg = false;
        let mut has_plain = false;
        for item in &stmt.items {
            match &item.expr {
                SelectExpr::Star => has_plain = true,
                SelectExpr::Expr(Expr::Call { .. }) => has_agg = true,
                SelectExpr::Expr(_) => has_plain = true,
            }
        }
        if has_agg && has_plain {
            return Err(DbError::unsupported(
                "mixing aggregates with plain columns needs GROUP BY, which this \
                 engine doesn't have (yet)",
            ));
        }

        let plan_text = self.render_plan_text(&schema, &plan, Some(stmt), "SELECT");
        let plan_access = Some(planner::access_to_json(&plan));

        if has_agg {
            if stmt.order_by.is_some() {
                return Err(DbError::unsupported(
                    "ORDER BY with aggregates isn't meaningful here (one row comes back)",
                ));
            }
            return self.exec_select_aggregate(stmt, &schema, &plan, plan_text, plan_access);
        }

        // Does the scan order already satisfy ORDER BY?
        let needs_sort = match &stmt.order_by {
            None => false,
            Some((col, asc)) => {
                let idx = schema.col_index(col).ok_or_else(|| {
                    DbError::schema(format!(
                        "ORDER BY column '{col}' doesn't exist in '{}'",
                        schema.name
                    ))
                })?;
                // The B+tree scans in primary-key order, so ORDER BY pk ASC
                // is free.
                !(schema.pk_index() == Some(idx) && *asc)
            }
        };

        let early_limit = if needs_sort { None } else { stmt.limit };
        let mut matches: Vec<Vec<Value>> = Vec::new();
        self.for_each_match(&schema, &plan, early_limit, &mut |_key, row| {
            matches.push(row);
            Ok(())
        })?;

        if needs_sort {
            let (col, asc) = stmt.order_by.clone().unwrap();
            let idx = schema.col_index(&col).unwrap();
            matches.sort_by(|a, b| executor::compare_for_sort(&a[idx], &b[idx]));
            if !asc {
                matches.reverse();
            }
        }
        if let Some(limit) = stmt.limit {
            matches.truncate(limit as usize);
        }

        // Projection.
        let mut columns: Vec<String> = Vec::new();
        for item in &stmt.items {
            match &item.expr {
                SelectExpr::Star => columns.extend(schema.columns.iter().map(|c| c.name.clone())),
                SelectExpr::Expr(e) => {
                    columns.push(item.alias.clone().unwrap_or_else(|| e.display()))
                }
            }
        }
        let mut rows: Vec<Vec<Value>> = Vec::with_capacity(matches.len());
        for row in &matches {
            let mut out = Vec::with_capacity(columns.len());
            for item in &stmt.items {
                match &item.expr {
                    SelectExpr::Star => out.extend(row.iter().cloned()),
                    SelectExpr::Expr(e) => out.push(executor::eval(e, &schema, row)?),
                }
            }
            rows.push(out);
        }

        Ok(QueryResult {
            columns,
            rows,
            message: None,
            plan: Some(plan_text),
            plan_access,
            stats: Stats::default(),
            trace: Vec::new(),
            trace_dropped: 0,
        })
    }

    fn exec_select_aggregate(
        &mut self,
        stmt: &SelectStmt,
        schema: &TableSchema,
        plan: &Plan,
        plan_text: String,
        plan_access: Option<J>,
    ) -> DbResult<QueryResult> {
        // Build one accumulator per SELECT item.
        let mut specs: Vec<(AggState, Option<Expr>)> = Vec::new();
        let mut columns = Vec::new();
        for item in &stmt.items {
            let SelectExpr::Expr(Expr::Call { func, arg, star }) = &item.expr else {
                unreachable!("checked by caller");
            };
            let kind = AggKind::from_call(func, arg.is_some(), *star)?;
            specs.push((AggState::new(kind), arg.as_deref().cloned()));
            columns.push(item.alias.clone().unwrap_or_else(|| {
                Expr::Call {
                    func: func.clone(),
                    arg: arg.clone(),
                    star: *star,
                }
                .display()
            }));
        }

        let schema2 = schema.clone();
        let mut agg_err: Option<DbError> = None;
        self.for_each_match(schema, plan, None, &mut |_key, row| {
            for (state, arg) in specs.iter_mut() {
                let value = match arg {
                    Some(expr) => executor::eval(expr, &schema2, &row)?,
                    None => Value::Null, // COUNT(*) ignores it
                };
                if let Err(e) = state.update(&value) {
                    agg_err = Some(e);
                    return Err(agg_err.clone().unwrap());
                }
            }
            Ok(())
        })?;

        let row: Vec<Value> = specs.into_iter().map(|(s, _)| s.finish()).collect();
        Ok(QueryResult {
            columns,
            rows: vec![row],
            message: None,
            plan: Some(plan_text),
            plan_access,
            stats: Stats::default(),
            trace: Vec::new(),
            trace_dropped: 0,
        })
    }

    // --- INSERT -----------------------------------------------------------

    fn exec_insert(
        &mut self,
        table: &str,
        columns: Option<&[String]>,
        rows: &[Vec<Expr>],
    ) -> DbResult<QueryResult> {
        let mut schema = self.schema_cloned(table)?;

        // Which schema slot does each provided value land in?
        let targets: Vec<usize> = match columns {
            Some(cols) => {
                let mut seen = Vec::new();
                let mut targets = Vec::with_capacity(cols.len());
                for col in cols {
                    let idx = schema.col_index(col).ok_or_else(|| {
                        DbError::schema(format!("no column '{col}' in table '{}'", schema.name))
                    })?;
                    if seen.contains(&idx) {
                        return Err(DbError::schema(format!(
                            "column '{col}' listed twice in INSERT"
                        )));
                    }
                    seen.push(idx);
                    targets.push(idx);
                }
                targets
            }
            None => (0..schema.columns.len()).collect(),
        };

        let mut tree = BTree { root: schema.root };
        let mut next_key = self.load_next_key(&schema)?;
        let pk_idx = schema.pk_index();
        let mut inserted = 0u64;

        for row_exprs in rows {
            if row_exprs.len() != targets.len() {
                return Err(DbError::type_error(format!(
                    "row {} has {} value(s) but {} column(s) are expected",
                    inserted + 1,
                    row_exprs.len(),
                    targets.len()
                )));
            }
            let mut row: Vec<Value> = vec![Value::Null; schema.columns.len()];
            for (&slot, expr) in targets.iter().zip(row_exprs) {
                let value = executor::eval_const(expr)?;
                let col = &schema.columns[slot];
                row[slot] = coerce(value, col.ty, &col.name)?;
            }

            // Decide this row's B+tree key.
            let key = match pk_idx {
                Some(i) => match row[i] {
                    Value::Int(k) => {
                        next_key = next_key.max(k + 1);
                        k
                    }
                    Value::Null => {
                        // Omitted/NULL INTEGER PRIMARY KEY auto-assigns,
                        // like SQLite.
                        let k = next_key;
                        next_key += 1;
                        row[i] = Value::Int(k);
                        k
                    }
                    _ => unreachable!("coerce() guarantees Int or Null here"),
                },
                None => {
                    // Hidden rowid table.
                    let k = next_key;
                    next_key += 1;
                    k
                }
            };

            if tree.get(&mut self.pager, key)?.is_some() {
                let pk_name = pk_idx
                    .map(|i| schema.columns[i].name.clone())
                    .unwrap_or_else(|| "rowid".into());
                return Err(DbError::schema(format!(
                    "UNIQUE constraint failed: {}.{pk_name} = {key} already exists",
                    schema.name
                )));
            }
            tree.insert(&mut self.pager, key, &encode_row(&row))?;
            inserted += 1;
        }

        self.update_root(&mut schema, tree.root)?;
        self.next_key.insert(schema.id, next_key);
        Ok(QueryResult::message(format!("{inserted} row(s) inserted")))
    }

    // --- UPDATE -----------------------------------------------------------

    fn exec_update(
        &mut self,
        table: &str,
        sets: &[(String, Expr)],
        where_clause: &Option<Expr>,
    ) -> DbResult<QueryResult> {
        let mut schema = self.schema_cloned(table)?;
        let set_slots: Vec<(usize, &Expr)> = sets
            .iter()
            .map(|(col, expr)| {
                schema.col_index(col).map(|i| (i, expr)).ok_or_else(|| {
                    DbError::schema(format!("no column '{col}' in table '{}'", schema.name))
                })
            })
            .collect::<DbResult<_>>()?;

        let plan = planner::plan(&schema, where_clause);
        let mut matches: Vec<(i64, Vec<Value>)> = Vec::new();
        self.for_each_match(&schema, &plan, None, &mut |key, row| {
            matches.push((key, row));
            Ok(())
        })?;

        let mut tree = BTree { root: schema.root };
        let pk_idx = schema.pk_index();
        let mut updated = 0u64;
        for (key, row) in &matches {
            let mut new_row = row.clone();
            for (slot, expr) in &set_slots {
                let value = executor::eval(expr, &schema, row)?;
                let col = &schema.columns[*slot];
                new_row[*slot] = coerce(value, col.ty, &col.name)?;
            }
            let new_key = match pk_idx {
                Some(i) => match new_row[i] {
                    Value::Int(k) => k,
                    Value::Null => return Err(DbError::schema("PRIMARY KEY can't be set to NULL")),
                    _ => unreachable!("coerce() guarantees Int or Null"),
                },
                None => *key,
            };
            if new_key != *key {
                if tree.get(&mut self.pager, new_key)?.is_some() {
                    return Err(DbError::schema(format!(
                        "UNIQUE constraint failed: changing the key to {new_key} would collide"
                    )));
                }
                tree.delete(&mut self.pager, *key)?;
                tree.insert(&mut self.pager, new_key, &encode_row(&new_row))?;
                self.next_key
                    .entry(schema.id)
                    .and_modify(|n| *n = (*n).max(new_key + 1));
            } else {
                tree.insert(&mut self.pager, *key, &encode_row(&new_row))?;
            }
            updated += 1;
        }
        self.update_root(&mut schema, tree.root)?;
        Ok(QueryResult::message(format!("{updated} row(s) updated")))
    }

    // --- DELETE -----------------------------------------------------------

    fn exec_delete(&mut self, table: &str, where_clause: &Option<Expr>) -> DbResult<QueryResult> {
        let mut schema = self.schema_cloned(table)?;
        let plan = planner::plan(&schema, where_clause);
        let mut keys: Vec<i64> = Vec::new();
        self.for_each_match(&schema, &plan, None, &mut |key, _row| {
            keys.push(key);
            Ok(())
        })?;
        let mut tree = BTree { root: schema.root };
        for key in &keys {
            tree.delete(&mut self.pager, *key)?;
        }
        self.update_root(&mut schema, tree.root)?;
        Ok(QueryResult::message(format!(
            "{} row(s) deleted",
            keys.len()
        )))
    }

    // --- DDL ---------------------------------------------------------------

    fn exec_create_table(
        &mut self,
        name: &str,
        column_defs: &[crate::sql::ast::ColumnDef],
    ) -> DbResult<QueryResult> {
        if self.tables.contains_key(&name.to_lowercase()) {
            return Err(DbError::schema(format!("table '{name}' already exists")));
        }
        let mut pk_count = 0;
        let mut columns = Vec::with_capacity(column_defs.len());
        for (i, def) in column_defs.iter().enumerate() {
            for other in &column_defs[..i] {
                if other.name.eq_ignore_ascii_case(&def.name) {
                    return Err(DbError::schema(format!(
                        "duplicate column name '{}'",
                        def.name
                    )));
                }
            }
            if def.primary_key {
                pk_count += 1;
                if def.ty != ColType::Int {
                    return Err(DbError::schema(format!(
                        "PRIMARY KEY column '{}' must be INTEGER — it becomes the \
                         B+tree key",
                        def.name
                    )));
                }
            }
            columns.push(Column {
                name: def.name.clone(),
                ty: def.ty,
                primary_key: def.primary_key,
            });
        }
        if pk_count > 1 {
            return Err(DbError::schema("only one PRIMARY KEY column is allowed"));
        }

        let id = self.tables.values().map(|t| t.id).max().unwrap_or(0) + 1;
        let tree = BTree::create(&mut self.pager)?;
        let schema = TableSchema {
            id,
            name: name.to_string(),
            root: tree.root,
            columns,
        };
        catalog::save(&mut self.pager, &schema)?;
        self.tables.insert(schema.name.to_lowercase(), schema);
        Ok(QueryResult::message(format!("table '{name}' created")))
    }

    fn exec_drop_table(&mut self, name: &str) -> DbResult<QueryResult> {
        let schema = self.schema_cloned(name)?;
        let tree = BTree { root: schema.root };
        tree.free_all(&mut self.pager)?;
        catalog::remove(&mut self.pager, schema.id)?;
        self.tables.remove(&name.to_lowercase());
        self.next_key.remove(&schema.id);
        Ok(QueryResult::message(format!("table '{name}' dropped")))
    }

    // --- EXPLAIN ------------------------------------------------------------

    fn exec_explain(&mut self, inner: &Statement) -> DbResult<QueryResult> {
        let (table, where_clause, what) = match inner {
            Statement::Select(s) => (&s.table, &s.where_clause, "SELECT"),
            Statement::Update {
                table,
                where_clause,
                ..
            } => (table, where_clause, "UPDATE"),
            Statement::Delete {
                table,
                where_clause,
            } => (table, where_clause, "DELETE"),
            _ => {
                return Err(DbError::unsupported(
                    "EXPLAIN works on SELECT, UPDATE, or DELETE",
                ))
            }
        };
        let schema = self.schema_cloned(table)?;
        let plan = planner::plan(&schema, where_clause);
        let select = match inner {
            Statement::Select(s) => Some(s),
            _ => None,
        };
        let text = self.render_plan_text(&schema, &plan, select, what);
        let rows: Vec<Vec<Value>> = text
            .lines()
            .map(|l| vec![Value::Text(l.to_string())])
            .collect();
        Ok(QueryResult {
            columns: vec!["query plan".into()],
            rows,
            message: None,
            plan: Some(text),
            plan_access: Some(planner::access_to_json(&plan)),
            stats: Stats::default(),
            trace: Vec::new(),
            trace_dropped: 0,
        })
    }

    fn render_plan_text(
        &self,
        schema: &TableSchema,
        plan: &Plan,
        select: Option<&SelectStmt>,
        what: &str,
    ) -> String {
        let mut lines = vec![format!("{what} on '{}'", schema.name)];
        lines.push(format!(
            "  access: {}",
            planner::describe_access(plan, &schema.name)
        ));
        if let Some(filter) = &plan.filter {
            lines.push(format!(
                "  filter: {} (re-checked on every row the access path yields)",
                filter.display()
            ));
        }
        if let Some(stmt) = select {
            if let Some((col, asc)) = &stmt.order_by {
                let is_pk_asc = schema
                    .pk_index()
                    .map(|i| schema.columns[i].name.eq_ignore_ascii_case(col) && *asc)
                    .unwrap_or(false);
                if is_pk_asc {
                    lines.push(format!(
                        "  sort: none needed — the B+tree already returns rows in {col} order"
                    ));
                } else {
                    lines.push(format!(
                        "  sort: in-memory sort by {col} {}",
                        if *asc { "ASC" } else { "DESC" }
                    ));
                }
            }
            if let Some(limit) = stmt.limit {
                lines.push(format!("  limit: {limit}"));
            }
        }
        lines.join("\n")
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        // Best-effort clean shutdown: make the main file current and empty
        // the WAL, so a copy of just the .db file is a complete database.
        if !self.pager.has_dirty() {
            let _ = self.pager.sync_storage();
            let _ = self.wal.reset();
        }
    }
}
