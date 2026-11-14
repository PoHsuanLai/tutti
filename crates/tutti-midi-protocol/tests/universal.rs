//! Universal System Exclusive: the grammar of M1 pp.40, 52 and 57 and CA-025,
//! and the instrument's edge that reassembles, filters by device id and reads.

use tutti_midi_protocol::{
    Addressed, Addressee, GeneralMidi, Heard, MtcFullMessage, MtcRate, NotSysexDeviceId, Read,
    Recipient, SysexDeviceId, Universal, UniversalReader, read,
};
use tutti_ump::{
    Address, DeviceIdentity, ManufacturerId, Message, MidiChannel, MidiGroup, NoteNumber,
    SysexMessage, U7, Ump,
};

fn parse(bytes: &[u8]) -> Read {
    read(SysexMessage::from_framed(bytes).expect("a framed message"))
}

fn heard(bytes: &[u8]) -> Addressed {
    match parse(bytes) {
        Read::Universal(a) => a,
        other => panic!("{other:?}"),
    }
}

fn u7s<const N: usize>(bytes: [u8; N]) -> [U7; N] {
    bytes.map(U7::of)
}

/// M1 p.40: `F0 7E <device ID> 06 01 F7` is the Identity Request.
#[test]
fn an_identity_request_is_the_inquiry_of_p40() {
    let a = heard(&[0xF0, 0x7E, 0x7F, 0x06, 0x01, 0xF7]);
    assert_eq!(a.message, Universal::IdentityRequest);
    assert_eq!(a.to, Recipient::AllCall);
    let a = heard(&[0xF0, 0x7E, 0x05, 0x06, 0x01, 0xF7]);
    assert_eq!(a.to, Recipient::Device(SysexDeviceId::of(5)));
}

/// M1 p.40: `06 02 mm ff ff dd dd ss ss ss ss`, the 14-bit family and member
/// codes LSB first, and a manufacturer id that begins with 00 extended by two
/// bytes.
#[test]
fn an_identity_reply_has_a_one_or_three_byte_manufacturer() {
    let one = heard(&[
        0xF0, 0x7E, 0x00, 0x06, 0x02, 0x41, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0xF7,
    ]);
    assert_eq!(
        one.message,
        Universal::IdentityReply(DeviceIdentity {
            manufacturer: ManufacturerId::of(0x41),
            family: u7s([1, 2]),
            model: u7s([3, 4]),
            revision: u7s([5, 6, 7, 8]),
        })
    );
    let three = heard(&[
        0xF0, 0x7E, 0x00, 0x06, 0x02, 0x00, 0x21, 0x09, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
        0x08, 0xF7,
    ]);
    let Universal::IdentityReply(id) = three.message else {
        panic!()
    };
    assert_eq!(id.manufacturer, ManufacturerId::of(0xA109));
    assert_eq!(id.revision, u7s([5, 6, 7, 8]));
}

/// M1 p.52: `09 01` is General MIDI On and `09 02` Off.
#[test]
fn general_midi_on_and_off() {
    assert_eq!(
        heard(&[0xF0, 0x7E, 0x7F, 0x09, 0x01, 0xF7]).message,
        Universal::GeneralMidi(GeneralMidi::On)
    );
    assert_eq!(
        heard(&[0xF0, 0x7E, 0x7F, 0x09, 0x02, 0xF7]).message,
        Universal::GeneralMidi(GeneralMidi::Off)
    );
}

fn value(message: Universal) -> u16 {
    match message {
        Universal::MasterVolume(v)
        | Universal::MasterBalance(v)
        | Universal::MasterFineTuning(v) => v.get(),
        Universal::MasterCoarseTuning(v) => u16::from(v.get()),
        other => panic!("{other:?}"),
    }
}

/// M1 p.57: Master Volume and Balance carry two data bytes, least significant first; `00 00` is
/// off or hard left and `7F 7F` hard right.
#[test]
fn master_volume_and_balance_are_fourteen_bits_lsb_first() {
    let volume = |lsb, msb| heard(&[0xF0, 0x7F, 0x7F, 0x04, 0x01, lsb, msb, 0xF7]).message;
    assert!(matches!(volume(0, 0), Universal::MasterVolume(_)));
    assert_eq!(value(volume(0x00, 0x00)), 0);
    assert_eq!(value(volume(0x7F, 0x7F)), 0x3FFF);
    // lsb 0x01, msb 0x02 is 2 * 128 + 1: the lsb comes first.
    assert_eq!(value(volume(0x01, 0x02)), 0x0101);
    let balance = heard(&[0xF0, 0x7F, 0x7F, 0x04, 0x02, 0x00, 0x40, 0xF7]).message;
    assert!(matches!(balance, Universal::MasterBalance(_)));
    assert_eq!(value(balance), 0x2000);
}

/// CA-025: Master Fine Tuning is `04 03 lsb msb`, `00 40` the centre; Master
/// Coarse Tuning is `04 04 00 msb`, `00 40` the centre and `00 7F` +63.
#[test]
fn master_fine_and_coarse_tuning_follow_ca25() {
    let fine = heard(&[0xF0, 0x7F, 0x7F, 0x04, 0x03, 0x00, 0x40, 0xF7]).message;
    assert!(matches!(fine, Universal::MasterFineTuning(_)));
    assert_eq!(value(fine), 0x2000);
    let coarse = |msb| heard(&[0xF0, 0x7F, 0x7F, 0x04, 0x04, 0x00, msb, 0xF7]).message;
    assert!(matches!(coarse(0), Universal::MasterCoarseTuning(_)));
    assert_eq!(value(coarse(0x40)), 0x40);
    assert_eq!(value(coarse(0x7F)), 0x7F);
}

/// M1 p.53 and T-9: the MTC Full Message is `F0 7F <id> 01 01 hr mn sc fr F7`
/// with `hr` as `0rrhhhhh`: the two rate bits sit above the five hour bits.
#[test]
fn an_mtc_full_message_reads_its_position_and_rate_code() {
    let full = |hr, mn, sc, fr| heard(&[0xF0, 0x7F, 0x7F, 0x01, 0x01, hr, mn, sc, fr, 0xF7]);
    for (code, rate) in [
        (0u8, MtcRate::Fps24),
        (1, MtcRate::Fps25),
        (2, MtcRate::Fps2997Df),
        (3, MtcRate::Fps30),
    ] {
        let a = full(code << 5 | 23, 59, 58, 17);
        assert_eq!(a.to, Recipient::AllCall);
        assert_eq!(
            a.message,
            Universal::MtcFullMessage(MtcFullMessage::new(rate, 23, 59, 58, 17).unwrap())
        );
    }
}

/// A position no clock reads is not a Full Message: the frame field is below
/// the rate's label count, and the hours below 24.
#[test]
fn an_mtc_full_message_out_of_range_is_unread() {
    let bytes = |hr, mn, sc, fr| [0xF0, 0x7F, 0x7F, 0x01, 0x01, hr, mn, sc, fr, 0xF7];
    for (hr, mn, sc, fr) in [
        (24u8, 0u8, 0u8, 0u8),
        (0, 60, 0, 0),
        (0, 0, 60, 0),
        (0, 0, 0, 24),
        (1 << 5, 0, 0, 25),
        (3 << 5, 0, 0, 30),
    ] {
        assert_eq!(
            parse(&bytes(hr, mn, sc, fr)),
            Read::Unread,
            "{hr} {mn} {sc} {fr}"
        );
    }
    assert!(MtcFullMessage::new(MtcRate::Fps25, 0, 0, 0, 24).is_some());
    assert!(MtcFullMessage::new(MtcRate::Fps25, 0, 0, 0, 25).is_none());
}

/// A message that is not one this crate reads is a typed outcome, never a
/// refusal that drops a stream: a manufacturer's SysEx is `Foreign`, another
/// Universal message (GM2 On, MIDI-CI) or a cut or overlong one is `Unread`.
#[test]
fn what_is_not_read_is_foreign_or_unread() {
    assert_eq!(
        parse(&[0xF0, 0x41, 0x10, 0x42, 0xF7]),
        Read::Foreign,
        "a manufacturer"
    );
    assert_eq!(parse(&[0xF0, 0xF7]), Read::Foreign, "an empty message");
    for unread in [
        &[0xF0, 0x7E, 0x7F, 0x09, 0x03, 0xF7][..],
        &[0xF0, 0x7E, 0x7F, 0x0D, 0x70, 0x01, 0xF7],
        &[0xF0, 0x7F, 0x7F, 0x04, 0x01, 0x00, 0xF7],
        &[0xF0, 0x7F, 0x7F, 0x04, 0x01, 0x00, 0x40, 0x01, 0xF7],
        &[0xF0, 0x7E, 0x7F, 0x09, 0x01, 0x00, 0xF7],
        &[0xF0, 0x7E, 0x7F, 0x06, 0x02, 0x41, 0x01, 0xF7],
        &[0xF0, 0x7E, 0xF7],
        &[0xF0, 0x7F, 0x7F, 0x04, 0x09, 0x00, 0x00, 0xF7],
    ] {
        assert_eq!(parse(unread), Read::Unread, "{unread:02X?}");
    }
}

fn packets(group: u8, bytes: &[u8]) -> Vec<Ump> {
    SysexMessage::from_framed(bytes)
        .unwrap()
        .packets(MidiGroup::of(group))
        .collect()
}

fn reader(own: u8) -> UniversalReader {
    UniversalReader::new(MidiGroup::of(0), SysexDeviceId::of(own))
}

fn feed(reader: &mut UniversalReader, umps: impl IntoIterator<Item = Ump>) -> Vec<Heard> {
    umps.into_iter().filter_map(|u| reader.push(u)).collect()
}

const VOLUME: [u8; 8] = [0xF0, 0x7F, 0x7F, 0x04, 0x01, 0x00, 0x40, 0xF7];
/// An Identity Reply: a 13-byte payload, so three SysEx7 packets.
const REPLY: [u8; 15] = [
    0xF0, 0x7E, 0x7F, 0x06, 0x02, 0x41, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0xF7,
];

/// M1 p.40: device id 7FH makes every device respond whatever id it is set to;
/// otherwise only the device with that id does.
#[test]
fn the_all_call_and_the_own_id_are_heard_and_no_other() {
    let mut r = reader(5);
    let to = |id: u8| {
        let mut bytes = VOLUME;
        bytes[2] = id;
        bytes
    };
    let all = feed(&mut r, packets(0, &to(0x7F)));
    assert_eq!(
        all.iter().map(|h| h.to).collect::<Vec<_>>(),
        [Addressee::All]
    );
    let own = feed(&mut r, packets(0, &to(5)));
    assert_eq!(
        own.iter().map(|h| h.to).collect::<Vec<_>>(),
        [Addressee::Own]
    );
    assert!(feed(&mut r, packets(0, &to(6))).is_empty());
}

/// A message longer than one packet is reassembled (M2-104 section 7.7): it is
/// heard when its End arrives, once.
#[test]
fn a_message_in_several_packets_is_heard_at_its_end() {
    let mut r = reader(0);
    let umps = packets(0, &REPLY);
    assert!(umps.len() > 1, "the test needs a divided message");
    let (last, first) = umps.split_last().unwrap();
    assert!(feed(&mut r, first.iter().copied()).is_empty());
    assert_eq!(feed(&mut r, [*last]).len(), 1);
}

/// M2-104 section 7.7.1: a message of the Group that is not a Continue ends an
/// open SysEx, so a Note On in the middle drops it; a message of another
/// Group is interspersed freely.
#[test]
fn a_message_of_the_group_ends_an_open_one_and_another_group_does_not() {
    let note = Ump::from(Message::note_on(
        Address {
            group: MidiGroup::of(0),
            channel: MidiChannel::of(0),
        },
        NoteNumber::of(60),
        U7::of(64),
    ));
    let other_group_note = Ump::from(Message::note_on(
        Address {
            group: MidiGroup::of(1),
            channel: MidiChannel::of(0),
        },
        NoteNumber::of(60),
        U7::of(64),
    ));
    let umps = packets(0, &REPLY);
    let mut r = reader(0);
    let cut = [umps[0], note].into_iter().chain(umps[1..].iter().copied());
    assert!(feed(&mut r, cut).is_empty());
    assert_eq!(r.dropped(), 1);
    let mut r = reader(0);
    let kept = [umps[0], other_group_note]
        .into_iter()
        .chain(umps[1..].iter().copied());
    assert_eq!(feed(&mut r, kept).len(), 1);
    // The same message on another Group is not this reader's.
    assert!(feed(&mut reader(0), packets(1, &REPLY)).is_empty());
}

/// A message longer than the reader's room is dropped whole and counted, and
/// the next one is read.
#[test]
fn an_overlong_message_is_dropped_counted_and_the_next_is_read() {
    let mut long = vec![0xF0, 0x41];
    long.extend(std::iter::repeat_n(0x11, 600));
    long.push(0xF7);
    let mut r = reader(0);
    assert!(feed(&mut r, packets(0, &long)).is_empty());
    assert_eq!(r.dropped(), 1);
    assert_eq!(feed(&mut r, packets(0, &VOLUME)).len(), 1);
}

/// A Key-Based Tuning Dump is 409 bytes with its framing: it crosses the SysEx7
/// packets whole and is read; a copy with a bad checksum is counted and not
/// passed on, but only when it was for this device or for all.
#[test]
fn a_tuning_dump_crosses_the_reader_and_a_corrupt_one_is_counted() {
    let mut payload = vec![0x7E, 0x7F, 0x08, 0x04, 0x00, 0x00];
    payload.extend_from_slice(b"Equal           ");
    for k in 0..128u8 {
        payload.extend([k, 0, 0]);
    }
    payload.push(payload.iter().fold(0, |a, b| a ^ b) & 0x7F);
    let mut framed = vec![0xF0];
    framed.extend(&payload);
    framed.push(0xF7);
    assert_eq!(framed.len(), 409);

    let mut r = reader(0);
    let heard = feed(&mut r, packets(0, &framed));
    assert_eq!(heard.len(), 1);
    assert!(matches!(heard[0].message, Universal::Tuning(_)));
    assert_eq!(r.corrupt(), 0);

    // The checksum is wrong, for all devices: counted, not passed on.
    let n = framed.len();
    framed[n - 2] ^= 0x01;
    assert!(feed(&mut r, packets(0, &framed)).is_empty());
    assert_eq!(r.corrupt(), 1);
    // For another device it is none of this reader's business.
    framed[2] = 9;
    assert!(feed(&mut r, packets(0, &framed)).is_empty());
    assert_eq!(r.corrupt(), 1);
    assert_eq!(r.dropped(), 0);
}

/// **A device's own id is `00` to `7E`: `7F` is the all-call** (M1 p.40), which
/// every device answers and none has as its own, so it is refused as an id and
/// read as `Recipient::AllCall` in a header.
#[test]
fn the_all_call_is_no_devices_own_id() {
    assert_eq!(SysexDeviceId::new(U7::of(0x7F)), None);
    assert_eq!(
        SysexDeviceId::try_from(U7::of(0x7F)),
        Err(NotSysexDeviceId(U7::of(0x7F)))
    );
    assert_eq!(
        SysexDeviceId::new(U7::of(0x7E)).map(SysexDeviceId::get),
        Some(U7::of(0x7E))
    );
    assert_eq!(Recipient::of_byte(U7::of(0x7F)), Recipient::AllCall);
    assert_eq!(
        Recipient::of_byte(U7::of(0x7E)),
        Recipient::Device(SysexDeviceId::of(0x7E))
    );
}
