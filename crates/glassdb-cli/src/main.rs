//! The GlassDB REPL.
//!
//!   glassdb            -> opens ./glass.db
//!   glassdb my.db      -> opens my.db (creates it if missing)
//!   glassdb :memory:   -> throwaway in-memory database

use std::io::{self, BufRead, Write};

use glassdb::errors::locate;
use glassdb::trace::TraceEvent;
use glassdb::{Database, DbError, Value};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "glass.db".to_string());

    let opened = if path == ":memory:" {
        Database::open_memory()
    } else {
        Database::open_file(&path)
    };
    let mut db = match opened {
        Ok(db) => db,
        Err(e) => {
            eprintln!("could not open '{path}': {e}");
            std::process::exit(1);
        }
    };

    println!("GlassDB · {path}");
    println!("type SQL ending with ';' — or .help for commands");
    if let Some((frames, txns)) = db.recovered {
        println!(
            "crash recovery: replayed {txns} committed transaction(s) from {frames} WAL frames"
        );
    }

    let mut trace_on = false;
    let stdin = io::stdin();
    let mut buffer = String::new();

    loop {
        print!(
            "{}",
            if buffer.is_empty() {
                "glassdb> "
            } else {
                "    ...> "
            }
        );
        let _ = io::stdout().flush();
        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(_) => break,
        }
        let trimmed = line.trim();

        if buffer.is_empty() && trimmed.starts_with('.') {
            if !dot_command(&mut db, trimmed, &mut trace_on) {
                break;
            }
            continue;
        }

        buffer.push_str(&line);
        if trimmed.ends_with(';') {
            let sql = std::mem::take(&mut buffer);
            run_sql(&mut db, &sql, trace_on);
        }
    }
    println!();
}

/// Returns false when the REPL should exit.
fn dot_command(db: &mut Database, cmd: &str, trace_on: &mut bool) -> bool {
    let mut parts = cmd.split_whitespace();
    match parts.next().unwrap_or("") {
        ".quit" | ".exit" | ".q" => return false,
        ".help" => {
            println!(".tables          list tables");
            println!(".schema <table>  show a table's columns");
            println!(".seed            create a demo 'users' table with 400 rows");
            println!(".trace on|off    show every page read / WAL write per statement");
            println!(".tree <table>    dump a table's B+tree shape as JSON");
            println!(".quit            exit");
            println!();
            println!("everything else is SQL — end statements with ';'");
            println!("try: EXPLAIN SELECT * FROM users WHERE id = 250;");
        }
        ".tables" => {
            let names = db.table_names();
            if names.is_empty() {
                println!("(no tables — try .seed or CREATE TABLE)");
            } else {
                for n in names {
                    println!("{n}");
                }
            }
        }
        ".schema" => match parts.next() {
            Some(table) => match db.schema_of(table) {
                Ok(schema) => {
                    for c in &schema.columns {
                        println!(
                            "{}  {}{}",
                            c.name,
                            c.ty.name(),
                            if c.primary_key { "  PRIMARY KEY" } else { "" }
                        );
                    }
                }
                Err(e) => eprintln!("{e}"),
            },
            None => eprintln!("usage: .schema <table>"),
        },
        ".seed" => match db.seed_demo() {
            Ok(msg) => println!("{msg}"),
            Err(e) => eprintln!("{e}"),
        },
        ".trace" => {
            *trace_on = match parts.next() {
                Some("on") => true,
                Some("off") => false,
                _ => !*trace_on,
            };
            println!("trace is {}", if *trace_on { "on" } else { "off" });
        }
        ".tree" => match parts.next() {
            Some(table) => match db.btree_layout(table) {
                Ok(json) => println!("{json}"),
                Err(e) => eprintln!("{e}"),
            },
            None => eprintln!("usage: .tree <table>"),
        },
        other => eprintln!("unknown command '{other}' — try .help"),
    }
    true
}

fn run_sql(db: &mut Database, sql: &str, trace_on: bool) {
    match db.execute_script(sql) {
        Ok(results) => {
            for r in results {
                if !r.columns.is_empty() {
                    print_table(&r.columns, &r.rows);
                }
                if let Some(msg) = &r.message {
                    println!("{msg}");
                }
                println!("{}", stats_line(&r.stats));
                if trace_on {
                    print_trace(&r.trace, r.trace_dropped);
                }
            }
        }
        Err(e) => print_error(&e, sql),
    }
}

fn stats_line(s: &glassdb::trace::Stats) -> String {
    let time = if s.elapsed_micros >= 1000 {
        format!("{:.1} ms", s.elapsed_micros as f64 / 1000.0)
    } else {
        format!("{} µs", s.elapsed_micros)
    };
    format!(
        "· {} page reads ({} cached) · {} pages written · {} WAL frames · {} rows scanned · {time}",
        s.pages_read, s.cache_hits, s.pages_written, s.wal_frames, s.rows_scanned
    )
}

fn print_trace(events: &[TraceEvent], dropped: u64) {
    const SHOW: usize = 60;
    for e in events.iter().take(SHOW) {
        println!("  {}", fmt_event(e));
    }
    let hidden = events.len().saturating_sub(SHOW) as u64 + dropped;
    if hidden > 0 {
        println!("  … {hidden} more events");
    }
}

fn fmt_event(e: &TraceEvent) -> String {
    match e {
        TraceEvent::PageRead { pid, kind, cached } => format!(
            "read  page {pid:>4} [{}]{}",
            kind.name(),
            if *cached {
                " (buffer pool hit)"
            } else {
                " (from disk)"
            }
        ),
        TraceEvent::PageWrite { pid } => format!("write page {pid:>4} -> db file"),
        TraceEvent::PageAlloc { pid } => format!("alloc page {pid:>4}"),
        TraceEvent::PageFree { pid } => format!("free  page {pid:>4}"),
        TraceEvent::WalFrame { lsn, pid } => format!("wal   frame lsn={lsn} page {pid}"),
        TraceEvent::WalCommit { lsn } => format!("wal   COMMIT lsn={lsn}"),
        TraceEvent::WalSync => "wal   fsync (durability point)".to_string(),
        TraceEvent::WalCheckpoint { frames } => {
            format!("wal   checkpoint ({frames} frames applied, log reset)")
        }
        TraceEvent::WalRecovery { frames, txns } => {
            format!("wal   recovery replayed {txns} txns from {frames} frames")
        }
        TraceEvent::Note { text } => format!("note  {text}"),
    }
}

fn print_error(e: &DbError, sql: &str) {
    eprintln!("{e}");
    if let Some(pos) = e.position {
        let (line_no, col, line_text) = locate(sql, pos);
        eprintln!("  line {line_no}: {line_text}");
        eprintln!(
            "  {}^",
            " ".repeat(format!("line {line_no}: ").len() + col - 1)
        );
    }
}

fn print_table(columns: &[String], rows: &[Vec<Value>]) {
    const MAX_W: usize = 40;
    let render = |v: &Value| {
        let s = v.to_string();
        if s.len() > MAX_W {
            format!(
                "{}…",
                &s[..s
                    .char_indices()
                    .take_while(|(i, _)| *i < MAX_W - 1)
                    .last()
                    .map(|(i, c)| i + c.len_utf8())
                    .unwrap_or(0)]
            )
        } else {
            s
        }
    };
    let mut widths: Vec<usize> = columns.iter().map(|c| c.len().min(MAX_W)).collect();
    let rendered: Vec<Vec<String>> = rows
        .iter()
        .map(|row| row.iter().map(render).collect())
        .collect();
    for row in &rendered {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }
    }
    let right_align: Vec<bool> = (0..columns.len())
        .map(|i| {
            rows.iter()
                .filter_map(|r| r.get(i))
                .all(|v| matches!(v, Value::Int(_) | Value::Real(_) | Value::Null))
                && !rows.is_empty()
        })
        .collect();

    let header: Vec<String> = columns
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{:<w$}", c, w = widths[i]))
        .collect();
    println!("{}", header.join("  "));
    println!(
        "{}",
        widths
            .iter()
            .map(|w| "-".repeat(*w))
            .collect::<Vec<_>>()
            .join("  ")
    );
    for row in &rendered {
        let cells: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                let w = widths.get(i).copied().unwrap_or(0);
                if right_align.get(i).copied().unwrap_or(false) {
                    format!("{:>w$}", cell, w = w)
                } else {
                    format!("{:<w$}", cell, w = w)
                }
            })
            .collect();
        println!("{}", cells.join("  "));
    }
    println!(
        "({} row{})",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" }
    );
}
