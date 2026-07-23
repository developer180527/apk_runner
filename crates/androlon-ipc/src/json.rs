//! A deliberately tiny JSON dialect for the suite's IPC: one flat object,
//! values limited to strings / integers / booleans / arrays of strings.
//! Hand-rolled (std-only); both ends are ours, and the shape is fixed enough
//! that a future native shell can still use a full JSON parser against it.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    Int(i64),
    Bool(bool),
    List(Vec<String>),
}

/// Encode a flat object as one JSON line (no trailing newline).
pub fn encode(obj: &BTreeMap<String, Value>) -> String {
    let mut out = String::from("{");
    for (i, (k, v)) in obj.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&quote(k));
        out.push(':');
        match v {
            Value::Str(s) => out.push_str(&quote(s)),
            Value::Int(n) => out.push_str(&n.to_string()),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::List(items) => {
                out.push('[');
                for (j, item) in items.iter().enumerate() {
                    if j > 0 {
                        out.push(',');
                    }
                    out.push_str(&quote(item));
                }
                out.push(']');
            }
        }
    }
    out.push('}');
    out
}

fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Decode one flat object. Returns None on anything outside the dialect.
pub fn decode(line: &str) -> Option<BTreeMap<String, Value>> {
    let mut p = Parser { bytes: line.as_bytes(), pos: 0 };
    p.skip_ws();
    p.expect(b'{')?;
    let mut obj = BTreeMap::new();
    p.skip_ws();
    if p.peek() == Some(b'}') {
        return Some(obj);
    }
    loop {
        p.skip_ws();
        let key = p.string()?;
        p.skip_ws();
        p.expect(b':')?;
        p.skip_ws();
        let value = p.value()?;
        obj.insert(key, value);
        p.skip_ws();
        match p.next() {
            Some(b',') => continue,
            Some(b'}') => return Some(obj),
            _ => return None,
        }
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }

    fn expect(&mut self, b: u8) -> Option<()> {
        (self.next()? == b).then_some(())
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.pos += 1;
        }
    }

    fn string(&mut self) -> Option<String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            match self.next()? {
                b'"' => return Some(out),
                b'\\' => match self.next()? {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        let mut code = 0u32;
                        for _ in 0..4 {
                            let d = self.next()?;
                            code = code * 16 + (d as char).to_digit(16)?;
                        }
                        out.push(char::from_u32(code)?);
                    }
                    _ => return None,
                },
                b => {
                    // Re-assemble UTF-8: collect continuation bytes.
                    if b < 0x80 {
                        out.push(b as char);
                    } else {
                        let start = self.pos - 1;
                        let len = match b {
                            0xC0..=0xDF => 2,
                            0xE0..=0xEF => 3,
                            0xF0..=0xF7 => 4,
                            _ => return None,
                        };
                        self.pos = start + len;
                        out.push_str(std::str::from_utf8(self.bytes.get(start..start + len)?).ok()?);
                    }
                }
            }
        }
    }

    fn value(&mut self) -> Option<Value> {
        match self.peek()? {
            b'"' => Some(Value::Str(self.string()?)),
            b't' => {
                self.literal("true")?;
                Some(Value::Bool(true))
            }
            b'f' => {
                self.literal("false")?;
                Some(Value::Bool(false))
            }
            b'[' => {
                self.pos += 1;
                let mut items = Vec::new();
                self.skip_ws();
                if self.peek() == Some(b']') {
                    self.pos += 1;
                    return Some(Value::List(items));
                }
                loop {
                    self.skip_ws();
                    items.push(self.string()?);
                    self.skip_ws();
                    match self.next() {
                        Some(b',') => continue,
                        Some(b']') => return Some(Value::List(items)),
                        _ => return None,
                    }
                }
            }
            b'-' | b'0'..=b'9' => {
                let start = self.pos;
                if self.peek() == Some(b'-') {
                    self.pos += 1;
                }
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
                std::str::from_utf8(&self.bytes[start..self.pos])
                    .ok()?
                    .parse()
                    .ok()
                    .map(Value::Int)
            }
            _ => None,
        }
    }

    fn literal(&mut self, lit: &str) -> Option<()> {
        for &b in lit.as_bytes() {
            self.expect(b)?;
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut obj = BTreeMap::new();
        obj.insert("req".into(), Value::Str("status".into()));
        obj.insert("count".into(), Value::Int(-3));
        obj.insert("booted".into(), Value::Bool(true));
        obj.insert("pkgs".into(), Value::List(vec!["a.b".into(), "c\"d".into()]));
        let line = encode(&obj);
        assert_eq!(decode(&line), Some(obj));
    }

    #[test]
    fn escapes_and_unicode() {
        let mut obj = BTreeMap::new();
        obj.insert("s".into(), Value::Str("li\ne \"q\" — ünïcode".into()));
        let line = encode(&obj);
        assert_eq!(decode(&line), Some(obj));
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode("not json").is_none());
        assert!(decode("{\"a\":}").is_none());
        assert!(decode("{\"a\":{\"nested\":1}}").is_none()); // outside the dialect
    }

    #[test]
    fn empty_object_and_list() {
        assert_eq!(decode("{}"), Some(BTreeMap::new()));
        let mut obj = BTreeMap::new();
        obj.insert("l".into(), Value::List(vec![]));
        assert_eq!(decode("{\"l\":[]}"), Some(obj));
    }
}
