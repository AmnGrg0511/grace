//! A tiny, dependency-free JSON value, parser, and serializer.
//!
//! This exists so the crate needs **zero external dependencies**. It supports
//! the subset of JSON required to talk to OpenAI-compatible APIs: objects,
//! arrays, strings (with the standard escapes), numbers, booleans, and null.
//! Object key order is preserved (stored as an ordered `Vec`), which keeps
//! request payloads stable and diff-friendly.

use std::fmt::Write as _;

/// A JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Json>),
    /// Object stored as an ordered list of `(key, value)` pairs.
    Object(Vec<(String, Json)>),
}

impl Json {
    /// Look up a key in an object. Returns `None` for non-objects or missing keys.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// All key/value pairs (empty for non-objects).
    pub fn as_object(&self) -> &[(String, Json)] {
        match self {
            Json::Object(pairs) => pairs,
            _ => &[],
        }
    }

    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Serialize to a compact JSON string.
    pub fn to_string_compact(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Number(n) => {
                if n.fract() == 0.0 && n.abs() < 1e15 {
                    let _ = write!(out, "{}", *n as i64);
                } else {
                    let _ = write!(out, "{}", n);
                }
            }
            Json::String(s) => write_json_string(s, out),
            Json::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Json::Object(pairs) => {
                out.push('{');
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_json_string(k, out);
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }
}

fn write_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Parse a JSON value from a string slice.
pub fn parse(input: &str) -> Result<Json, String> {
    let chars: Vec<char> = input.chars().collect();
    let mut p = Parser { chars, pos: 0 };
    p.skip_ws();
    let value = p.parse_value()?;
    p.skip_ws();
    if p.pos != p.chars.len() {
        return Err(format!("trailing characters at position {}", p.pos));
    }
    Ok(value)
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.chars.get(self.pos).copied();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.pos += 1;
        }
    }

    fn parse_value(&mut self) -> Result<Json, String> {
        self.skip_ws();
        match self.peek() {
            Some('{') => self.parse_object(),
            Some('[') => self.parse_array(),
            Some('"') => Ok(Json::String(self.parse_string()?)),
            Some('t') | Some('f') => self.parse_bool(),
            Some('n') => self.parse_null(),
            Some(c) if c == '-' || c.is_ascii_digit() => self.parse_number(),
            Some(c) => Err(format!("unexpected character '{c}' at position {}", self.pos)),
            None => Err("unexpected end of input".to_string()),
        }
    }

    fn parse_object(&mut self) -> Result<Json, String> {
        self.expect('{')?;
        let mut pairs = Vec::new();
        self.skip_ws();
        if self.peek() == Some('}') {
            self.pos += 1;
            return Ok(Json::Object(pairs));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some('"') {
                return Err(format!("expected object key string at position {}", self.pos));
            }
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect(':')?;
            let value = self.parse_value()?;
            pairs.push((key, value));
            self.skip_ws();
            match self.bump() {
                Some(',') => continue,
                Some('}') => break,
                _ => return Err(format!("expected ',' or '}}' at position {}", self.pos)),
            }
        }
        Ok(Json::Object(pairs))
    }

    fn parse_array(&mut self) -> Result<Json, String> {
        self.expect('[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(']') {
            self.pos += 1;
            return Ok(Json::Array(items));
        }
        loop {
            let value = self.parse_value()?;
            items.push(value);
            self.skip_ws();
            match self.bump() {
                Some(',') => continue,
                Some(']') => break,
                _ => return Err(format!("expected ',' or ']' at position {}", self.pos)),
            }
        }
        Ok(Json::Array(items))
    }

    fn parse_string(&mut self) -> Result<String, String> {
        self.expect('"')?;
        let mut s = String::new();
        while let Some(c) = self.bump() {
            match c {
                '"' => return Ok(s),
                '\\' => {
                    let esc = self
                        .bump()
                        .ok_or_else(|| "unterminated escape sequence".to_string())?;
                    match esc {
                        '"' => s.push('"'),
                        '\\' => s.push('\\'),
                        '/' => s.push('/'),
                        'b' => s.push('\u{08}'),
                        'f' => s.push('\u{0c}'),
                        'n' => s.push('\n'),
                        'r' => s.push('\r'),
                        't' => s.push('\t'),
                        'u' => {
                            let cp = self.parse_hex4()?;
                            // Handle surrogate pairs.
                            if (0xD800..=0xDBFF).contains(&cp) {
                                // Expect low surrogate.
                                if self.bump() != Some('\\') || self.bump() != Some('u') {
                                    return Err("invalid surrogate pair".to_string());
                                }
                                let lo = self.parse_hex4()?;
                                if !(0xDC00..=0xDFFF).contains(&lo) {
                                    return Err("invalid low surrogate".to_string());
                                }
                                let c = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                s.push(
                                    char::from_u32(c)
                                        .ok_or_else(|| "invalid unicode scalar".to_string())?,
                                );
                            } else {
                                s.push(
                                    char::from_u32(cp)
                                        .ok_or_else(|| "invalid unicode scalar".to_string())?,
                                );
                            }
                        }
                        other => return Err(format!("invalid escape '\\{other}'")),
                    }
                }
                c if (c as u32) < 0x20 => {
                    return Err("unescaped control character in string".to_string())
                }
                c => s.push(c),
            }
        }
        Err("unterminated string".to_string())
    }

    fn parse_hex4(&mut self) -> Result<u32, String> {
        let mut v = 0u32;
        for _ in 0..4 {
            let c = self
                .bump()
                .ok_or_else(|| "unexpected end of unicode escape".to_string())?;
            let d = c.to_digit(16).ok_or_else(|| "invalid hex digit".to_string())?;
            v = v * 16 + d;
        }
        Ok(v)
    }

    fn parse_bool(&mut self) -> Result<Json, String> {
        if self.match_literal("true") {
            Ok(Json::Bool(true))
        } else if self.match_literal("false") {
            Ok(Json::Bool(false))
        } else {
            Err(format!("invalid literal at position {}", self.pos))
        }
    }

    fn parse_null(&mut self) -> Result<Json, String> {
        if self.match_literal("null") {
            Ok(Json::Null)
        } else {
            Err(format!("invalid literal at position {}", self.pos))
        }
    }

    fn match_literal(&mut self, lit: &str) -> bool {
        let lit_chars: Vec<char> = lit.chars().collect();
        if self.pos + lit_chars.len() > self.chars.len() {
            return false;
        }
        let matches = self.chars[self.pos..self.pos + lit_chars.len()] == lit_chars;
        if matches {
            self.pos += lit_chars.len();
        }
        matches
    }

    fn parse_number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        if self.peek() == Some('-') {
            self.pos += 1;
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-') {
                self.pos += 1;
            } else {
                break;
            }
        }
        let s: String = self.chars[start..self.pos].iter().collect();
        s.parse::<f64>()
            .map(Json::Number)
            .map_err(|_| format!("invalid number '{s}'"))
    }

    fn expect(&mut self, c: char) -> Result<(), String> {
        if self.bump() == Some(c) {
            Ok(())
        } else {
            Err(format!("expected '{c}' at position {}", self.pos))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_object() {
        let src = r#"{"name":"grace","count":3,"enabled":true,"nested":{"x":[1,2,null]}}"#;
        let v = parse(src).unwrap();
        assert_eq!(v.get("name").and_then(Json::as_str), Some("grace"));
        assert_eq!(v.get("count"), Some(&Json::Number(3.0)));
        assert_eq!(v.get("enabled"), Some(&Json::Bool(true)));
        assert_eq!(v.to_string_compact(), src);
    }

    #[test]
    fn escapes_and_unicode() {
        let src = r#"{"s":"line\n\"q\"\tend","u":"\u00e9\ud83d\ude00"}"#;
        let v = parse(src).unwrap();
        assert_eq!(v.get("s").and_then(Json::as_str), Some("line\n\"q\"\tend"));
        assert_eq!(v.get("u").and_then(Json::as_str), Some("é😀"));
    }

    #[test]
    fn rejects_trailing() {
        assert!(parse(r#"{} x"#).is_err());
    }
}
