//! A minimal JSON encoder, written from scratch (no serde).
//!
//! The engine reports query results, plans, and trace events as JSON so the
//! CLI, tests, and the browser visualizer all consume the same format.

#[derive(Debug, Clone)]
pub enum J {
    Null,
    B(bool),
    I(i64),
    F(f64),
    S(String),
    A(Vec<J>),
    O(Vec<(String, J)>),
}

impl J {
    pub fn s(v: impl Into<String>) -> J {
        J::S(v.into())
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            J::Null => out.push_str("null"),
            J::B(b) => out.push_str(if *b { "true" } else { "false" }),
            J::I(i) => out.push_str(&i.to_string()),
            J::F(f) => {
                if f.is_finite() {
                    out.push_str(&f.to_string());
                } else {
                    // JSON has no NaN/Infinity; null is the least-bad choice.
                    out.push_str("null");
                }
            }
            J::S(s) => write_escaped(s, out),
            J::A(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            J::O(pairs) => {
                out.push('{');
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_escaped(k, out);
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }
}

fn write_escaped(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_nested_structures() {
        let v = J::O(vec![
            ("name".into(), J::s("glass\"db")),
            ("pages".into(), J::A(vec![J::I(1), J::I(2)])),
            ("ok".into(), J::B(true)),
            ("missing".into(), J::Null),
        ]);
        assert_eq!(
            v.render(),
            r#"{"name":"glass\"db","pages":[1,2],"ok":true,"missing":null}"#
        );
    }

    #[test]
    fn escapes_control_characters() {
        assert_eq!(J::s("a\nb\u{1}").render(), "\"a\\nb\\u0001\"");
    }
}
