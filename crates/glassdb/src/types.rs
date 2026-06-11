//! Values, column types, table schemas, and the on-disk row codec.

use std::cmp::Ordering;
use std::fmt;

use crate::errors::{DbError, DbResult};
use crate::json::J;

/// A single SQL value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Bool(bool),
}

impl Value {
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "NULL",
            Value::Int(_) => "INTEGER",
            Value::Real(_) => "REAL",
            Value::Text(_) => "TEXT",
            Value::Bool(_) => "BOOLEAN",
        }
    }

    /// SQL comparison. Integers and reals compare numerically with each
    /// other; anything involving NULL (or mismatched types) is incomparable
    /// and returns None, which makes WHERE treat it as "not a match".
    pub fn compare(&self, other: &Value) -> Option<Ordering> {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => Some(a.cmp(b)),
            (Value::Int(a), Value::Real(b)) => (*a as f64).partial_cmp(b),
            (Value::Real(a), Value::Int(b)) => a.partial_cmp(&(*b as f64)),
            (Value::Real(a), Value::Real(b)) => a.partial_cmp(b),
            (Value::Text(a), Value::Text(b)) => Some(a.cmp(b)),
            (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
            _ => None,
        }
    }

    pub fn to_json(&self) -> J {
        match self {
            Value::Null => J::Null,
            Value::Int(i) => J::I(*i),
            Value::Real(f) => J::F(*f),
            Value::Text(s) => J::s(s.clone()),
            Value::Bool(b) => J::B(*b),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Real(r) => write!(f, "{r}"),
            Value::Text(s) => write!(f, "{s}"),
            Value::Bool(b) => write!(f, "{b}"),
        }
    }
}

/// Declared type of a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColType {
    Int,
    Real,
    Text,
    Bool,
}

impl ColType {
    pub fn name(self) -> &'static str {
        match self {
            ColType::Int => "INTEGER",
            ColType::Real => "REAL",
            ColType::Text => "TEXT",
            ColType::Bool => "BOOLEAN",
        }
    }

    pub fn from_keyword(kw: &str) -> Option<ColType> {
        match kw.to_ascii_uppercase().as_str() {
            "INTEGER" | "INT" => Some(ColType::Int),
            "REAL" | "FLOAT" | "DOUBLE" => Some(ColType::Real),
            "TEXT" | "STRING" | "VARCHAR" => Some(ColType::Text),
            "BOOLEAN" | "BOOL" => Some(ColType::Bool),
            _ => None,
        }
    }

    fn tag(self) -> u8 {
        match self {
            ColType::Int => 1,
            ColType::Real => 2,
            ColType::Text => 3,
            ColType::Bool => 4,
        }
    }

    fn from_tag(tag: u8) -> Option<ColType> {
        match tag {
            1 => Some(ColType::Int),
            2 => Some(ColType::Real),
            3 => Some(ColType::Text),
            4 => Some(ColType::Bool),
            _ => None,
        }
    }
}

/// Coerce a value into a column's declared type. The only implicit
/// conversion allowed is INTEGER -> REAL (lossless); everything else is a
/// type error — this engine refuses to guess.
pub fn coerce(value: Value, ty: ColType, col_name: &str) -> DbResult<Value> {
    match (&value, ty) {
        (Value::Null, _) => Ok(Value::Null),
        (Value::Int(_), ColType::Int) => Ok(value),
        (Value::Real(_), ColType::Real) => Ok(value),
        (Value::Text(_), ColType::Text) => Ok(value),
        (Value::Bool(_), ColType::Bool) => Ok(value),
        (Value::Int(i), ColType::Real) => Ok(Value::Real(*i as f64)),
        _ => Err(DbError::type_error(format!(
            "column '{col_name}' is {}, but the value {value:?} is {}",
            ty.name(),
            value.type_name()
        ))),
    }
}

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub ty: ColType,
    pub primary_key: bool,
}

#[derive(Debug, Clone)]
pub struct TableSchema {
    pub id: i64,
    pub name: String,
    /// Page id of the root of this table's B+tree. Changes when the root
    /// splits, and is persisted in the catalog.
    pub root: u32,
    pub columns: Vec<Column>,
}

impl TableSchema {
    /// Index of the declared INTEGER PRIMARY KEY column, if there is one.
    /// Tables without one get a hidden auto-increment rowid as the B+tree key.
    pub fn pk_index(&self) -> Option<usize> {
        self.columns.iter().position(|c| c.primary_key)
    }

    pub fn col_index(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    }

    // --- catalog codec -----------------------------------------------------
    // Schemas are stored as the values of the catalog B+tree (key = table id).

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        write_str(&mut out, &self.name);
        out.extend_from_slice(&self.root.to_le_bytes());
        out.extend_from_slice(&(self.columns.len() as u16).to_le_bytes());
        for col in &self.columns {
            write_str(&mut out, &col.name);
            out.push(col.ty.tag());
            out.push(col.primary_key as u8);
        }
        out
    }

    pub fn decode(id: i64, bytes: &[u8]) -> DbResult<TableSchema> {
        let mut r = Reader::new(bytes);
        let name = r.read_str()?;
        let root = r.read_u32()?;
        let n_cols = r.read_u16()? as usize;
        let mut columns = Vec::with_capacity(n_cols);
        for _ in 0..n_cols {
            let col_name = r.read_str()?;
            let ty = ColType::from_tag(r.read_u8()?)
                .ok_or_else(|| DbError::corruption("unknown column type tag in catalog"))?;
            let primary_key = r.read_u8()? != 0;
            columns.push(Column {
                name: col_name,
                ty,
                primary_key,
            });
        }
        Ok(TableSchema {
            id,
            name,
            root,
            columns,
        })
    }
}

// --- row codec ---------------------------------------------------------
// A row is stored as the value of a B+tree leaf cell:
//   u16 column count, then per value: 1 tag byte + fixed/length-prefixed payload.

const TAG_NULL: u8 = 0;
const TAG_INT: u8 = 1;
const TAG_REAL: u8 = 2;
const TAG_TEXT: u8 = 3;
const TAG_BOOL: u8 = 4;

pub fn encode_row(values: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(values.len() as u16).to_le_bytes());
    for v in values {
        match v {
            Value::Null => out.push(TAG_NULL),
            Value::Int(i) => {
                out.push(TAG_INT);
                out.extend_from_slice(&i.to_le_bytes());
            }
            Value::Real(f) => {
                out.push(TAG_REAL);
                out.extend_from_slice(&f.to_le_bytes());
            }
            Value::Text(s) => {
                out.push(TAG_TEXT);
                out.extend_from_slice(&(s.len() as u32).to_le_bytes());
                out.extend_from_slice(s.as_bytes());
            }
            Value::Bool(b) => {
                out.push(TAG_BOOL);
                out.push(*b as u8);
            }
        }
    }
    out
}

pub fn decode_row(bytes: &[u8]) -> DbResult<Vec<Value>> {
    let mut r = Reader::new(bytes);
    let n = r.read_u16()? as usize;
    let mut values = Vec::with_capacity(n);
    for _ in 0..n {
        let tag = r.read_u8()?;
        values.push(match tag {
            TAG_NULL => Value::Null,
            TAG_INT => Value::Int(i64::from_le_bytes(r.read_array()?)),
            TAG_REAL => Value::Real(f64::from_le_bytes(r.read_array()?)),
            TAG_TEXT => {
                let len = r.read_u32()? as usize;
                let raw = r.read_bytes(len)?;
                Value::Text(
                    String::from_utf8(raw.to_vec())
                        .map_err(|_| DbError::corruption("row contains invalid UTF-8"))?,
                )
            }
            TAG_BOOL => Value::Bool(r.read_u8()? != 0),
            _ => {
                return Err(DbError::corruption(format!(
                    "unknown value tag {tag} in row"
                )))
            }
        });
    }
    Ok(values)
}

// --- small byte reader used by the codecs --------------------------------

pub struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Reader { bytes, pos: 0 }
    }

    pub fn read_bytes(&mut self, n: usize) -> DbResult<&'a [u8]> {
        if self.pos + n > self.bytes.len() {
            return Err(DbError::corruption("unexpected end of encoded data"));
        }
        let s = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn read_array<const N: usize>(&mut self) -> DbResult<[u8; N]> {
        let s = self.read_bytes(N)?;
        let mut a = [0u8; N];
        a.copy_from_slice(s);
        Ok(a)
    }

    pub fn read_u8(&mut self) -> DbResult<u8> {
        Ok(self.read_array::<1>()?[0])
    }
    pub fn read_u16(&mut self) -> DbResult<u16> {
        Ok(u16::from_le_bytes(self.read_array()?))
    }
    pub fn read_u32(&mut self) -> DbResult<u32> {
        Ok(u32::from_le_bytes(self.read_array()?))
    }
    pub fn read_u64(&mut self) -> DbResult<u64> {
        Ok(u64::from_le_bytes(self.read_array()?))
    }
    pub fn read_i64(&mut self) -> DbResult<i64> {
        Ok(i64::from_le_bytes(self.read_array()?))
    }

    pub fn read_str(&mut self) -> DbResult<String> {
        let len = self.read_u16()? as usize;
        let raw = self.read_bytes(len)?;
        String::from_utf8(raw.to_vec())
            .map_err(|_| DbError::corruption("invalid UTF-8 in encoded string"))
    }

    pub fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }
}

fn write_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u16).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_roundtrip() {
        let row = vec![
            Value::Int(-42),
            Value::Null,
            Value::Text("héllo 'world'".into()),
            Value::Real(3.5),
            Value::Bool(true),
        ];
        let bytes = encode_row(&row);
        assert_eq!(decode_row(&bytes).unwrap(), row);
    }

    #[test]
    fn schema_roundtrip() {
        let schema = TableSchema {
            id: 7,
            name: "users".into(),
            root: 12,
            columns: vec![
                Column {
                    name: "id".into(),
                    ty: ColType::Int,
                    primary_key: true,
                },
                Column {
                    name: "name".into(),
                    ty: ColType::Text,
                    primary_key: false,
                },
            ],
        };
        let decoded = TableSchema::decode(7, &schema.encode()).unwrap();
        assert_eq!(decoded.name, "users");
        assert_eq!(decoded.root, 12);
        assert_eq!(decoded.columns.len(), 2);
        assert!(decoded.columns[0].primary_key);
    }

    #[test]
    fn truncated_row_is_corruption_not_panic() {
        let bytes = encode_row(&[Value::Text("hello".into())]);
        assert!(decode_row(&bytes[..bytes.len() - 2]).is_err());
    }
}
