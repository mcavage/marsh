//! Strict JSON for request bodies (B1).
//!
//! Differences from `serde_json`, on purpose: duplicate object keys are an
//! error (Go keeps the last one), trailing data is an error (Go's decoder
//! ignores it), invalid UTF-8, a BOM, lone surrogates and control characters
//! in strings are errors, depth is bounded, and object key order is kept so
//! the schema layer can detect keys that differ only in case.

use std::fmt::Write as _;

pub const MAX_DEPTH: usize = 48;

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(serde_json::Number),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonError(pub &'static str);

impl std::fmt::Display for JsonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

/// Parse exactly one JSON value covering the whole input.
pub fn parse(input: &[u8]) -> Result<Json, JsonError> {
    let text = std::str::from_utf8(input).map_err(|_| JsonError("body is not valid UTF-8"))?;
    let mut p = Parser {
        b: text.as_bytes(),
        i: 0,
    };
    p.ws();
    let v = p.value(0)?;
    p.ws();
    if p.i != p.b.len() {
        return Err(JsonError("trailing data after the JSON value"));
    }
    Ok(v)
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while matches!(self.b.get(self.i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn value(&mut self, depth: usize) -> Result<Json, JsonError> {
        if depth > MAX_DEPTH {
            return Err(JsonError("JSON is nested too deeply"));
        }
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.literal(b"true", Json::Bool(true)),
            Some(b'f') => self.literal(b"false", Json::Bool(false)),
            Some(b'n') => self.literal(b"null", Json::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(JsonError("invalid JSON")),
        }
    }

    fn literal(&mut self, word: &[u8], v: Json) -> Result<Json, JsonError> {
        if self.b[self.i..].starts_with(word) {
            self.i += word.len();
            Ok(v)
        } else {
            Err(JsonError("invalid JSON"))
        }
    }

    fn number(&mut self) -> Result<Json, JsonError> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        match self.peek() {
            Some(b'0') => self.i += 1,
            Some(b'1'..=b'9') => self.digits(),
            _ => return Err(JsonError("invalid JSON number")),
        }
        if self.peek() == Some(b'.') {
            self.i += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(JsonError("invalid JSON number"));
            }
            self.digits();
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(JsonError("invalid JSON number"));
            }
            self.digits();
        }
        let text = std::str::from_utf8(&self.b[start..self.i])
            .map_err(|_| JsonError("invalid JSON number"))?;
        let n: serde_json::Number = text
            .parse()
            .map_err(|_| JsonError("JSON number out of range"))?;
        Ok(Json::Num(n))
    }

    fn digits(&mut self) {
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.i += 1;
        }
    }

    fn hex4(&mut self) -> Result<u32, JsonError> {
        let mut v = 0u32;
        for _ in 0..4 {
            let c = self.peek().ok_or(JsonError("invalid JSON escape"))?;
            let d = (c as char)
                .to_digit(16)
                .ok_or(JsonError("invalid JSON escape"))?;
            v = v * 16 + d;
            self.i += 1;
        }
        Ok(v)
    }

    fn string(&mut self) -> Result<String, JsonError> {
        self.i += 1; // opening quote
        let mut out = String::new();
        loop {
            let start = self.i;
            while let Some(c) = self.peek() {
                if c == b'"' || c == b'\\' || c < 0x20 {
                    break;
                }
                self.i += 1;
            }
            // The input is valid UTF-8 and we only stop on ASCII bytes, so
            // this slice is on character boundaries.
            out.push_str(
                std::str::from_utf8(&self.b[start..self.i])
                    .map_err(|_| JsonError("invalid UTF-8"))?,
            );
            match self.peek() {
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.i += 1;
                    let c = self.peek().ok_or(JsonError("invalid JSON escape"))?;
                    self.i += 1;
                    match c {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let cp = if (0xD800..0xDC00).contains(&hi) {
                                if !self.b[self.i..].starts_with(b"\\u") {
                                    return Err(JsonError("lone surrogate in JSON string"));
                                }
                                self.i += 2;
                                let lo = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    return Err(JsonError("lone surrogate in JSON string"));
                                }
                                0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                            } else if (0xDC00..0xE000).contains(&hi) {
                                return Err(JsonError("lone surrogate in JSON string"));
                            } else {
                                hi
                            };
                            out.push(char::from_u32(cp).ok_or(JsonError("invalid code point"))?);
                        }
                        _ => return Err(JsonError("invalid JSON escape")),
                    }
                }
                _ => return Err(JsonError("invalid JSON string")),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json, JsonError> {
        self.i += 1;
        let mut items = Vec::new();
        self.ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            self.ws();
            items.push(self.value(depth + 1)?);
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Json::Arr(items));
                }
                _ => return Err(JsonError("invalid JSON array")),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, JsonError> {
        self.i += 1;
        let mut members: Vec<(String, Json)> = Vec::new();
        self.ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Json::Obj(members));
        }
        loop {
            self.ws();
            if self.peek() != Some(b'"') {
                return Err(JsonError("invalid JSON object"));
            }
            let key = self.string()?;
            if members.iter().any(|(k, _)| *k == key) {
                return Err(JsonError("duplicate key in JSON object"));
            }
            self.ws();
            if self.peek() != Some(b':') {
                return Err(JsonError("invalid JSON object"));
            }
            self.i += 1;
            self.ws();
            let v = self.value(depth + 1)?;
            members.push((key, v));
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Json::Obj(members));
                }
                _ => return Err(JsonError("invalid JSON object")),
            }
        }
    }
}

impl Json {
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Json> {
        match self {
            Json::Obj(m) => m.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn set(&mut self, key: &str, v: Json) {
        if let Json::Obj(m) = self {
            if let Some(slot) = m.iter_mut().find(|(k, _)| k == key) {
                slot.1 = v;
            } else {
                m.push((key.to_string(), v));
            }
        }
    }

    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        if let Json::Str(s) = self {
            Some(s)
        } else {
            None
        }
    }

    #[must_use]
    pub fn as_i64(&self) -> Option<i64> {
        if let Json::Num(n) = self {
            n.as_i64()
        } else {
            None
        }
    }

    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    /// Compact serialization; the only form that is ever sent upstream.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut s = String::new();
        self.write(&mut s);
        s.into_bytes()
    }

    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Num(n) => {
                let _ = write!(out, "{n}");
            }
            Json::Str(s) => write_str(out, s),
            Json::Arr(a) => {
                out.push('[');
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.write(out);
                }
                out.push(']');
            }
            Json::Obj(m) => {
                out.push('{');
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_str(out, k);
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }
}

fn write_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[must_use]
pub fn string(s: &str) -> Json {
    Json::Str(s.to_string())
}

#[must_use]
pub fn int(n: i64) -> Json {
    Json::Num(n.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_and_roundtrips() {
        let v = parse(br#" {"a":[1,2.5,true,null,"x\u00e9\ud83d\ude00"],"b":{}} "#).unwrap();
        assert_eq!(
            String::from_utf8(v.to_bytes()).unwrap(),
            "{\"a\":[1,2.5,true,null,\"x\u{e9}\u{1f600}\"],\"b\":{}}"
        );
    }

    #[test]
    fn rejects_what_go_would_accept() {
        for (name, body) in [
            ("duplicate", &br#"{"a":1,"a":2}"#[..]),
            ("trailing", br#"{"a":1} xyz"#),
            ("two documents", br#"{"a":1}{"b":2}"#),
            ("bom", b"\xef\xbb\xbf{}"),
            ("utf16", b"{\0}\0"),
            ("invalid utf8", b"{\"a\":\"\xff\"}"),
            ("lone surrogate", br#"{"a":"\ud800"}"#),
            ("control char", b"{\"a\":\"\x01\"}"),
            ("leading zero", b"[01]"),
            ("trailing comma", b"[1,]"),
            ("empty", b""),
            ("huge number", b"[1e999]"),
        ] {
            assert!(parse(body).is_err(), "{name} must be rejected");
        }
    }

    #[test]
    fn depth_is_bounded() {
        let deep = "[".repeat(10_000) + &"]".repeat(10_000);
        assert!(parse(deep.as_bytes()).is_err());
        let ok = "[".repeat(40) + &"]".repeat(40);
        assert!(parse(ok.as_bytes()).is_ok());
    }
}
