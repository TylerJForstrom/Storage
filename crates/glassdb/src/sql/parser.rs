//! Recursive-descent SQL parser. Keywords are case-insensitive; errors say
//! what was expected and carry the byte position of the offending token.

use crate::errors::{DbError, DbResult};
use crate::sql::ast::*;
use crate::sql::lexer::{lex, Tok, Token};
use crate::types::{ColType, Value};

/// Words that can never be a table/column name, so that e.g. the expression
/// parser doesn't swallow FROM as a column reference.
const RESERVED: &[&str] = &[
    "SELECT", "FROM", "WHERE", "ORDER", "BY", "ASC", "DESC", "LIMIT", "INSERT", "INTO", "VALUES",
    "CREATE", "TABLE", "PRIMARY", "KEY", "DROP", "UPDATE", "SET", "DELETE", "BEGIN", "COMMIT",
    "ROLLBACK", "EXPLAIN", "AND", "OR", "NOT", "NULL", "TRUE", "FALSE", "AS",
];

fn is_reserved(word: &str) -> bool {
    RESERVED.iter().any(|r| r.eq_ignore_ascii_case(word))
}

pub fn parse_statements(sql: &str) -> DbResult<Vec<Statement>> {
    let tokens = lex(sql)?;
    let mut p = Parser { tokens, pos: 0 };
    let mut stmts = Vec::new();
    loop {
        while p.accept(&Tok::Semi) {}
        if matches!(p.peek().tok, Tok::Eof) {
            break;
        }
        stmts.push(p.parse_statement()?);
        if !matches!(p.peek().tok, Tok::Semi | Tok::Eof) {
            return Err(p.expected("';' between statements"));
        }
    }
    Ok(stmts)
}

/// Parse exactly one statement (a trailing ';' is fine).
pub fn parse_one(sql: &str) -> DbResult<Statement> {
    let mut stmts = parse_statements(sql)?;
    match stmts.len() {
        0 => Err(DbError::syntax("no statement found", 0)),
        1 => Ok(stmts.pop().unwrap()),
        n => Err(DbError::syntax(
            format!("expected one statement, found {n} (run them one at a time)"),
            0,
        )),
    }
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn bump(&mut self) -> Token {
        let t = self.tokens[self.pos].clone();
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        t
    }

    fn accept(&mut self, tok: &Tok) -> bool {
        if &self.peek().tok == tok {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, tok: &Tok, what: &str) -> DbResult<()> {
        if self.accept(tok) {
            Ok(())
        } else {
            Err(self.expected(what))
        }
    }

    fn peek_kw(&self, kw: &str) -> bool {
        matches!(&self.peek().tok, Tok::Ident(s) if s.eq_ignore_ascii_case(kw))
    }

    fn accept_kw(&mut self, kw: &str) -> bool {
        if self.peek_kw(kw) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect_kw(&mut self, kw: &str) -> DbResult<()> {
        if self.accept_kw(kw) {
            Ok(())
        } else {
            Err(self.expected(&format!("keyword {kw}")))
        }
    }

    fn expected(&self, what: &str) -> DbError {
        let t = self.peek();
        DbError::syntax(
            format!("expected {what}, found {}", t.tok.describe()),
            t.pos,
        )
    }

    /// A table or column name: any identifier that isn't a reserved word.
    fn name(&mut self, what: &str) -> DbResult<String> {
        match &self.peek().tok {
            Tok::Ident(s) if !is_reserved(s) => {
                let s = s.clone();
                self.bump();
                Ok(s)
            }
            Tok::Ident(s) => Err(DbError::syntax(
                format!("'{s}' is a keyword and can't be used as {what}"),
                self.peek().pos,
            )),
            _ => Err(self.expected(what)),
        }
    }

    fn parse_statement(&mut self) -> DbResult<Statement> {
        if self.accept_kw("SELECT") {
            return Ok(Statement::Select(self.parse_select_body()?));
        }
        if self.accept_kw("INSERT") {
            return self.parse_insert();
        }
        if self.accept_kw("CREATE") {
            self.expect_kw("TABLE")?;
            return self.parse_create_table();
        }
        if self.accept_kw("DROP") {
            self.expect_kw("TABLE")?;
            return Ok(Statement::DropTable {
                name: self.name("a table name")?,
            });
        }
        if self.accept_kw("UPDATE") {
            return self.parse_update();
        }
        if self.accept_kw("DELETE") {
            self.expect_kw("FROM")?;
            let table = self.name("a table name")?;
            let where_clause = self.parse_optional_where()?;
            return Ok(Statement::Delete {
                table,
                where_clause,
            });
        }
        if self.accept_kw("BEGIN") {
            return Ok(Statement::Begin);
        }
        if self.accept_kw("COMMIT") {
            return Ok(Statement::Commit);
        }
        if self.accept_kw("ROLLBACK") {
            return Ok(Statement::Rollback);
        }
        if self.accept_kw("EXPLAIN") {
            let inner = self.parse_statement()?;
            return Ok(Statement::Explain(Box::new(inner)));
        }
        Err(self.expected(
            "a statement (SELECT, INSERT, CREATE TABLE, UPDATE, DELETE, DROP TABLE, \
             BEGIN, COMMIT, ROLLBACK, or EXPLAIN)",
        ))
    }

    fn parse_create_table(&mut self) -> DbResult<Statement> {
        let name = self.name("a table name")?;
        self.expect(&Tok::LParen, "'(' before the column list")?;
        let mut columns = Vec::new();
        loop {
            let col_name = self.name("a column name")?;
            let ty_token = self.peek().clone();
            let ty = match &ty_token.tok {
                Tok::Ident(word) => ColType::from_keyword(word).ok_or_else(|| {
                    DbError::syntax(
                        format!("unknown type '{word}' — use INTEGER, REAL, TEXT, or BOOLEAN"),
                        ty_token.pos,
                    )
                })?,
                _ => return Err(self.expected("a column type (INTEGER, REAL, TEXT, BOOLEAN)")),
            };
            self.bump();
            let mut primary_key = false;
            if self.accept_kw("PRIMARY") {
                self.expect_kw("KEY")?;
                primary_key = true;
            }
            columns.push(ColumnDef {
                name: col_name,
                ty,
                primary_key,
            });
            if self.accept(&Tok::Comma) {
                continue;
            }
            self.expect(&Tok::RParen, "')' after the column list")?;
            break;
        }
        Ok(Statement::CreateTable { name, columns })
    }

    fn parse_insert(&mut self) -> DbResult<Statement> {
        self.expect_kw("INTO")?;
        let table = self.name("a table name")?;
        let columns = if self.accept(&Tok::LParen) {
            let mut cols = Vec::new();
            loop {
                cols.push(self.name("a column name")?);
                if self.accept(&Tok::Comma) {
                    continue;
                }
                self.expect(&Tok::RParen, "')' after the column list")?;
                break;
            }
            Some(cols)
        } else {
            None
        };
        self.expect_kw("VALUES")?;
        let mut rows = Vec::new();
        loop {
            self.expect(&Tok::LParen, "'(' before a row of values")?;
            let mut row = Vec::new();
            loop {
                row.push(self.parse_expr()?);
                if self.accept(&Tok::Comma) {
                    continue;
                }
                self.expect(&Tok::RParen, "')' after a row of values")?;
                break;
            }
            rows.push(row);
            if !self.accept(&Tok::Comma) {
                break;
            }
        }
        Ok(Statement::Insert {
            table,
            columns,
            rows,
        })
    }

    fn parse_select_body(&mut self) -> DbResult<SelectStmt> {
        let mut items = Vec::new();
        loop {
            if self.accept(&Tok::Star) {
                items.push(SelectItem {
                    expr: SelectExpr::Star,
                    alias: None,
                });
            } else {
                let expr = self.parse_expr()?;
                let alias = if self.accept_kw("AS") {
                    Some(self.name("an alias after AS")?)
                } else {
                    None
                };
                items.push(SelectItem {
                    expr: SelectExpr::Expr(expr),
                    alias,
                });
            }
            if !self.accept(&Tok::Comma) {
                break;
            }
        }
        self.expect_kw("FROM")?;
        let table = self.name("a table name")?;
        let where_clause = self.parse_optional_where()?;
        let order_by = if self.accept_kw("ORDER") {
            self.expect_kw("BY")?;
            let col = self.name("a column name to order by")?;
            let asc = if self.accept_kw("DESC") {
                false
            } else {
                self.accept_kw("ASC");
                true
            };
            Some((col, asc))
        } else {
            None
        };
        let limit = if self.accept_kw("LIMIT") {
            match &self.peek().tok {
                Tok::Int(n) if *n >= 0 => {
                    let n = *n as u64;
                    self.bump();
                    Some(n)
                }
                _ => return Err(self.expected("a non-negative integer after LIMIT")),
            }
        } else {
            None
        };
        Ok(SelectStmt {
            items,
            table,
            where_clause,
            order_by,
            limit,
        })
    }

    fn parse_update(&mut self) -> DbResult<Statement> {
        let table = self.name("a table name")?;
        self.expect_kw("SET")?;
        let mut sets = Vec::new();
        loop {
            let col = self.name("a column name")?;
            self.expect(&Tok::Eq, "'=' after the column name")?;
            sets.push((col, self.parse_expr()?));
            if !self.accept(&Tok::Comma) {
                break;
            }
        }
        let where_clause = self.parse_optional_where()?;
        Ok(Statement::Update {
            table,
            sets,
            where_clause,
        })
    }

    fn parse_optional_where(&mut self) -> DbResult<Option<Expr>> {
        if self.accept_kw("WHERE") {
            Ok(Some(self.parse_expr()?))
        } else {
            Ok(None)
        }
    }

    // --- expressions, by precedence: OR < AND < NOT < comparison < +- < */ ---

    fn parse_expr(&mut self) -> DbResult<Expr> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> DbResult<Expr> {
        let mut left = self.parse_and()?;
        while self.accept_kw("OR") {
            let right = self.parse_and()?;
            left = Expr::Binary {
                op: BinOp::Or,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> DbResult<Expr> {
        let mut left = self.parse_not()?;
        while self.accept_kw("AND") {
            let right = self.parse_not()?;
            left = Expr::Binary {
                op: BinOp::And,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> DbResult<Expr> {
        if self.accept_kw("NOT") {
            let inner = self.parse_not()?;
            return Ok(Expr::Unary {
                op: UnOp::Not,
                expr: Box::new(inner),
            });
        }
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> DbResult<Expr> {
        let left = self.parse_additive()?;
        let op = match self.peek().tok {
            Tok::Eq => Some(BinOp::Eq),
            Tok::Ne => Some(BinOp::Ne),
            Tok::Lt => Some(BinOp::Lt),
            Tok::Le => Some(BinOp::Le),
            Tok::Gt => Some(BinOp::Gt),
            Tok::Ge => Some(BinOp::Ge),
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let right = self.parse_additive()?;
            return Ok(Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            });
        }
        Ok(left)
    }

    fn parse_additive(&mut self) -> DbResult<Expr> {
        let mut left = self.parse_multiplicative()?;
        loop {
            let op = match self.peek().tok {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                _ => break,
            };
            self.bump();
            let right = self.parse_multiplicative()?;
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_multiplicative(&mut self) -> DbResult<Expr> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek().tok {
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                _ => break,
            };
            self.bump();
            let right = self.parse_unary()?;
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> DbResult<Expr> {
        if self.accept(&Tok::Minus) {
            let inner = self.parse_unary()?;
            // Fold -<number> into a literal so "-5" plans as a constant.
            return Ok(match inner {
                Expr::Literal(Value::Int(n)) => Expr::Literal(Value::Int(-n)),
                Expr::Literal(Value::Real(f)) => Expr::Literal(Value::Real(-f)),
                other => Expr::Unary {
                    op: UnOp::Neg,
                    expr: Box::new(other),
                },
            });
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> DbResult<Expr> {
        let token = self.peek().clone();
        match &token.tok {
            Tok::Int(n) => {
                self.bump();
                Ok(Expr::Literal(Value::Int(*n)))
            }
            Tok::Float(f) => {
                self.bump();
                Ok(Expr::Literal(Value::Real(*f)))
            }
            Tok::Str(s) => {
                self.bump();
                Ok(Expr::Literal(Value::Text(s.clone())))
            }
            Tok::LParen => {
                self.bump();
                let inner = self.parse_expr()?;
                self.expect(&Tok::RParen, "')' to close the parenthesis")?;
                Ok(inner)
            }
            Tok::Ident(word) => {
                if word.eq_ignore_ascii_case("NULL") {
                    self.bump();
                    return Ok(Expr::Literal(Value::Null));
                }
                if word.eq_ignore_ascii_case("TRUE") {
                    self.bump();
                    return Ok(Expr::Literal(Value::Bool(true)));
                }
                if word.eq_ignore_ascii_case("FALSE") {
                    self.bump();
                    return Ok(Expr::Literal(Value::Bool(false)));
                }
                if is_reserved(word) {
                    return Err(DbError::syntax(
                        format!("expected an expression, found keyword '{word}'"),
                        token.pos,
                    ));
                }
                let name = word.clone();
                self.bump();
                if self.accept(&Tok::LParen) {
                    // Function call: COUNT(*), SUM(x), ...
                    if self.accept(&Tok::Star) {
                        self.expect(&Tok::RParen, "')' after '*'")?;
                        return Ok(Expr::Call {
                            func: name,
                            arg: None,
                            star: true,
                        });
                    }
                    if self.accept(&Tok::RParen) {
                        return Err(DbError::syntax(
                            format!("{name}() needs an argument (or use {name}(*))",),
                            token.pos,
                        ));
                    }
                    let arg = self.parse_expr()?;
                    self.expect(&Tok::RParen, "')' after the function argument")?;
                    return Ok(Expr::Call {
                        func: name,
                        arg: Some(Box::new(arg)),
                        star: false,
                    });
                }
                Ok(Expr::Column(name))
            }
            _ => Err(self.expected("an expression")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_select() {
        let stmt = parse_one(
            "SELECT name, age * 2 AS doubled FROM users \
             WHERE age >= 21 AND city = 'NYC' ORDER BY age DESC LIMIT 10;",
        )
        .unwrap();
        let Statement::Select(s) = stmt else {
            panic!("not a select")
        };
        assert_eq!(s.items.len(), 2);
        assert_eq!(s.table, "users");
        assert!(s.where_clause.is_some());
        assert_eq!(s.order_by, Some(("age".into(), false)));
        assert_eq!(s.limit, Some(10));
    }

    #[test]
    fn parses_multi_row_insert() {
        let stmt = parse_one("INSERT INTO t (a, b) VALUES (1, 'x'), (2, 'y')").unwrap();
        let Statement::Insert { rows, columns, .. } = stmt else {
            panic!()
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(columns.unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn error_carries_position() {
        let err = parse_one("SELECT FROM users").unwrap_err();
        assert_eq!(err.position, Some(7)); // points at FROM
        assert!(err.message.contains("expected an expression"));
    }

    #[test]
    fn keyword_cannot_be_table_name() {
        let err = parse_one("CREATE TABLE select (id INTEGER)").unwrap_err();
        assert!(err.message.contains("keyword"));
    }

    #[test]
    fn explain_wraps_statement() {
        let stmt = parse_one("EXPLAIN SELECT * FROM t WHERE id = 5").unwrap();
        assert!(matches!(stmt, Statement::Explain(_)));
    }
}
