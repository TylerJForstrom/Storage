//! Error type for the whole engine. Errors carry a byte position into the
//! SQL text when one is known, so the CLI and web UI can point at the exact
//! spot — the same philosophy as a good compiler diagnostic.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// The SQL text could not be parsed.
    Syntax,
    /// Unknown table/column, duplicate table, primary-key violation, etc.
    Schema,
    /// A value had the wrong type for where it was used.
    Type,
    /// The underlying storage failed (I/O error, simulated crash).
    Storage,
    /// On-disk bytes failed validation (bad magic, bad checksum).
    Corruption,
    /// Valid SQL that this engine intentionally does not support yet.
    Unsupported,
}

impl ErrorKind {
    pub fn name(self) -> &'static str {
        match self {
            ErrorKind::Syntax => "syntax error",
            ErrorKind::Schema => "schema error",
            ErrorKind::Type => "type error",
            ErrorKind::Storage => "storage error",
            ErrorKind::Corruption => "corruption detected",
            ErrorKind::Unsupported => "not supported",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DbError {
    pub kind: ErrorKind,
    pub message: String,
    /// Byte offset into the SQL text where the problem starts, when known.
    pub position: Option<usize>,
}

impl DbError {
    pub fn syntax(message: impl Into<String>, position: usize) -> Self {
        DbError {
            kind: ErrorKind::Syntax,
            message: message.into(),
            position: Some(position),
        }
    }
    pub fn schema(message: impl Into<String>) -> Self {
        DbError {
            kind: ErrorKind::Schema,
            message: message.into(),
            position: None,
        }
    }
    pub fn type_error(message: impl Into<String>) -> Self {
        DbError {
            kind: ErrorKind::Type,
            message: message.into(),
            position: None,
        }
    }
    pub fn storage(message: impl Into<String>) -> Self {
        DbError {
            kind: ErrorKind::Storage,
            message: message.into(),
            position: None,
        }
    }
    pub fn corruption(message: impl Into<String>) -> Self {
        DbError {
            kind: ErrorKind::Corruption,
            message: message.into(),
            position: None,
        }
    }
    pub fn unsupported(message: impl Into<String>) -> Self {
        DbError {
            kind: ErrorKind::Unsupported,
            message: message.into(),
            position: None,
        }
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.name(), self.message)
    }
}

impl std::error::Error for DbError {}

impl From<std::io::Error> for DbError {
    fn from(e: std::io::Error) -> Self {
        DbError::storage(e.to_string())
    }
}

pub type DbResult<T> = Result<T, DbError>;

/// Map a byte offset back to (1-based line, 1-based column, the line's text).
/// Used to render caret diagnostics like:
/// ```text
///   SELEC name FROM users;
///   ^ expected a statement keyword, found 'SELEC'
/// ```
pub fn locate(sql: &str, pos: usize) -> (usize, usize, String) {
    let pos = pos.min(sql.len());
    let before = &sql[..pos];
    let line = before.bytes().filter(|&b| b == b'\n').count() + 1;
    let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let col = sql[line_start..pos].chars().count() + 1;
    let line_end = sql[line_start..]
        .find('\n')
        .map(|i| line_start + i)
        .unwrap_or(sql.len());
    (line, col, sql[line_start..line_end].to_string())
}
