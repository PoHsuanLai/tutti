//! The Universal System Exclusive grammar: the payload bytes of a checked
//! [`SysexMessage`](tutti_ump::SysexMessage) into a [`Universal`], built from
//! `winnow` combinators.
//!
//! ```text
//! message   := 7E device ( general_information | general_midi ) eof   -- Non-Real Time
//!            | 7F device device_control eof                           -- Real Time
//! general_information := 06 ( 01                                      -- Identity Request
//!                           | 02 manufacturer family model revision ) -- Identity Reply
//! general_midi        := 09 ( 01 | 02 )                               -- GM System On, Off
//! device_control      := 04 ( 01 u14 | 02 u14 | 03 u14 | 04 00 u7 )   -- volume, balance, tuning
//! manufacturer        := 00 xx yy | mm                                -- one byte or three
//! ```
//!
//! Every parser is `fn(&mut &[u8]) -> Res<T>`. A data byte is 7-bit before it
//! gets here (`SysexMessage` checked it), and a field is read as the unit type
//! that holds it, so no rule masks or shifts a byte. A message that matches no
//! rule is not an error to the caller: [`parse`] returns `None`, and
//! [`read()`](fn@super::read) says whether it was Universal at all.

use winnow::Parser;
use winnow::combinator::{alt, eof, fail, preceded, seq};
use winnow::error::ContextError;
use winnow::token::{any, literal};

use tutti_ump::{DeviceIdentity, ManufacturerId, U7, U14};

use super::{DeviceId, GeneralMidi, Universal};

type Res<T> = winnow::ModalResult<T, ContextError>;

/// The SysEx id of a Universal Non-Real Time message (M1 p.34).
pub(super) const NON_REAL_TIME: u8 = 0x7E;
/// The SysEx id of a Universal Real Time message (M1 p.34).
pub(super) const REAL_TIME: u8 = 0x7F;

/// A 7-bit data byte as a [`U7`].
fn u7(input: &mut &[u8]) -> Res<U7> {
    any.verify_map(U7::new).parse_next(input)
}

/// A 14-bit value sent least significant byte first (M1 p.40, p.57).
fn u14(input: &mut &[u8]) -> Res<U14> {
    (u7, u7)
        .map(|(lsb, msb)| U14::masked(u16::from(msb.get()) << 7 | u16::from(lsb.get())))
        .parse_next(input)
}

fn device(input: &mut &[u8]) -> Res<DeviceId> {
    u7.map(DeviceId::new).parse_next(input)
}

/// The manufacturer id of an Identity Reply: one byte, or `00` and two more
/// (M1 p.40, "if the manufacturers id code (mm) begins with 00H then the above
/// message is extended by two bytes").
fn manufacturer(input: &mut &[u8]) -> Res<ManufacturerId> {
    alt((
        preceded(literal(0x00_u8), (u7, u7))
            .verify_map(|(a, b)| ManufacturerId::from_sysex_bytes([U7::of(0), a, b])),
        u7.verify_map(|m| ManufacturerId::new(u16::from(m.get()))),
    ))
    .parse_next(input)
}

/// The Identity Reply's fields after its sub-ids (M1 p.40).
fn identity(input: &mut &[u8]) -> Res<DeviceIdentity> {
    seq!(DeviceIdentity {
        manufacturer: manufacturer,
        family: (u7, u7).map(|(a, b)| [a, b]),
        model: (u7, u7).map(|(a, b)| [a, b]),
        revision: (u7, u7, u7, u7).map(|(a, b, c, d)| [a, b, c, d]),
    })
    .parse_next(input)
}

/// `06`, General Information (M1 p.40).
fn general_information(input: &mut &[u8]) -> Res<Universal> {
    preceded(
        literal(0x06_u8),
        alt((
            literal(0x01_u8).value(Universal::IdentityRequest),
            preceded(literal(0x02_u8), identity).map(Universal::IdentityReply),
        )),
    )
    .parse_next(input)
}

/// `09`, General MIDI (M1 p.52).
fn general_midi(input: &mut &[u8]) -> Res<Universal> {
    preceded(
        literal(0x09_u8),
        alt((
            literal(0x01_u8).value(GeneralMidi::On),
            literal(0x02_u8).value(GeneralMidi::Off),
        )),
    )
    .map(Universal::GeneralMidi)
    .parse_next(input)
}

/// `04`, Device Control: Master Volume and Balance (M1 p.57), Master Fine and
/// Coarse Tuning (CA-025). Coarse Tuning's data is `00 msb`: "the LSB is always
/// 0", so only the MSB is read.
fn device_control(input: &mut &[u8]) -> Res<Universal> {
    preceded(
        literal(0x04_u8),
        alt((
            preceded(literal(0x01_u8), u14).map(Universal::MasterVolume),
            preceded(literal(0x02_u8), u14).map(Universal::MasterBalance),
            preceded(literal(0x03_u8), u14).map(Universal::MasterFineTuning),
            preceded(literal(0x04_u8), preceded(u7, u7)).map(Universal::MasterCoarseTuning),
        )),
    )
    .parse_next(input)
}

/// The addressed form: the Universal message and who it names.
fn addressed(input: &mut &[u8]) -> Res<(DeviceId, Universal)> {
    let class = any.parse_next(input)?;
    let to = device.parse_next(input)?;
    let message = match class {
        NON_REAL_TIME => alt((general_information, general_midi)).parse_next(input)?,
        REAL_TIME => device_control.parse_next(input)?,
        _ => return fail.parse_next(input),
    };
    eof.parse_next(input)?;
    Ok((to, message))
}

/// The payload of a SysEx message (between `F0` and `F7`) as a Universal
/// message and its device id, or `None` when no rule matches.
pub(super) fn parse(mut payload: &[u8]) -> Option<(DeviceId, Universal)> {
    addressed.parse_next(&mut payload).ok()
}
