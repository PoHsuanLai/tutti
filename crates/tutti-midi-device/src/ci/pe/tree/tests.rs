//! The JSON of Property Data (M2-103 §6.1). Each test names the property of the
//! spec it holds.

use super::*;

fn parse(text: &str) -> Result<PeJson, PeJsonError> {
    PeJson::parse(text.as_bytes())
}

// §8.2 Table 18 and §10.1 Table 28: Property Data is written with line breaks and
// indentation, which are not Header Data's restriction (§7.1.1).
#[test]
fn property_data_may_have_whitespace_between_tokens() {
    let got = parse("{\n  \"name\": \"PIANO 4\",\n  \"rates\": [94, 67,\t95]\n}").unwrap();
    let object = got.as_object().unwrap();
    assert_eq!(object.get("name").and_then(PeJson::as_str), Some("PIANO 4"));
    assert_eq!(
        object.get("rates"),
        Some(&PeJson::Array(vec![
            PeJson::Integer(94),
            PeJson::Integer(67),
            PeJson::Integer(95)
        ]))
    );
}

// §6.1.1: a character beyond 7-bit ASCII is written as the `\u` escapes of its
// UTF-16, and Beats followed by the eighth note is `Beats` and the escape of
// U+266A.
#[test]
fn a_non_ascii_character_is_written_as_utf16_escapes() {
    let escape = |hex: &str| format!("\\u{hex}");
    let text = PeJson::String("Beats\u{266a}".to_owned());
    let written = format!("\"Beats{}\"", escape("266a"));
    assert_eq!(text.encode(), written.as_bytes());
    assert_eq!(parse(&written).unwrap(), text);
    // Outside the Basic Multilingual Plane the escapes are a surrogate pair.
    let clef = PeJson::String("\u{1d11e}".to_owned());
    let pair = format!("\"{}{}\"", escape("d834"), escape("dd1e"));
    assert_eq!(clef.encode(), pair.as_bytes());
    assert_eq!(parse(&pair).unwrap(), clef);
}

// §6.1.1: the text of Property Data is 7-bit, so a byte past 7F is not read.
#[test]
fn a_byte_past_7f_is_not_json_of_property_exchange() {
    assert!(PeJson::parse(b"\"\xc3\xa9\"").is_err());
}

// §6.1.1: the forward slash may be escaped or not.
#[test]
fn a_forward_slash_may_be_escaped() {
    assert_eq!(parse(r#""a\/b""#).unwrap(), parse(r#""a/b""#).unwrap());
}

// §10.1 Table 29: the compact text of a value reads back as the value.
#[test]
fn the_compact_text_of_a_value_reads_back() {
    let value = parse(r#"{"a":[1,2.5,true,null,"x\"y"],"b":{"c":-3}}"#).unwrap();
    assert_eq!(
        parse(std::str::from_utf8(&value.encode()).unwrap()).unwrap(),
        value
    );
    assert!(!value.encode().contains(&b' '));
}

// A name written twice in an object has no one value, so the object is refused.
#[test]
fn an_object_with_a_name_twice_is_refused() {
    assert!(parse(r#"{"a":1,"a":2}"#).is_err());
}

// Property Data comes from a peer: the nesting the reader recurses to is bounded.
#[test]
fn nesting_past_the_limit_is_refused() {
    let deep = |n: usize| format!("{}{}", "[".repeat(n), "]".repeat(n));
    assert!(parse(&deep(PE_JSON_MAX_DEPTH)).is_ok());
    assert!(parse(&deep(PE_JSON_MAX_DEPTH + 2)).is_err());
}

#[test]
fn text_after_the_value_is_refused() {
    assert!(parse("{} x").is_err());
    assert!(parse("").is_err());
    assert_eq!(
        parse(" \n{} \r\n").unwrap(),
        PeJson::Object(PeObject::new())
    );
}

// §6.1.1 and RFC 8259: a number is decimal; one with a point or an exponent is
// not an integer.
#[test]
fn a_number_with_a_point_or_exponent_is_a_decimal() {
    assert_eq!(parse("120").unwrap(), PeJson::Integer(120));
    assert!(matches!(parse("1.5").unwrap(), PeJson::Decimal(_)));
    assert!(matches!(parse("1e3").unwrap(), PeJson::Decimal(_)));
    assert!(parse("0x42").is_err());
    assert!(parse("01").is_err());
}
