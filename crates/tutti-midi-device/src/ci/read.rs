//! The MIDI-CI grammar: the preamble of a message (M2-101 §3.3, Table 1) and the
//! bodies of Discovery, its reply, Inquiry: Endpoint and its reply, ACK and NAK (§5.5 to §5.8, §5.10, §5.11), built
//! from `winnow` combinators. This is the one module of the crate that names
//! `winnow`, and no `winnow` type leaves it: a body is read through
//! [`discovery_body`] and [`status_body`], which answer `Option`.
//!
//! ```text
//! preamble  := 7E device_id 0D sub_id2 version muid muid          -- source, destination
//! muid      := b b b b                                            -- four bytes; checked after
//! discovery := manufacturer family model revision categories      -- Table 6 and 8
//!              max_size path_id [ function_block ]                -- a Version 1 peer stops early
//! manufacturer := 00 xx yy                                        -- three bytes (M2-104 §7.1.3)
//! family, model := u7 u7
//! revision  := u7 u7 u7 u7
//! max_size  := u7 u7 u7 u7                                        -- 28 bits, 128 or more
//! function_block := 7F | 00..=1F
//! profile_id := b{5}                                              -- Table 19; checked after
//! profile_inquiry := (empty)                                      -- Table 17
//! profile_reply := count profile_id{count} count profile_id{count} -- Table 18; counts are u14
//! set_on, enabled, disabled := profile_id [ u14 ]                 -- Tables 24, 26, 27; Version 2 adds the u14
//! set_off := profile_id [ u14 ]                                   -- Table 25; the u14 is reserved
//! added, removed := profile_id                                    -- Tables 20 and 21
//! details_inquiry := profile_id u7                                -- Table 22
//! details_reply := profile_id u7 u14 data{len}                    -- Table 23
//! specific_data := profile_id u28 data{len}                       -- Table 28; u28 is four bytes
//! endpoint_inquiry := status                                     -- Table 9
//! endpoint_reply   := status len u14 data{len}                    -- Table 11
//! status    := sub_id2? code? data? details{5} [ len u14 text{len} ]
//!                                                                 -- ACK and NAK, Tables 13 and 15
//! ```
//!
//! A field a Version 1 peer does not send (`?`, `[ .. ]`) reads as its default,
//! §5.3's rule to read the fields of the received version, and a field that is
//! there is read in full: a byte with its top bit set, or a text that runs past
//! the message, is malformed. Bytes after the last field are ignored.

use winnow::Parser;
use winnow::combinator::{alt, cut_err, eof, seq};
use winnow::error::ContextError;
use winnow::token::{any, literal, take};

use tutti_ump::{DeviceIdentity, FunctionBlockId, ManufacturerId, U7, U14};

use super::{
    CI_SUB_ID_1, CI_UNIVERSAL_SYSEX, CiCategories, Destination, DetailsData, DiscoveryData,
    InquiryTarget, MaxSysexSize, MessageText, Muid, ProfileChannels, ProfileId, ProfileLists,
    SpecificData,
};

type Res<T> = winnow::ModalResult<T, ContextError>;

/// The wire value of no Function Block (M2-101 §5.6.2).
const NO_FUNCTION_BLOCK_WIRE: u8 = 0x7F;

/// The fixed fields of a message as the wire gives them, before any is checked:
/// what an [`Unread`](super::Unread) keeps.
pub(super) struct Preamble {
    pub(super) device_id: u8,
    pub(super) sub_id2: u8,
    pub(super) version: u8,
    pub(super) source: Option<Muid>,
    pub(super) destination: Option<Destination>,
}

fn wire4(input: &mut &[u8]) -> Res<[u8; 4]> {
    (any, any, any, any)
        .map(|(a, b, c, d)| [a, b, c, d])
        .parse_next(input)
}

/// A 7-bit data byte as a [`U7`].
fn u7(input: &mut &[u8]) -> Res<U7> {
    any.verify_map(U7::new).parse_next(input)
}

/// A [`U7`], or its default when the message ended before it.
fn u7_or_default(input: &mut &[u8]) -> Res<U7> {
    alt((eof.value(U7::default()), u7)).parse_next(input)
}

/// `7E device_id 0D sub_id2 version source destination`: `None` for a message
/// that is not MIDI-CI, or too short to hold the preamble.
pub(super) fn preamble(bytes: &[u8]) -> Option<(Preamble, &[u8])> {
    let mut input = bytes;
    let preamble = preamble_fields(&mut input).ok()?;
    Some((preamble, input))
}

fn preamble_fields(input: &mut &[u8]) -> Res<Preamble> {
    seq!(Preamble {
        _: literal(CI_UNIVERSAL_SYSEX),
        device_id: any,
        _: literal(CI_SUB_ID_1),
        sub_id2: any,
        version: any,
        source: wire4.map(Muid::from_bytes),
        destination: wire4.map(Destination::from_bytes),
    })
    .parse_next(input)
}

/// The three-byte manufacturer id.
fn manufacturer(input: &mut &[u8]) -> Res<ManufacturerId> {
    (u7, u7, u7)
        .verify_map(|(a, b, c)| ManufacturerId::from_sysex_bytes([a, b, c]))
        .parse_next(input)
}

fn identity(input: &mut &[u8]) -> Res<DeviceIdentity> {
    seq!(DeviceIdentity {
        manufacturer: manufacturer,
        family: (u7, u7).map(|(a, b)| [a, b]),
        model: (u7, u7).map(|(a, b)| [a, b]),
        revision: (u7, u7, u7, u7).map(|(a, b, c, d)| [a, b, c, d]),
    })
    .parse_next(input)
}

/// The Receivable Maximum SysEx Message Size: four 7-bit bytes, least
/// significant first (§5.5.3).
fn max_sysex_size(input: &mut &[u8]) -> Res<MaxSysexSize> {
    (u7, u7, u7, u7)
        .verify_map(|(a, b, c, d)| {
            MaxSysexSize::new(
                u32::from(a.get())
                    | (u32::from(b.get()) << 7)
                    | (u32::from(c.get()) << 14)
                    | (u32::from(d.get()) << 21),
            )
        })
        .parse_next(input)
}

/// A reply's Function Block: absent (a Version 1 peer) and `7F` are `None`; 0 to
/// 31 a block; anything else malformed (§5.6.2).
fn function_block(input: &mut &[u8]) -> Res<Option<FunctionBlockId>> {
    alt((
        eof.value(None),
        literal(NO_FUNCTION_BLOCK_WIRE).value(None),
        any.verify_map(FunctionBlockId::new).map(Some),
    ))
    .parse_next(input)
}

fn discovery(is_reply: bool, input: &mut &[u8]) -> Res<DiscoveryData> {
    seq!(DiscoveryData {
        identity: identity,
        categories: u7.map(|c| CiCategories::from_bits_truncate(c.get())),
        max_sysex_size: max_sysex_size,
        output_path_id: u7_or_default,
        function_block: |i: &mut &[u8]| if is_reply {
            function_block(i)
        } else {
            Ok(None)
        },
    })
    .parse_next(input)
}

/// The body of a Discovery (`is_reply` false) or a Reply to Discovery, or
/// `None` when it is malformed.
pub(super) fn discovery_body(is_reply: bool, mut body: &[u8]) -> Option<DiscoveryData> {
    discovery(is_reply, &mut body).ok()
}

/// A 14-bit value, least significant byte first.
fn u14(input: &mut &[u8]) -> Res<U14> {
    (u7, u7)
        .map(|(lsb, msb)| U14::masked(u16::from(msb.get()) << 7 | u16::from(lsb.get())))
        .parse_next(input)
}

/// A Message Length and that many bytes of text; the length is 14 bits.
fn text(input: &mut &[u8]) -> Res<MessageText> {
    u14.flat_map(|len| take(usize::from(len.get())))
        .map(MessageText::decode)
        .parse_next(input)
}

/// The text of an ACK or NAK: none for a message that stops before it (one
/// byte is too short for the length, which is how a Version 1 body ends).
fn message(input: &mut &[u8]) -> Res<MessageText> {
    alt((
        eof.value(MessageText::default()),
        (any, eof).value(MessageText::default()),
        cut_err(text),
    ))
    .parse_next(input)
}

/// The fields of an ACK or NAK body, in wire order.
pub(super) struct Status {
    pub(super) sub_id2: U7,
    pub(super) status_code: U7,
    pub(super) status_data: U7,
    pub(super) details: [U7; 5],
    pub(super) message: MessageText,
}

fn status(input: &mut &[u8]) -> Res<Status> {
    seq!(Status {
        sub_id2: u7_or_default,
        status_code: u7_or_default,
        status_data: u7_or_default,
        details: (
            u7_or_default,
            u7_or_default,
            u7_or_default,
            u7_or_default,
            u7_or_default
        )
            .map(|(a, b, c, d, e)| [a, b, c, d, e]),
        message: message,
    })
    .parse_next(input)
}

/// The body of an ACK or a NAK, or `None` when it is malformed.
pub(super) fn status_body(mut body: &[u8]) -> Option<Status> {
    status(&mut body).ok()
}

/// The raw Status byte of an Inquiry: Endpoint (Table 9), which may be a reserved
/// value: `None` when it is missing or has its top bit set.
pub(super) fn endpoint_inquiry_body(mut body: &[u8]) -> Option<u8> {
    u7.map(U7::get).parse_next(&mut body).ok()
}

/// The Status and Information Data of a Reply to Endpoint (Table 11): the
/// status byte, then a 14-bit length and that many bytes. Bytes after the data are
/// ignored, as after every body.
pub(super) fn endpoint_reply_body(mut body: &[u8]) -> Option<(u8, &[u8])> {
    (
        u7.map(U7::get),
        u14.flat_map(|len| take(usize::from(len.get()))),
    )
        .parse_next(&mut body)
        .ok()
}

/// A Muid, four 7-bit bytes, or `None` (Invalidate MUID's target).
pub(super) fn target_body(mut body: &[u8]) -> Option<Muid> {
    wire4
        .verify_map(Muid::from_bytes)
        .parse_next(&mut body)
        .ok()
}

/// A Profile Id: five bytes (Table 19), each 7-bit, a one-byte manufacturer id
/// followed by two zeroes.
fn profile_id(input: &mut &[u8]) -> Res<ProfileId> {
    take(5usize)
        .verify_map(|bytes: &[u8]| ProfileId::from_bytes(bytes.try_into().ok()?))
        .parse_next(input)
}

/// A count of Channels, a 14-bit field.
fn channels(input: &mut &[u8]) -> Res<ProfileChannels> {
    u14.map(ProfileChannels::from_wire).parse_next(input)
}

/// A count of Profile Ids and that many.
fn profile_list(input: &mut &[u8]) -> Res<Vec<ProfileId>> {
    u14.flat_map(|count| take(usize::from(count.get()) * 5))
        .verify_map(|bytes: &[u8]| {
            // `take` gave exactly `count * 5` bytes.
            bytes
                .as_chunks::<5>()
                .0
                .iter()
                .map(|id| ProfileId::from_bytes(*id))
                .collect::<Option<Vec<_>>>()
        })
        .parse_next(input)
}

/// The two lists of a Reply to Profile Inquiry, or `None` when malformed.
pub(super) fn profile_reply_body(mut body: &[u8]) -> Option<ProfileLists> {
    (profile_list, profile_list)
        .verify_map(|(enabled, disabled)| ProfileLists::new(enabled, disabled).ok())
        .parse_next(&mut body)
        .ok()
}

/// A Profile Id alone (Added, Removed, and Set Off, whose reserved field is
/// ignored), or `None` when malformed.
pub(super) fn profile_body(mut body: &[u8]) -> Option<ProfileId> {
    profile_id.parse_next(&mut body).ok()
}

/// A Profile Id and its Channel count: absent for a Version 1 sender (it reads as
/// 0, §5.3), read in full when there.
pub(super) fn profile_with_count_body(mut body: &[u8]) -> Option<(ProfileId, ProfileChannels)> {
    (
        profile_id,
        alt((eof.value(ProfileChannels::BY_ADDRESS), channels)),
    )
        .parse_next(&mut body)
        .ok()
}

/// The Profile Id and Inquiry Target of a Profile Details Inquiry.
pub(super) fn details_inquiry_body(mut body: &[u8]) -> Option<(ProfileId, InquiryTarget)> {
    (profile_id, u7.map(InquiryTarget::new))
        .parse_next(&mut body)
        .ok()
}

/// The Profile Id, target and data of a Reply to Profile Details Inquiry.
pub(super) fn details_reply_body(
    mut body: &[u8],
) -> Option<(ProfileId, InquiryTarget, DetailsData)> {
    (
        profile_id,
        u7.map(InquiryTarget::new),
        u14.flat_map(|len| take(usize::from(len.get())))
            .verify_map(|data: &[u8]| DetailsData::new(data.to_vec()).ok()),
    )
        .map(|(profile, target, data)| (profile, target, data))
        .parse_next(&mut body)
        .ok()
}

/// The Profile Id and data of Profile Specific Data: a 28-bit length, four 7-bit
/// bytes, least significant first.
pub(super) fn specific_data_body(mut body: &[u8]) -> Option<(ProfileId, SpecificData)> {
    (
        profile_id,
        wire4
            .verify_map(|b| {
                b.iter()
                    .all(|byte| byte & 0x80 == 0)
                    .then(|| {
                        usize::try_from(
                            u32::from(b[0])
                                | (u32::from(b[1]) << 7)
                                | (u32::from(b[2]) << 14)
                                | (u32::from(b[3]) << 21),
                        )
                        .ok()
                    })
                    .flatten()
            })
            .flat_map(take)
            .verify_map(|data: &[u8]| SpecificData::new(data.to_vec()).ok()),
    )
        .parse_next(&mut body)
        .ok()
}
