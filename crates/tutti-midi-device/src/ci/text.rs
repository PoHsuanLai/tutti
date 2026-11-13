//! The Message Text of an ACK or a NAK (M2-101 §5.10.4, §5.11.4): ASCII on the
//! wire, with everything else a JSON `\u` escape.

use std::fmt::{self, Write as _};
use std::str::FromStr;
use std::vec::Vec;

use crate::name::NameError;

/// The human-readable text of an ACK or NAK, held as the text it says and
/// written as the pure ASCII the wire carries.
///
/// M2-101 §5.10.4 (7-Bit Encoding of Message Text) has three rules:
///
/// 1. 7-bit ASCII is supported, with the backslash given the special meaning it
///    has in JSON.
/// 2. A character that is not 7-bit ASCII is converted to UTF-16 and escaped
///    as `\u`, as in JSON.
/// 3. A message should not use ASCII below 0x20 except 0x0A (line feed); other
///    such characters, tab and carriage return among them, should be ignored.
///
/// So the writer escapes `\` as `\\`, every character outside `0x20..=0x7E`
/// other than a line feed as one `\uXXXX` per UTF-16 unit (a character past the
/// Basic Multilingual Plane is a surrogate pair, as in JSON), and the reader
/// undoes both, and drops a raw control character the rule says to ignore.
///
/// The bound: a NAK may not exceed the responder's Maximum Receivable SysEx
/// Size from Discovery, so the spec calls up to 103 bytes of Message text safe
/// to send (§5.10.4). 103 is what the
/// smallest receivable size, 128 bytes, leaves after the 25 bytes the rest of
/// the message takes (`F0`, the 13-byte preamble, 10 bytes of status fields,
/// `F7`): [`MAX_ENCODED`](Self::MAX_ENCODED) counts the *escaped* bytes, the
/// ones on the wire. [`new`](Self::new) holds a text to it; a text a peer sent
/// longer (it declared a larger receive size) is read whole.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageText(String);

impl MessageText {
    /// The most escaped bytes the spec calls safe to send, 103.
    pub(crate) const MAX_ENCODED: usize = 103;

    /// The text, or `None` when its escaped form is longer than
    /// [`MAX_ENCODED`](Self::MAX_ENCODED) bytes.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        Self::try_from(text).ok()
    }

    /// The text, unescaped.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether there is no text.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Appends the escaped text to `out`.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        let mut escaped = String::with_capacity(self.0.len());
        for c in self.0.chars() {
            match c {
                '\\' => escaped.push_str("\\\\"),
                '\n' | ' '..='~' => escaped.push(c),
                _ => {
                    let mut units = [0u16; 2];
                    for unit in c.encode_utf16(&mut units) {
                        // Writing to a `String` cannot fail.
                        let _ = write!(escaped, "\\u{unit:04x}");
                    }
                }
            }
        }
        out.extend_from_slice(escaped.as_bytes());
    }

    /// The text of `bytes`, unescaped.
    pub(crate) fn decode(bytes: &[u8]) -> Self {
        let text = String::from_utf8_lossy(bytes);
        let mut out = String::with_capacity(text.len());
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' => match chars.peek().copied() {
                    Some('\\' | '/' | '"') => {
                        out.extend(chars.next());
                    }
                    Some('b') => push_escaped(&mut chars, &mut out, '\u{8}'),
                    Some('f') => push_escaped(&mut chars, &mut out, '\u{c}'),
                    Some('n') => push_escaped(&mut chars, &mut out, '\n'),
                    Some('r') => push_escaped(&mut chars, &mut out, '\r'),
                    Some('t') => push_escaped(&mut chars, &mut out, '\t'),
                    Some('u') => {
                        chars.next();
                        match read_unit(&mut chars) {
                            Some(high @ 0xD800..=0xDBFF) => {
                                // A pair needs its low half right behind it.
                                let mut ahead = chars.clone();
                                let low = (ahead.next() == Some('\\') && ahead.next() == Some('u'))
                                    .then(|| read_unit(&mut ahead))
                                    .flatten()
                                    .filter(|u| (0xDC00..=0xDFFF).contains(u));
                                if let Some(low) = low {
                                    chars = ahead;
                                    out.extend(char::decode_utf16([high, low]).flatten());
                                } else {
                                    out.push(char::REPLACEMENT_CHARACTER);
                                }
                            }
                            Some(unit) => out.push(
                                char::from_u32(u32::from(unit))
                                    .unwrap_or(char::REPLACEMENT_CHARACTER),
                            ),
                            // Not four hex digits: keep what was written.
                            None => out.push_str("\\u"),
                        }
                    }
                    // Not an escape JSON defines: the backslash stands.
                    _ => out.push('\\'),
                },
                // Rule 3: a raw control character is ignored, a line feed not.
                c if c < ' ' && c != '\n' => {}
                c => out.push(c),
            }
        }
        Self(out)
    }
}

impl fmt::Display for MessageText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for MessageText {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl TryFrom<&str> for MessageText {
    type Error = NameError;

    /// The text, or [`NameError::TooLong`] with [`MAX_ENCODED`](Self::MAX_ENCODED)
    /// when its escaped form is longer. A message text has no other refusal:
    /// every character has an escape.
    fn try_from(text: &str) -> Result<Self, NameError> {
        if encoded_len(text) <= Self::MAX_ENCODED {
            Ok(Self(text.to_owned()))
        } else {
            Err(NameError::TooLong {
                max: Self::MAX_ENCODED,
            })
        }
    }
}

impl FromStr for MessageText {
    type Err = NameError;

    fn from_str(text: &str) -> Result<Self, NameError> {
        Self::try_from(text)
    }
}

/// Takes the escape letter just peeked and pushes what it stands for.
fn push_escaped(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, out: &mut String, c: char) {
    chars.next();
    out.push(c);
}

/// Four hex digits as a UTF-16 unit, consuming them only when all four are
/// there.
fn read_unit(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<u16> {
    let mut ahead = chars.clone();
    let mut unit = 0u16;
    for _ in 0..4 {
        unit = unit << 4 | u16::try_from(ahead.next()?.to_digit(16)?).ok()?;
    }
    *chars = ahead;
    Some(unit)
}

fn encoded_len(text: &str) -> usize {
    text.chars()
        .map(|c| match c {
            '\\' => 2,
            '\n' | ' '..='~' => 1,
            c => 6 * c.len_utf16(),
        })
        .sum()
}
