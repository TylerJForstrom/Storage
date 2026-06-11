//! Expression evaluation and aggregates. Pure functions over rows — all the
//! storage-touching orchestration lives in `db.rs`.

use std::cmp::Ordering;

use crate::errors::{DbError, DbResult};
use crate::sql::ast::{BinOp, Expr, UnOp};
use crate::types::{TableSchema, Value};

/// Evaluate an expression against one row of a table.
pub fn eval(expr: &Expr, schema: &TableSchema, row: &[Value]) -> DbResult<Value> {
    match expr {
        Expr::Literal(v) => Ok(v.clone()),
        Expr::Column(name) => match schema.col_index(name) {
            Some(i) => Ok(row[i].clone()),
            None => Err(DbError::schema(format!(
                "no column '{name}' in table '{}' (columns: {})",
                schema.name,
                schema
                    .columns
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        },
        Expr::Unary { op, expr } => {
            let v = eval(expr, schema, row)?;
            apply_unary(*op, v)
        }
        Expr::Binary { op, left, right } => {
            let l = eval(left, schema, row)?;
            let r = eval(right, schema, row)?;
            apply_binary(*op, l, r)
        }
        Expr::Call { func, .. } => Err(DbError::unsupported(format!(
            "{}() is an aggregate — it can appear in the SELECT list, not inside \
             a row expression",
            func.to_ascii_uppercase()
        ))),
    }
}

/// Evaluate an expression that must not reference any column (VALUES lists).
pub fn eval_const(expr: &Expr) -> DbResult<Value> {
    match expr {
        Expr::Literal(v) => Ok(v.clone()),
        Expr::Column(name) => Err(DbError::type_error(format!(
            "'{name}' looks like a column reference, but VALUES only takes constants"
        ))),
        Expr::Unary { op, expr } => apply_unary(*op, eval_const(expr)?),
        Expr::Binary { op, left, right } => {
            apply_binary(*op, eval_const(left)?, eval_const(right)?)
        }
        Expr::Call { func, .. } => Err(DbError::type_error(format!(
            "{}() can't be used inside VALUES",
            func.to_ascii_uppercase()
        ))),
    }
}

fn apply_unary(op: UnOp, v: Value) -> DbResult<Value> {
    match (op, v) {
        (_, Value::Null) => Ok(Value::Null),
        (UnOp::Neg, Value::Int(i)) => Ok(Value::Int(-i)),
        (UnOp::Neg, Value::Real(f)) => Ok(Value::Real(-f)),
        (UnOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
        (UnOp::Neg, v) => Err(DbError::type_error(format!(
            "can't negate a {} value",
            v.type_name()
        ))),
        (UnOp::Not, v) => Err(DbError::type_error(format!(
            "NOT needs a BOOLEAN, got {}",
            v.type_name()
        ))),
    }
}

fn apply_binary(op: BinOp, l: Value, r: Value) -> DbResult<Value> {
    match op {
        BinOp::And | BinOp::Or => logical(op, l, r),
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            // SQL three-valued logic: comparing with NULL yields NULL,
            // and WHERE only keeps rows where the result is exactly TRUE.
            if matches!(l, Value::Null) || matches!(r, Value::Null) {
                return Ok(Value::Null);
            }
            match l.compare(&r) {
                Some(ord) => Ok(Value::Bool(match op {
                    BinOp::Eq => ord == Ordering::Equal,
                    BinOp::Ne => ord != Ordering::Equal,
                    BinOp::Lt => ord == Ordering::Less,
                    BinOp::Le => ord != Ordering::Greater,
                    BinOp::Gt => ord == Ordering::Greater,
                    BinOp::Ge => ord != Ordering::Less,
                    _ => unreachable!(),
                })),
                None => Err(DbError::type_error(format!(
                    "can't compare {} with {}",
                    l.type_name(),
                    r.type_name()
                ))),
            }
        }
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => arithmetic(op, l, r),
    }
}

fn logical(op: BinOp, l: Value, r: Value) -> DbResult<Value> {
    let as_bool = |v: &Value| -> DbResult<Option<bool>> {
        match v {
            Value::Bool(b) => Ok(Some(*b)),
            Value::Null => Ok(None),
            v => Err(DbError::type_error(format!(
                "{} needs BOOLEAN operands, got {}",
                op.symbol(),
                v.type_name()
            ))),
        }
    };
    let (lb, rb) = (as_bool(&l)?, as_bool(&r)?);
    // Kleene three-valued logic, same as real SQL.
    Ok(match op {
        BinOp::And => match (lb, rb) {
            (Some(false), _) | (_, Some(false)) => Value::Bool(false),
            (Some(true), Some(true)) => Value::Bool(true),
            _ => Value::Null,
        },
        BinOp::Or => match (lb, rb) {
            (Some(true), _) | (_, Some(true)) => Value::Bool(true),
            (Some(false), Some(false)) => Value::Bool(false),
            _ => Value::Null,
        },
        _ => unreachable!(),
    })
}

fn arithmetic(op: BinOp, l: Value, r: Value) -> DbResult<Value> {
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return Ok(Value::Null);
    }
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => match op {
            BinOp::Add => Ok(Value::Int(a.wrapping_add(b))),
            BinOp::Sub => Ok(Value::Int(a.wrapping_sub(b))),
            BinOp::Mul => Ok(Value::Int(a.wrapping_mul(b))),
            BinOp::Div => {
                if b == 0 {
                    Err(DbError::type_error("division by zero"))
                } else {
                    Ok(Value::Int(a.wrapping_div(b)))
                }
            }
            _ => unreachable!(),
        },
        (l, r) => {
            let to_f = |v: &Value| -> DbResult<f64> {
                match v {
                    Value::Int(i) => Ok(*i as f64),
                    Value::Real(f) => Ok(*f),
                    v => Err(DbError::type_error(format!(
                        "arithmetic needs numbers, got {}",
                        v.type_name()
                    ))),
                }
            };
            let (a, b) = (to_f(&l)?, to_f(&r)?);
            match op {
                BinOp::Add => Ok(Value::Real(a + b)),
                BinOp::Sub => Ok(Value::Real(a - b)),
                BinOp::Mul => Ok(Value::Real(a * b)),
                BinOp::Div => {
                    if b == 0.0 {
                        Err(DbError::type_error("division by zero"))
                    } else {
                        Ok(Value::Real(a / b))
                    }
                }
                _ => unreachable!(),
            }
        }
    }
}

/// WHERE keeps a row only when the predicate is exactly TRUE
/// (FALSE and NULL both drop it — real SQL semantics).
pub fn is_true(v: &Value) -> bool {
    matches!(v, Value::Bool(true))
}

/// Order rows by one column: NULLs first, then natural order.
pub fn compare_for_sort(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        _ => a.compare(b).unwrap_or(Ordering::Equal),
    }
}

// --- aggregates -----------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggKind {
    CountStar,
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl AggKind {
    pub fn from_call(func: &str, has_arg: bool, star: bool) -> DbResult<AggKind> {
        let kind = match func.to_ascii_lowercase().as_str() {
            "count" if star => AggKind::CountStar,
            "count" => AggKind::Count,
            "sum" => AggKind::Sum,
            "avg" => AggKind::Avg,
            "min" => AggKind::Min,
            "max" => AggKind::Max,
            _ => {
                return Err(DbError::unsupported(format!(
                    "unknown function '{func}' — available: COUNT, SUM, AVG, MIN, MAX"
                )))
            }
        };
        if kind != AggKind::CountStar && !has_arg {
            return Err(DbError::type_error(format!(
                "{}() needs a column or expression argument",
                func.to_ascii_uppercase()
            )));
        }
        Ok(kind)
    }
}

pub struct AggState {
    kind: AggKind,
    count: u64,
    sum_int: Option<i64>, // None once a REAL appears or an overflow happens
    sum_real: f64,
    best: Option<Value>, // MIN/MAX
}

impl AggState {
    pub fn new(kind: AggKind) -> AggState {
        AggState {
            kind,
            count: 0,
            sum_int: Some(0),
            sum_real: 0.0,
            best: None,
        }
    }

    /// Feed one row's value (ignored for COUNT(*), which counts rows).
    pub fn update(&mut self, v: &Value) -> DbResult<()> {
        match self.kind {
            AggKind::CountStar => self.count += 1,
            AggKind::Count => {
                if !matches!(v, Value::Null) {
                    self.count += 1;
                }
            }
            AggKind::Sum | AggKind::Avg => match v {
                Value::Null => {}
                Value::Int(i) => {
                    self.count += 1;
                    self.sum_real += *i as f64;
                    self.sum_int = self.sum_int.and_then(|s| s.checked_add(*i));
                }
                Value::Real(f) => {
                    self.count += 1;
                    self.sum_real += f;
                    self.sum_int = None;
                }
                v => {
                    return Err(DbError::type_error(format!(
                        "SUM/AVG need numbers, got {}",
                        v.type_name()
                    )))
                }
            },
            AggKind::Min | AggKind::Max => {
                if matches!(v, Value::Null) {
                    return Ok(());
                }
                match &self.best {
                    None => self.best = Some(v.clone()),
                    Some(cur) => {
                        let ord = v.compare(cur).ok_or_else(|| {
                            DbError::type_error(format!(
                                "MIN/MAX saw mixed types: {} vs {}",
                                v.type_name(),
                                cur.type_name()
                            ))
                        })?;
                        let take = if self.kind == AggKind::Min {
                            ord == Ordering::Less
                        } else {
                            ord == Ordering::Greater
                        };
                        if take {
                            self.best = Some(v.clone());
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn finish(self) -> Value {
        match self.kind {
            AggKind::CountStar | AggKind::Count => Value::Int(self.count as i64),
            AggKind::Sum => {
                if self.count == 0 {
                    Value::Null
                } else {
                    match self.sum_int {
                        Some(s) => Value::Int(s),
                        None => Value::Real(self.sum_real),
                    }
                }
            }
            AggKind::Avg => {
                if self.count == 0 {
                    Value::Null
                } else {
                    Value::Real(self.sum_real / self.count as f64)
                }
            }
            AggKind::Min | AggKind::Max => self.best.unwrap_or(Value::Null),
        }
    }
}
