//! The writer of Header Data: compact JSON, with every character outside
//! printable ASCII escaped as JSON does (M2-103 §6.1.1, §7.1.1).

use std::fmt::Write as _;

use super::value::PeValue;

/// Appends `text` to `out` as a JSON string, quotes included.
fn string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' | '\u{7f}' => out.push(c),
            _ => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    // Writing to a `String` cannot fail.
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
        }
    }
    out.push('"');
}

/// The bytes `text` takes as the content of a JSON string, quotes excluded.
pub(super) fn escaped_len(text: &str) -> usize {
    let mut out = String::new();
    string(&mut out, text);
    out.len() - 2
}

/// A JSON object being written, property by property.
pub(super) struct Object {
    out: String,
    empty: bool,
}

impl Object {
    pub(super) fn new() -> Self {
        Self {
            out: String::from("{"),
            empty: true,
        }
    }

    fn key(&mut self, key: &str) {
        if !self.empty {
            self.out.push(',');
        }
        self.empty = false;
        string(&mut self.out, key);
        self.out.push(':');
    }

    pub(super) fn text(&mut self, key: &str, value: &str) {
        self.key(key);
        string(&mut self.out, value);
    }

    pub(super) fn integer(&mut self, key: &str, value: u64) {
        self.key(key);
        // Writing to a `String` cannot fail.
        let _ = write!(self.out, "{value}");
    }

    pub(super) fn boolean(&mut self, key: &str, value: bool) {
        self.key(key);
        self.out.push_str(if value { "true" } else { "false" });
    }

    pub(super) fn value(&mut self, key: &str, value: &PeValue) {
        match value {
            PeValue::Bool(b) => self.boolean(key, *b),
            PeValue::Integer(n) => {
                self.key(key);
                let _ = write!(self.out, "{n}");
            }
            PeValue::Decimal(d) => {
                self.key(key);
                // `Debug` keeps a point or an exponent, so a decimal reads back as one.
                let _ = write!(self.out, "{:?}", d.get());
            }
            PeValue::String(s) => self.text(key, s),
        }
    }

    pub(super) fn finish(mut self) -> Vec<u8> {
        self.out.push('}');
        self.out.into_bytes()
    }
}
