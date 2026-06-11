//! The lexer: SQL text -> tokens. Every token remembers its byte position
//! so later stages can point at the exact spot in an error message.

use crate::errors::{DbError, DbResult};

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Ident(String),
    Int(i64),
    Float(f64),
    Str(String),
    LParen,
    RParen,
    Comma,
    Semi,
    Star, // '*': multiplication or SELECT * — the parser decides
    Plus,
    Minus,
    Slash,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Eof,
}

impl Tok {
    /// Human description for error messages.
    pub fn describe(&self) -> String {
        match self {
            Tok::Ident(s) => format!("'{s}'"),
            Tok::Int(i) => format!("number {i}"),
            Tok::Float(f) => format!("number {f}"),
            Tok::Str(_) => "a string".to_string(),
            Tok::LParen => "'('".to_string(),
            Tok::RParen => "')'".to_string(),
            Tok::Comma => "','".to_string(),
            Tok::Semi => "';'".to_string(),
            Tok::Star => "'*'".to_string(),
            Tok::Plus => "'+'".to_string(),
            Tok::Minus => "'-'".to_string(),
            Tok::Slash => "'/'".to_string(),
            Tok::Eq => "'='".to_string(),
            Tok::Ne => "'!='".to_string(),
            Tok::Lt => "'<'".to_string(),
            Tok::Le => "'<='".to_string(),
            Tok::Gt => "'>'".to_string(),
            Tok::Ge => "'>='".to_string(),
            Tok::Eof => "end of input".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Token {
    pub tok: Tok,
    pub pos: usize,
}

pub fn lex(sql: &str) -> DbResult<Vec<Token>> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'-' if i + 1 < bytes.len() && bytes[i + 1] == b'-' => {
                // -- comment to end of line
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'(' => {
                tokens.push(Token {
                    tok: Tok::LParen,
                    pos: i,
                });
                i += 1;
            }
            b')' => {
                tokens.push(Token {
                    tok: Tok::RParen,
                    pos: i,
                });
                i += 1;
            }
            b',' => {
                tokens.push(Token {
                    tok: Tok::Comma,
                    pos: i,
                });
                i += 1;
            }
            b';' => {
                tokens.push(Token {
                    tok: Tok::Semi,
                    pos: i,
                });
                i += 1;
            }
            b'*' => {
                tokens.push(Token {
                    tok: Tok::Star,
                    pos: i,
                });
                i += 1;
            }
            b'+' => {
                tokens.push(Token {
                    tok: Tok::Plus,
                    pos: i,
                });
                i += 1;
            }
            b'-' => {
                tokens.push(Token {
                    tok: Tok::Minus,
                    pos: i,
                });
                i += 1;
            }
            b'/' => {
                tokens.push(Token {
                    tok: Tok::Slash,
                    pos: i,
                });
                i += 1;
            }
            b'=' => {
                tokens.push(Token {
                    tok: Tok::Eq,
                    pos: i,
                });
                i += 1;
            }
            b'!' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    tokens.push(Token {
                        tok: Tok::Ne,
                        pos: i,
                    });
                    i += 2;
                } else {
                    return Err(DbError::syntax("expected '=' after '!'", i));
                }
            }
            b'<' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    tokens.push(Token {
                        tok: Tok::Le,
                        pos: i,
                    });
                    i += 2;
                } else if i + 1 < bytes.len() && bytes[i + 1] == b'>' {
                    tokens.push(Token {
                        tok: Tok::Ne,
                        pos: i,
                    }); // <> also means !=
                    i += 2;
                } else {
                    tokens.push(Token {
                        tok: Tok::Lt,
                        pos: i,
                    });
                    i += 1;
                }
            }
            b'>' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    tokens.push(Token {
                        tok: Tok::Ge,
                        pos: i,
                    });
                    i += 2;
                } else {
                    tokens.push(Token {
                        tok: Tok::Gt,
                        pos: i,
                    });
                    i += 1;
                }
            }
            b'\'' => {
                // String literal; '' inside is an escaped quote.
                let start = i;
                i += 1;
                let mut s = String::new();
                loop {
                    if i >= bytes.len() {
                        return Err(DbError::syntax(
                            "string starts here but never closes (missing ')",
                            start,
                        ));
                    }
                    if bytes[i] == b'\'' {
                        if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                            s.push('\'');
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        // Copy a full UTF-8 character, not just a byte.
                        let ch_len = utf8_len(bytes[i]);
                        s.push_str(&sql[i..i + ch_len]);
                        i += ch_len;
                    }
                }
                tokens.push(Token {
                    tok: Tok::Str(s),
                    pos: start,
                });
            }
            b'0'..=b'9' => {
                let start = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                let is_float =
                    i + 1 < bytes.len() && bytes[i] == b'.' && bytes[i + 1].is_ascii_digit();
                if is_float {
                    i += 1;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                    let text = &sql[start..i];
                    let f: f64 = text
                        .parse()
                        .map_err(|_| DbError::syntax(format!("bad number '{text}'"), start))?;
                    tokens.push(Token {
                        tok: Tok::Float(f),
                        pos: start,
                    });
                } else {
                    let text = &sql[start..i];
                    let n: i64 = text.parse().map_err(|_| {
                        DbError::syntax(
                            format!("integer '{text}' is too large for a 64-bit value"),
                            start,
                        )
                    })?;
                    tokens.push(Token {
                        tok: Tok::Int(n),
                        pos: start,
                    });
                }
            }
            b'A'..=b'Z' | b'a'..=b'z' | b'_' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                tokens.push(Token {
                    tok: Tok::Ident(sql[start..i].to_string()),
                    pos: start,
                });
            }
            _ => {
                let ch_len = utf8_len(c);
                return Err(DbError::syntax(
                    format!("unexpected character '{}'", &sql[i..i + ch_len]),
                    i,
                ));
            }
        }
    }
    tokens.push(Token {
        tok: Tok::Eof,
        pos: sql.len(),
    });
    Ok(tokens)
}

fn utf8_len(first_byte: u8) -> usize {
    match first_byte {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexes_a_select() {
        let toks = lex("SELECT * FROM users WHERE age >= 21;").unwrap();
        let kinds: Vec<&Tok> = toks.iter().map(|t| &t.tok).collect();
        assert!(matches!(kinds[0], Tok::Ident(s) if s == "SELECT"));
        assert!(matches!(kinds[1], Tok::Star));
        assert!(matches!(kinds[6], Tok::Ge));
        assert!(matches!(kinds.last().unwrap(), Tok::Eof));
    }

    #[test]
    fn string_escapes_and_unicode() {
        let toks = lex("'it''s héré'").unwrap();
        assert!(matches!(&toks[0].tok, Tok::Str(s) if s == "it's héré"));
    }

    #[test]
    fn unterminated_string_points_at_opening_quote() {
        let err = lex("SELECT 'oops").unwrap_err();
        assert_eq!(err.position, Some(7));
    }

    #[test]
    fn comments_are_skipped() {
        let toks = lex("1 -- the answer\n2").unwrap();
        assert!(matches!(toks[0].tok, Tok::Int(1)));
        assert!(matches!(toks[1].tok, Tok::Int(2)));
    }
}
