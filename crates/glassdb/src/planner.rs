//! The query planner: looks at a WHERE clause and decides how to touch the
//! B+tree. Three possibilities, in order of preference:
//!
//! - `PkLookup`  — WHERE pins the primary key to one value: O(log n), a
//!   single root-to-leaf descent.
//! - `PkRange`   — WHERE bounds the primary key: descend once, then walk
//!   the leaf chain only across the matching range.
//! - `FullScan`  — nothing usable: walk every leaf.
//!
//! It can also prove a query returns `Nothing` (e.g. `id > 10 AND id < 5`).
//!
//! Honesty note: the original WHERE expression is always re-checked against
//! every row the access path yields, so a planner bug can make a query
//! slower, never wrong.

use std::ops::Bound;

use crate::json::J;
use crate::sql::ast::{BinOp, Expr};
use crate::types::{TableSchema, Value};

#[derive(Debug, Clone, PartialEq)]
pub enum Access {
    PkLookup(i64),
    PkRange {
        lo: Bound<i64>,
        hi: Bound<i64>,
    },
    FullScan,
    /// The planner proved no row can match.
    Nothing,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub access: Access,
    /// Name of the primary-key column the access path uses, if any.
    pub pk_name: Option<String>,
    /// The full WHERE clause, always re-applied to every row.
    pub filter: Option<Expr>,
}

pub fn plan(schema: &TableSchema, where_clause: &Option<Expr>) -> Plan {
    let pk_name = schema.pk_index().map(|i| schema.columns[i].name.clone());
    let access = match (where_clause, &pk_name) {
        (Some(expr), Some(pk)) => match key_range(expr, pk) {
            Some((lo, hi)) => classify(lo, hi),
            None => Access::FullScan,
        },
        _ => Access::FullScan,
    };
    Plan {
        access,
        pk_name,
        filter: where_clause.clone(),
    }
}

/// What constraint does `expr` place on the primary key? `None` means
/// "no constraint derivable" (which is always safe — it just means scanning
/// more than strictly necessary).
fn key_range(expr: &Expr, pk: &str) -> Option<(Bound<i64>, Bound<i64>)> {
    match expr {
        Expr::Binary {
            op: BinOp::And,
            left,
            right,
        } => match (key_range(left, pk), key_range(right, pk)) {
            (Some(a), Some(b)) => Some(intersect(a, b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        },
        Expr::Binary { op, left, right } => {
            // Match  pk <op> literal  or  literal <op> pk  (flipping the op).
            let (col, lit, op) = match (&**left, &**right) {
                (Expr::Column(c), Expr::Literal(Value::Int(n))) => (c, *n, *op),
                (Expr::Literal(Value::Int(n)), Expr::Column(c)) => (c, *n, flip(*op)?),
                _ => return None,
            };
            if !col.eq_ignore_ascii_case(pk) {
                return None;
            }
            match op {
                BinOp::Eq => Some((Bound::Included(lit), Bound::Included(lit))),
                BinOp::Lt => Some((Bound::Unbounded, Bound::Excluded(lit))),
                BinOp::Le => Some((Bound::Unbounded, Bound::Included(lit))),
                BinOp::Gt => Some((Bound::Excluded(lit), Bound::Unbounded)),
                BinOp::Ge => Some((Bound::Included(lit), Bound::Unbounded)),
                _ => None,
            }
        }
        _ => None,
    }
}

/// `5 < id` is `id > 5`.
fn flip(op: BinOp) -> Option<BinOp> {
    Some(match op {
        BinOp::Eq => BinOp::Eq,
        BinOp::Lt => BinOp::Gt,
        BinOp::Le => BinOp::Ge,
        BinOp::Gt => BinOp::Lt,
        BinOp::Ge => BinOp::Le,
        _ => return None,
    })
}

fn intersect(a: (Bound<i64>, Bound<i64>), b: (Bound<i64>, Bound<i64>)) -> (Bound<i64>, Bound<i64>) {
    (tighter_lo(a.0, b.0), tighter_hi(a.1, b.1))
}

fn tighter_lo(a: Bound<i64>, b: Bound<i64>) -> Bound<i64> {
    match (a, b) {
        (Bound::Unbounded, x) | (x, Bound::Unbounded) => x,
        (Bound::Included(x), Bound::Included(y)) => Bound::Included(x.max(y)),
        (Bound::Excluded(x), Bound::Excluded(y)) => Bound::Excluded(x.max(y)),
        (Bound::Included(i), Bound::Excluded(e)) | (Bound::Excluded(e), Bound::Included(i)) => {
            if e >= i {
                Bound::Excluded(e)
            } else {
                Bound::Included(i)
            }
        }
    }
}

fn tighter_hi(a: Bound<i64>, b: Bound<i64>) -> Bound<i64> {
    match (a, b) {
        (Bound::Unbounded, x) | (x, Bound::Unbounded) => x,
        (Bound::Included(x), Bound::Included(y)) => Bound::Included(x.min(y)),
        (Bound::Excluded(x), Bound::Excluded(y)) => Bound::Excluded(x.min(y)),
        (Bound::Included(i), Bound::Excluded(e)) | (Bound::Excluded(e), Bound::Included(i)) => {
            if e <= i {
                Bound::Excluded(e)
            } else {
                Bound::Included(i)
            }
        }
    }
}

fn classify(lo: Bound<i64>, hi: Bound<i64>) -> Access {
    // Point lookup?
    if let (Bound::Included(a), Bound::Included(b)) = (lo, hi) {
        if a == b {
            return Access::PkLookup(a);
        }
    }
    // Provably empty?
    let lo_val = match lo {
        Bound::Included(v) => Some((v, true)),
        Bound::Excluded(v) => Some((v, false)),
        Bound::Unbounded => None,
    };
    let hi_val = match hi {
        Bound::Included(v) => Some((v, true)),
        Bound::Excluded(v) => Some((v, false)),
        Bound::Unbounded => None,
    };
    if let (Some((l, l_inc)), Some((h, h_inc))) = (lo_val, hi_val) {
        let empty = l > h || (l == h && !(l_inc && h_inc));
        if empty {
            return Access::Nothing;
        }
    }
    if lo == Bound::Unbounded && hi == Bound::Unbounded {
        return Access::FullScan;
    }
    Access::PkRange { lo, hi }
}

// --- EXPLAIN rendering ----------------------------------------------------

pub fn describe_access(plan: &Plan, table: &str) -> String {
    let pk = plan.pk_name.as_deref().unwrap_or("rowid");
    match &plan.access {
        Access::PkLookup(k) => {
            format!("PRIMARY KEY LOOKUP on {table} ({pk} = {k}) — one descent, O(log n)")
        }
        Access::PkRange { lo, hi } => {
            format!(
                "PRIMARY KEY RANGE SCAN on {table} ({}) — descend once, walk the leaf chain",
                range_text(pk, lo, hi)
            )
        }
        Access::FullScan => {
            format!("FULL SCAN of {table} — every leaf page will be read")
        }
        Access::Nothing => {
            format!("NO ROWS — the WHERE clause on {table} can never match (proved at plan time)")
        }
    }
}

fn range_text(pk: &str, lo: &Bound<i64>, hi: &Bound<i64>) -> String {
    let lo_s = match lo {
        Bound::Included(v) => format!("{v} <= "),
        Bound::Excluded(v) => format!("{v} < "),
        Bound::Unbounded => String::new(),
    };
    let hi_s = match hi {
        Bound::Included(v) => format!(" <= {v}"),
        Bound::Excluded(v) => format!(" < {v}"),
        Bound::Unbounded => String::new(),
    };
    format!("{lo_s}{pk}{hi_s}")
}

pub fn access_to_json(plan: &Plan) -> J {
    let pk = plan.pk_name.clone().unwrap_or_else(|| "rowid".to_string());
    match &plan.access {
        Access::PkLookup(k) => J::O(vec![
            ("type".into(), J::s("pk_lookup")),
            ("pk".into(), J::s(pk)),
            ("key".into(), J::I(*k)),
        ]),
        Access::PkRange { lo, hi } => J::O(vec![
            ("type".into(), J::s("pk_range")),
            ("pk".into(), J::s(pk)),
            ("lo".into(), bound_json(lo)),
            ("hi".into(), bound_json(hi)),
        ]),
        Access::FullScan => J::O(vec![("type".into(), J::s("full_scan"))]),
        Access::Nothing => J::O(vec![("type".into(), J::s("nothing"))]),
    }
}

fn bound_json(b: &Bound<i64>) -> J {
    match b {
        Bound::Included(v) => J::O(vec![
            ("value".into(), J::I(*v)),
            ("inclusive".into(), J::B(true)),
        ]),
        Bound::Excluded(v) => J::O(vec![
            ("value".into(), J::I(*v)),
            ("inclusive".into(), J::B(false)),
        ]),
        Bound::Unbounded => J::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::ast::Statement;
    use crate::sql::parser::parse_one;
    use crate::types::{ColType, Column};

    fn users() -> TableSchema {
        TableSchema {
            id: 1,
            name: "users".into(),
            root: 2,
            columns: vec![
                Column {
                    name: "id".into(),
                    ty: ColType::Int,
                    primary_key: true,
                },
                Column {
                    name: "age".into(),
                    ty: ColType::Int,
                    primary_key: false,
                },
            ],
        }
    }

    fn where_of(sql: &str) -> Option<Expr> {
        match parse_one(sql).unwrap() {
            Statement::Select(s) => s.where_clause,
            _ => panic!(),
        }
    }

    #[test]
    fn equality_becomes_lookup() {
        let p = plan(&users(), &where_of("SELECT * FROM users WHERE id = 7"));
        assert_eq!(p.access, Access::PkLookup(7));
    }

    #[test]
    fn flipped_literal_still_lookup() {
        let p = plan(&users(), &where_of("SELECT * FROM users WHERE 7 = id"));
        assert_eq!(p.access, Access::PkLookup(7));
    }

    #[test]
    fn and_intersects_ranges() {
        let p = plan(
            &users(),
            &where_of("SELECT * FROM users WHERE id > 10 AND id <= 20"),
        );
        assert_eq!(
            p.access,
            Access::PkRange {
                lo: Bound::Excluded(10),
                hi: Bound::Included(20)
            }
        );
    }

    #[test]
    fn impossible_range_is_nothing() {
        let p = plan(
            &users(),
            &where_of("SELECT * FROM users WHERE id > 10 AND id < 5"),
        );
        assert_eq!(p.access, Access::Nothing);
    }

    #[test]
    fn non_pk_predicate_is_full_scan() {
        let p = plan(&users(), &where_of("SELECT * FROM users WHERE age = 30"));
        assert_eq!(p.access, Access::FullScan);
    }

    #[test]
    fn or_cannot_use_the_index() {
        let p = plan(
            &users(),
            &where_of("SELECT * FROM users WHERE id = 1 OR id = 2"),
        );
        assert_eq!(p.access, Access::FullScan);
    }

    #[test]
    fn pk_constraint_mixed_with_other_filter() {
        let p = plan(
            &users(),
            &where_of("SELECT * FROM users WHERE id >= 5 AND age < 40"),
        );
        assert_eq!(
            p.access,
            Access::PkRange {
                lo: Bound::Included(5),
                hi: Bound::Unbounded
            }
        );
    }
}
