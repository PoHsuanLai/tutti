//! The reader of Header Data: a JSON object of number, boolean and string
//! properties, built from `winnow` combinators (M2-103 §6.1.1, §7.1.1). No
//! `winnow` type leaves this module: [`object`] answers the properties in order
//! or the offset where the text stopped being the grammar.
//!
//! ```text
//! object   := '{' [ member (',' member)* ] '}'      -- no whitespace anywhere
//! member   := string ':' value
//! value    := string | number | 'true' | 'false'    -- no null, object or array
//! string   := '"' (run | '\' escape)* '"'
//! run      := bytes 0x20..=0x7F except '"' and '\'
//! escape   := '"' | '\' | '/' | b | f | n | r | t | 'u' hex{4}
//! number   := '-'? ('0' | 1-9 digit*) ('.' digit+)? ([eE] [+-]? digit+)?
//! ```
//!
//! A `\u` escape is a UTF-16 unit; a pair of them is one character and a lone
//! surrogate is malformed.

use winnow::Parser;
use winnow::combinator::{alt, delimited, opt, repeat, separated, separated_pair};
use winnow::error::{ContextError, ErrMode};
use winnow::token::{literal, one_of, take, take_while};

use super::tree::{PE_JSON_MAX_DEPTH, PeJson, PeObject};
use super::value::{PeDecimal, PeValue};

type Res<T> = winnow::ModalResult<T, ContextError>;

/// One property as written: its name and its value.
pub(super) type Member = (String, PeValue);

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

fn number(input: &mut &[u8]) -> Res<PeValue> {
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
                    .map(PeValue::Decimal)
            } else {
                text.parse().ok().map(PeValue::Integer)
            }
        })
        .parse_next(input)
}

fn value(input: &mut &[u8]) -> Res<PeValue> {
    alt((
        string.map(PeValue::String),
        number,
        literal("true").value(PeValue::Bool(true)),
        literal("false").value(PeValue::Bool(false)),
    ))
    .parse_next(input)
}

fn member(input: &mut &[u8]) -> Res<Member> {
    separated_pair(string, literal(b':'), value).parse_next(input)
}

/// The properties of the JSON object `bytes` holds, in order, or the offset of
/// the first byte that is not the grammar.
pub(super) fn object(bytes: &[u8]) -> Result<Vec<Member>, usize> {
    delimited(
        literal(b'{'),
        separated(0.., member, literal(b',')),
        literal(b'}'),
    )
    .parse(bytes)
    .map_err(|e| e.offset())
}

fn white(input: &mut &[u8]) -> Res<()> {
    take_while(0.., |b: u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        .void()
        .parse_next(input)
}

fn scalar(value: PeValue) -> PeJson {
    match value {
        PeValue::Bool(b) => PeJson::Bool(b),
        PeValue::Integer(n) => PeJson::Integer(n),
        PeValue::Decimal(d) => PeJson::Decimal(d),
        PeValue::String(s) => PeJson::String(s),
    }
}

/// A value of Property Data at nesting `depth`: whitespace may precede it.
fn node(input: &mut &[u8], depth: usize) -> Res<PeJson> {
    if depth > PE_JSON_MAX_DEPTH {
        return Err(ErrMode::Cut(ContextError::new()));
    }
    white(input)?;
    let value = alt((
        string.map(PeJson::String),
        number.map(scalar),
        literal("true").value(PeJson::Bool(true)),
        literal("false").value(PeJson::Bool(false)),
        literal("null").value(PeJson::Null),
        |i: &mut &[u8]| array(i, depth),
        |i: &mut &[u8]| object_node(i, depth),
    ))
    .parse_next(input)?;
    white(input)?;
    Ok(value)
}

fn array(input: &mut &[u8], depth: usize) -> Res<PeJson> {
    delimited(
        literal(b'['),
        separated(0.., |i: &mut &[u8]| node(i, depth + 1), literal(b',')),
        literal(b']'),
    )
    .map(PeJson::Array)
    .parse_next(input)
}

fn object_node(input: &mut &[u8], depth: usize) -> Res<PeJson> {
    let pair = |i: &mut &[u8]| {
        white(i)?;
        let name = string(i)?;
        white(i)?;
        literal(b':').parse_next(i)?;
        Ok((name, node(i, depth + 1)?))
    };
    delimited(
        literal(b'{'),
        separated(0.., pair, literal(b',')),
        literal(b'}'),
    )
    .verify_map(|members: Vec<(String, PeJson)>| PeObject::from_members(members))
    .map(PeJson::Object)
    .parse_next(input)
}

/// The value the JSON text `bytes` holds, or the offset of the first byte that
/// is not the grammar.
pub(super) fn document(bytes: &[u8]) -> Result<PeJson, usize> {
    (|i: &mut &[u8]| node(i, 0))
        .parse(bytes)
        .map_err(|e| e.offset())
}
