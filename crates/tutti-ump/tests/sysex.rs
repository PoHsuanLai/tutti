//! `SysexMessage`: a borrowed, checked, complete MIDI 1.0 SysEx message.

use tutti_ump::{NotSysex, SysexMessage};

/// The least message is the two delimiters.
#[test]
fn the_minimum_message_is_f0_f7() {
    let message = SysexMessage::new(&[0xF0, 0xF7]).unwrap();
    assert_eq!(message.bytes(), [0xF0, 0xF7]);
    assert_eq!(message.payload(), [] as [u8; 0]);
}

/// A message with a payload hands back both views.
#[test]
fn bytes_and_payload() {
    let raw = [0xF0, 0x7E, 0x7F, 0x00, 0xF7];
    let message = SysexMessage::new(&raw).unwrap();
    assert_eq!(message.bytes(), raw);
    assert_eq!(message.payload(), [0x7E, 0x7F, 0x00]);
    assert_eq!(<&[u8]>::from(message), raw);
}

/// Without a leading `F0`, or empty.
#[test]
fn no_f0_is_refused() {
    assert!(SysexMessage::new(&[]).is_none());
    assert!(SysexMessage::new(&[0x7E, 0x7F, 0xF7]).is_none());
    assert!(SysexMessage::new(&[0xF7, 0xF7]).is_none());
}

/// Without a trailing `F7`.
#[test]
fn no_f7_is_refused() {
    assert!(SysexMessage::new(&[0xF0, 0x7E, 0x7F]).is_none());
    assert!(SysexMessage::new(&[0xF0, 0xF0]).is_none());
}

/// One byte cannot be both delimiters.
///
/// Nothing in `new` reaches this alone (the slice pattern needs two bytes,
/// and `F0 != F7`); it pins the one-byte edge against a rewrite that slices
/// `bytes[1..len - 1]` after a `len >= 1` check, which would panic here.
#[test]
fn a_one_byte_message_is_refused() {
    assert!(SysexMessage::new(&[0xF0]).is_none());
    assert!(SysexMessage::new(&[0xF7]).is_none());
}

/// An interior byte of 0x80 or more, `F7` and a status byte included.
#[test]
fn a_wide_interior_byte_is_refused() {
    for bad in [0x80, 0x90, 0xF0, 0xF7, 0xFF] {
        assert!(
            SysexMessage::new(&[0xF0, 0x01, bad, 0x02, 0xF7]).is_none(),
            "{bad:#x}"
        );
    }
    assert!(SysexMessage::new(&[0xF0, 0x7F, 0xF7]).is_some());
}

/// `TryFrom` agrees with `new` and its error carries the refused slice.
#[test]
fn try_from_agrees_with_new() {
    let cases: [&[u8]; 5] = [
        &[0xF0, 0xF7],
        &[0xF0, 1, 2, 0xF7],
        &[1, 2],
        &[0xF0, 0x80, 0xF7],
        &[],
    ];
    for raw in cases {
        let via = SysexMessage::try_from(raw);
        assert_eq!(via.is_ok(), SysexMessage::new(raw).is_some());
        match via {
            Ok(message) => assert_eq!(message.bytes(), raw),
            Err(NotSysex(refused)) => assert_eq!(refused, raw),
        }
    }
    assert!(
        SysexMessage::try_from(&[1u8, 2][..])
            .unwrap_err()
            .to_string()
            .contains("2 bytes")
    );
}
