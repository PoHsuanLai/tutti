//! The reader of JSON text, built from `winnow` combinators, for the Header Data
//! of a message (M2-103 §7.1.1) and for Property Data (§6.1). One grammar reads
//! both, in the [`Grammar`] of each: Header Data is an object of number, boolean
//! and string properties with no whitespace, Property Data is any JSON value with
//! whitespace between tokens. No `winnow` type leaves this module: [`object`] and
//! [`document`] answer the value or the offset where the text stopped being the
//! grammar.
//!
//! ```text
//! object   := '{' [ member (',' member)* ] '}'
//! member   := string ':' value
//! value    := string | number | 'true' | 'false'    -- Header Data stops here
//!           | 'null' | array | object               -- Property Data goes on
//! array    := '[' [ value (',' value)* ] ']'
//!           -- Header Data has no whitespace anywhere; Property Data has it between
//!           -- tokens, and each name once in an object, to a nesting of
//!           -- `PE_JSON_MAX_DEPTH`
//! string   := '"' (run | '\' escape)* '"'
//! run      := bytes 0x20..=0x7F except '"' and '\'
//! escape   := '"' | '\' | '/' | b | f | n | r | t | 'u' hex{4}
//! number   := '-'? ('0' | 1-9 digit*) ('.' digit+)? ([eE] [+-]? digit+)?
//! ```
//!
//! A `\u` escape is a UTF-16 unit; a pair of them is one character and a lone
//! surrogate is malformed.

use winnow::Parser;
use winnow::combinator::{alt, delimited, opt, repeat, separated};
use winnow::error::{ContextError, ErrMode};
use winnow::token::{literal, one_of, take, take_while};

use super::tree::{PE_JSON_MAX_DEPTH, PeDecimal, PeJson, PeObject};

type Res<T> = winnow::ModalResult<T, ContextError>;

/// One property as written: its name and its value.
pub(super) type Member = (String, PeJson);

/// A piece of a string, in UTF-16 units.
fn piece(input: &mut &[u8]) -> Res<Vec<u16>> {
    alt((
        take_while(1.., |b: u8| {
            (0x20..=0x7F).contains(&b) && b != b'"' && b != b'\\'
        })
        .map(|run: &[u8]| run.iter().map(|&b| u16::from(b)).collect()),
        (literal(b'\\'), escape).map(|(_, unit)| vec![unit]),
    ))
    .parse_next(input)
}

fn escape(input: &mut &[u8]) -> Res<u16> {
    alt((
        literal(b'"').value(0x22),
        literal(b'\\').value(0x5C),
        literal(b'/').value(0x2F),
        literal(b'b').value(0x08),
        literal(b'f').value(0x0C),
        literal(b'n').value(0x0A),
        literal(b'r').value(0x0D),
        literal(b't').value(0x09),
        (literal(b'u'), take(4usize).verify_map(hex4)).map(|(_, unit)| unit),
    ))
    .parse_next(input)
}

fn hex4(digits: &[u8]) -> Option<u16> {
    if !digits.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    u16::from_str_radix(std::str::from_utf8(digits).ok()?, 16).ok()
}

fn string(input: &mut &[u8]) -> Res<String> {
    delimited(
        literal(b'"'),
        repeat(0.., piece).fold(Vec::new, |mut units: Vec<u16>, piece| {
            units.extend(piece);
            units
        }),
        literal(b'"'),
    )
    .verify_map(|units| String::from_utf16(&units).ok())
    .parse_next(input)
}

fn digit1<'a>(input: &mut &'a [u8]) -> Res<&'a [u8]> {
    take_while(1.., b'0'..=b'9').parse_next(input)
}

fn number(input: &mut &[u8]) -> Res<PeJson> {
    (
        opt(literal(b'-')),
        alt((
            literal(b'0').take(),
            (one_of(b'1'..=b'9'), opt(digit1)).take(),
        )),
        opt((literal(b'.'), digit1)),
        opt((one_of(b"eE"), opt(one_of(b"+-")), digit1)),
    )
        .take()
        .verify_map(|raw: &[u8]| {
            let text = std::str::from_utf8(raw).ok()?;
            if raw.iter().any(|b| matches!(b, b'.' | b'e' | b'E')) {
                text.parse()
                    .ok()
                    .and_then(PeDecimal::new)
                    .map(PeJson::Decimal)
            } else {
                text.parse().ok().map(PeJson::Integer)
            }
        })
        .parse_next(input)
}

/// What a text may hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Grammar {
    /// Header Data (M2-103 §7.1.1): an object of scalar properties, with no
    /// whitespace.
    Header,
    /// Property Data (§6.1): any JSON value, with whitespace between tokens.
    Data,
}

fn white(input: &mut &[u8], grammar: Grammar) -> Res<()> {
    match grammar {
        Grammar::Header => Ok(()),
        Grammar::Data => take_while(0.., |b: u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
            .void()
            .parse_next(input),
    }
}

/// A value at nesting `depth`, with the whitespace around it.
fn node(input: &mut &[u8], depth: usize, grammar: Grammar) -> Res<PeJson> {
    if depth > PE_JSON_MAX_DEPTH {
        return Err(ErrMode::Cut(ContextError::new()));
    }
    white(input, grammar)?;
    let value = match grammar {
        Grammar::Header => alt((
            string.map(PeJson::String),
            number,
            literal("true").value(PeJson::Bool(true)),
            literal("false").value(PeJson::Bool(false)),
        ))
        .parse_next(input)?,
        Grammar::Data => alt((
            string.map(PeJson::String),
            number,
            literal("true").value(PeJson::Bool(true)),
            literal("false").value(PeJson::Bool(false)),
            literal("null").value(PeJson::Null),
            |i: &mut &[u8]| array(i, depth),
            |i: &mut &[u8]| {
                members(i, depth, grammar).and_then(|m| {
                    PeObject::from_members(m)
                        .map(PeJson::Object)
                        .ok_or_else(|| ErrMode::Backtrack(ContextError::new()))
                })
            },
        ))
        .parse_next(input)?,
    };
    white(input, grammar)?;
    Ok(value)
}

fn array(input: &mut &[u8], depth: usize) -> Res<PeJson> {
    delimited(
        literal(b'['),
        separated(
            0..,
            |i: &mut &[u8]| node(i, depth + 1, Grammar::Data),
            literal(b','),
        ),
        literal(b']'),
    )
    .map(PeJson::Array)
    .parse_next(input)
}

/// The properties of an object, in order and as written: a name may repeat.
fn members(input: &mut &[u8], depth: usize, grammar: Grammar) -> Res<Vec<Member>> {
    let pair = |i: &mut &[u8]| {
        white(i, grammar)?;
        let name = string(i)?;
        white(i, grammar)?;
        literal(b':').parse_next(i)?;
        Ok((name, node(i, depth + 1, grammar)?))
    };
    delimited(
        literal(b'{'),
        separated(0.., pair, literal(b',')),
        literal(b'}'),
    )
    .parse_next(input)
}

/// The properties of the Header Data object `bytes` holds, in order, or the offset
/// of the first byte that is not the grammar.
pub(super) fn object(bytes: &[u8]) -> Result<Vec<Member>, usize> {
    (|i: &mut &[u8]| members(i, 0, Grammar::Header))
        .parse(bytes)
        .map_err(|e| e.offset())
}

/// The value the Property Data text `bytes` holds, or the offset of the first byte
/// that is not the grammar.
pub(super) fn document(bytes: &[u8]) -> Result<PeJson, usize> {
    (|i: &mut &[u8]| node(i, 0, Grammar::Data))
        .parse(bytes)
        .map_err(|e| e.offset())
}
