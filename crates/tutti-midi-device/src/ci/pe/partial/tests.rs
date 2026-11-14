//! Partial Set and `partial` update (M2-103 §10.2, §11.1). Each test names the
//! property of the spec it holds.

use super::*;

fn json(text: &str) -> PeJson {
    PeJson::parse(text.as_bytes()).unwrap()
}

fn pointer(text: &str) -> JsonPointer {
    JsonPointer::new(text).unwrap()
}

/// §10.2 Table 32: the Property Data of X-ProgramEdit before a change.
fn program() -> PeJson {
    json(
        r#"{"name":"PIANO 4","lfoSpeed":30,"lfoWaveform":"triangle",
            "pitchEnvelope":{"rates":[94,67,95,60],"levels":[50,50,50,50]}}"#,
    )
}

// §10.2 Tables 33 and 34: `{"/lfoSpeed":10}` changes the one property.
#[test]
fn a_partial_set_changes_the_property_a_pointer_names() {
    let update = PartialUpdate::from_json(&json(r#"{"/lfoSpeed":10}"#)).unwrap();
    let got = update.apply(&program()).unwrap();
    let expected = json(
        r#"{"name":"PIANO 4","lfoSpeed":10,"lfoWaveform":"triangle",
            "pitchEnvelope":{"rates":[94,67,95,60],"levels":[50,50,50,50]}}"#,
    );
    assert_eq!(got, expected);
}

// §10.2 Tables 35 to 38: two changes to elements of arrays, and the Property Data
// after them.
#[test]
fn a_partial_set_changes_elements_of_arrays() {
    let update = PartialUpdate::from_json(&json(
        r#"{"/pitchEnvelope/rates/0":80,"/pitchEnvelope/levels/0":60}"#,
    ))
    .unwrap();
    let got = update.apply(&program()).unwrap();
    let expected = json(
        r#"{"name":"PIANO 4","lfoSpeed":30,"lfoWaveform":"triangle",
            "pitchEnvelope":{"rates":[80,67,95,60],"levels":[60,50,50,50]}}"#,
    );
    assert_eq!(got, expected);
}

// §10: the value of a change is a string, a number or a boolean, never an object
// or an array.
#[test]
fn a_change_to_an_object_or_an_array_is_not_a_partial_set() {
    for bad in [
        r#"{"/pitchEnvelope":{"rates":[1]}}"#,
        r#"{"/pitchEnvelope/rates":[1,2]}"#,
        r#"{"/x":null}"#,
        r"[1]",
        r#"{"noSlash":1}"#,
    ] {
        assert_eq!(
            PartialUpdate::from_json(&json(bad)),
            Err(PartialError::NotAChange),
            "{bad}"
        );
    }
    assert!(PartialUpdate::from_json(&json(r#"{"/a":"s","/b":1.5,"/c":false}"#)).is_ok());
}

// §10: each key is a JSON Pointer (RFC 6901 §3, §5): empty, or `/`-led tokens in
// which `~` begins `~0` or `~1`.
#[test]
fn a_pointer_follows_rfc_6901() {
    assert!(JsonPointer::new("").is_some());
    assert!(JsonPointer::new("/a/b/0").is_some());
    assert!(JsonPointer::new("/").is_some());
    assert!(JsonPointer::new("a").is_none());
    assert!(JsonPointer::new("/a~").is_none());
    assert!(JsonPointer::new("/a~2").is_none());
    let tokens: Vec<String> = pointer("/a~1b/m~0n/~01").tokens().collect();
    assert_eq!(tokens, ["a/b", "m~n", "~1"]);
}

// A pointer that names a place the data does not have changes nothing: an array
// index past the end, a missing container, a place inside a scalar, an index with
// a leading zero (RFC 6901 §4).
#[test]
fn a_pointer_to_no_place_is_refused_and_changes_nothing() {
    let held = program();
    for bad in [
        "/pitchEnvelope/rates/4",
        "/pitchEnvelope/rates/-",
        "/pitchEnvelope/rates/01",
        "/missing/deeper",
        "/name/inside",
    ] {
        let update = PartialUpdate::new()
            .with(pointer(bad), PeJson::Integer(1))
            .unwrap();
        assert_eq!(
            update.apply(&held),
            Err(PartialError::NoSuchPlace(pointer(bad))),
            "{bad}"
        );
    }
    // The first change is good and the second is not: the data is as it was.
    let update = PartialUpdate::new()
        .with(pointer("/lfoSpeed"), PeJson::Integer(1))
        .unwrap()
        .with(pointer("/nowhere/x"), PeJson::Integer(2))
        .unwrap();
    assert!(update.apply(&held).is_err());
    assert_eq!(held, program());
}

// An optional property the data does not hold yet is added by a change to it.
#[test]
fn a_change_adds_a_property_the_object_lacks() {
    let update = PartialUpdate::new()
        .with(pointer("/programTitle"), PeJson::String("Organ".into()))
        .unwrap();
    let got = update.apply(&json(r#"{"title":"Lead"}"#)).unwrap();
    assert_eq!(got, json(r#"{"title":"Lead","programTitle":"Organ"}"#));
}

// §11.1 and §8.7: the whole value of a Simple Property Resource is changed by the
// empty pointer.
#[test]
fn the_empty_pointer_replaces_the_whole_value() {
    let update = PartialUpdate::new()
        .with(pointer(""), PeJson::Bool(true))
        .unwrap();
    assert_eq!(
        update.apply(&PeJson::Bool(false)).unwrap(),
        PeJson::Bool(true)
    );
}

// A pointer is in a change once: a second value for it replaces the first.
#[test]
fn a_pointer_has_one_value_in_a_change() {
    let update = PartialUpdate::new()
        .with(pointer("/a"), PeJson::Integer(1))
        .unwrap()
        .with(pointer("/a"), PeJson::Integer(2))
        .unwrap();
    assert_eq!(update.iter().count(), 1);
    assert_eq!(update.to_json(), json(r#"{"/a":2}"#));
}

// §10.2 Table 35: the Property Data of a Partial Set is the object of pointers.
#[test]
fn a_change_is_written_as_an_object_of_pointers() {
    let update = PartialUpdate::new()
        .with(pointer("/pitchEnvelope/rates/0"), PeJson::Integer(80))
        .unwrap()
        .with(pointer("/pitchEnvelope/levels/0"), PeJson::Integer(60))
        .unwrap();
    assert_eq!(
        update.to_json().encode(),
        br#"{"/pitchEnvelope/rates/0":80,"/pitchEnvelope/levels/0":60}"#
    );
    assert_eq!(PartialUpdate::from_json(&update.to_json()), Ok(update));
}
