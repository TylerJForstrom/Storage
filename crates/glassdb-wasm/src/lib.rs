//! WebAssembly bindings: the real GlassDB engine, in the browser.
//!
//! Everything crosses the JS boundary as JSON strings (the engine has its
//! own zero-dependency JSON encoder), so the JS side stays trivially simple:
//! `JSON.parse(db.execute(sql))`.

use glassdb::errors::locate;
use glassdb::json::J;
use glassdb::{Database, DbError};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub struct WasmDb {
    inner: Database,
}

#[wasm_bindgen]
impl WasmDb {
    /// A fresh in-memory database.
    #[wasm_bindgen(constructor)]
    #[allow(clippy::new_without_default)]
    pub fn new() -> WasmDb {
        WasmDb {
            inner: Database::open_memory().expect("in-memory open cannot fail"),
        }
    }

    /// Run one SQL statement. Returns the QueryResult as JSON, or
    /// `{"error": {...}}` with line/column info.
    pub fn execute(&mut self, sql: &str) -> String {
        match self.inner.execute(sql) {
            Ok(result) => result.to_json(),
            Err(e) => error_json(&e, sql),
        }
    }

    /// B+tree shape of a table, as JSON, for the tree visualizer.
    pub fn layout(&mut self, table: &str) -> String {
        match self.inner.btree_layout(table) {
            Ok(json) => json,
            Err(e) => error_json(&e, ""),
        }
    }

    /// All table schemas, for the sidebar.
    pub fn schemas(&self) -> String {
        self.inner.schemas_json()
    }

    /// Create the demo `users` table (400 rows).
    pub fn seed(&mut self) -> String {
        match self.inner.seed_demo() {
            Ok(msg) => J::O(vec![("message".into(), J::s(msg))]).render(),
            Err(e) => error_json(&e, ""),
        }
    }
}

fn error_json(e: &DbError, sql: &str) -> String {
    let mut fields = vec![
        ("kind".into(), J::s(e.kind.name())),
        ("message".into(), J::s(e.message.clone())),
    ];
    if let Some(pos) = e.position {
        let (line, col, line_text) = locate(sql, pos);
        fields.push(("position".into(), J::I(pos as i64)));
        fields.push(("line".into(), J::I(line as i64)));
        fields.push(("col".into(), J::I(col as i64)));
        fields.push(("line_text".into(), J::s(line_text)));
    }
    J::O(vec![("error".into(), J::O(fields))]).render()
}
