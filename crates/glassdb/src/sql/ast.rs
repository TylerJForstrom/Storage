//! The abstract syntax tree the parser produces and the planner consumes.

use crate::types::{ColType, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Add,
    Sub,
    Mul,
    Div,
}

impl BinOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinOp::Eq => "=",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::And => "AND",
            BinOp::Or => "OR",
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
        }
    }
}

#[derive(Debug, Clone)]
pub enum Expr {
    Literal(Value),
    Column(String),
    Unary {
        op: UnOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    /// COUNT(*) / COUNT(x) / SUM(x) / AVG(x) / MIN(x) / MAX(x).
    Call {
        func: String,
        arg: Option<Box<Expr>>,
        star: bool,
    },
}

impl Expr {
    /// Render back to SQL-ish text — used for column headers and EXPLAIN.
    pub fn display(&self) -> String {
        match self {
            Expr::Literal(Value::Text(s)) => format!("'{}'", s.replace('\'', "''")),
            Expr::Literal(v) => v.to_string(),
            Expr::Column(name) => name.clone(),
            Expr::Unary {
                op: UnOp::Neg,
                expr,
            } => format!("-{}", expr.display_atom()),
            Expr::Unary {
                op: UnOp::Not,
                expr,
            } => format!("NOT {}", expr.display_atom()),
            Expr::Binary { op, left, right } => {
                format!(
                    "{} {} {}",
                    left.display_atom(),
                    op.symbol(),
                    right.display_atom()
                )
            }
            Expr::Call { func, arg, star } => {
                let inner = if *star {
                    "*".to_string()
                } else {
                    arg.as_ref().map(|a| a.display()).unwrap_or_default()
                };
                format!("{}({})", func.to_ascii_uppercase(), inner)
            }
        }
    }

    fn display_atom(&self) -> String {
        match self {
            Expr::Binary { .. } => format!("({})", self.display()),
            _ => self.display(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ColumnDef {
    pub name: String,
    pub ty: ColType,
    pub primary_key: bool,
}

#[derive(Debug, Clone)]
pub enum SelectExpr {
    Star,
    Expr(Expr),
}

#[derive(Debug, Clone)]
pub struct SelectItem {
    pub expr: SelectExpr,
    pub alias: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SelectStmt {
    pub items: Vec<SelectItem>,
    pub table: String,
    pub where_clause: Option<Expr>,
    /// (column name, ascending)
    pub order_by: Option<(String, bool)>,
    pub limit: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum Statement {
    CreateTable {
        name: String,
        columns: Vec<ColumnDef>,
    },
    DropTable {
        name: String,
    },
    Insert {
        table: String,
        columns: Option<Vec<String>>,
        rows: Vec<Vec<Expr>>,
    },
    Select(SelectStmt),
    Update {
        table: String,
        sets: Vec<(String, Expr)>,
        where_clause: Option<Expr>,
    },
    Delete {
        table: String,
        where_clause: Option<Expr>,
    },
    Begin,
    Commit,
    Rollback,
    Explain(Box<Statement>),
}
