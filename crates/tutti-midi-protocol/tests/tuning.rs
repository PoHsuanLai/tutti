//! The MIDI Tuning Standard's grammar ("MIDI Tuning Updated Specification":
//! the original messages, CA-020 and CA-021), from the byte layouts it gives.

use tutti_midi_protocol::{
    BadChecksum, ChannelMask, KeyTable, OctaveOffsets, Read, Timing, TuningFrequency,
    TuningMessage, Universal, read,
};
use tutti_ump::{MidiChannel, NoteNumber, SysexMessage, U7, U14};

fn parse(payload: &[u8]) -> Read {
    let mut framed = vec![0xF0];
    framed.extend_from_slice(payload);
    framed.push(0xF7);
    read(SysexMessage::new(&framed).expect("a framed message"))
}

fn tuning(payload: &[u8]) -> TuningMessage {
    match parse(payload) {
        Read::Universal(a) => match a.message {
            Universal::Tuning(t) => t,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

/// The checksum field: "successively XOR'ing the bytes in the message,
/// excluding the F0, F7, and the checksum field [...] AND'ed with 7F".
fn checksum(payload: &[u8]) -> u8 {
    payload.iter().fold(0, |a, b| a ^ b) & 0x7F
}

/// A name of sixteen ASCII characters.
const NAME: &[u8; 16] = b"Just Intonation ";

/// The table the dump tests send: key `k` at `k` semitones and `k` steps.
fn table_bytes() -> (Vec<u8>, KeyTable) {
    let mut bytes = Vec::new();
    let mut words = [TuningFrequency::NO_CHANGE; 128];
    for k in 0..128u32 {
        let (fraction, semitone) = (k, k);
        bytes.extend([
            semitone as u8,
            (fraction >> 7) as u8,
            (fraction & 0x7F) as u8,
        ]);
        words[k as usize] = TuningFrequency::of(semitone << 14 | fraction);
    }
    (bytes, KeyTable::new(words))
}

/// MTS, "Frequency Data Format": `3C 00 00` is middle C, `45 00 00` is A440,
/// `45 00 01` one step (0.0061 cents) above it, and the fraction is sent most
/// significant byte first: `44 7F 7F` is the step below A440.
#[test]
fn frequency_data_is_a_semitone_and_a_fraction_most_significant_byte_first() {
    let change = |kk: u8, xx: u8, yy: u8, zz: u8| {
        let TuningMessage::NoteChange { changes, .. } =
            tuning(&[0x7F, 0x7F, 0x08, 0x02, 0x00, 0x01, kk, xx, yy, zz])
        else {
            panic!()
        };
        changes.as_slice()[0].frequency
    };
    assert_eq!(change(60, 0x3C, 0x00, 0x00).get(), 60 << 14);
    assert_eq!(change(69, 0x45, 0x00, 0x00).pitch().unwrap().get(), 69.0);
    assert_eq!(change(69, 0x45, 0x00, 0x01).get(), 69 << 14 | 1);
    assert_eq!(change(69, 0x44, 0x7F, 0x7F).get(), (69 << 14) - 1);
    // 01 00 00 and 00 01 00 differ: the middle byte is the fraction's high 7.
    assert_eq!(change(1, 0x00, 0x01, 0x00).get(), 128);
    assert_eq!(change(1, 0x7F, 0x7F, 0x7E), TuningFrequency::TOP);
}

/// MTS: "7F 7F 7F" is "no change" and is a value, not a sentinel the caller
/// compares bytes against.
#[test]
fn seven_f_seven_f_seven_f_is_the_no_change_word() {
    let TuningMessage::NoteChange { changes, .. } =
        tuning(&[0x7F, 0x7F, 0x08, 0x02, 0x00, 0x01, 0x30, 0x7F, 0x7F, 0x7F])
    else {
        panic!()
    };
    let word = changes.as_slice()[0].frequency;
    assert!(word.is_no_change() && word.pitch().is_none());
    assert_eq!(word, TuningFrequency::NO_CHANGE);
}

/// `F0 7E <device ID> 08 00 tt F7`, and the bank form `08 03 bb tt`.
#[test]
fn a_bulk_dump_request_names_a_program_and_maybe_a_bank() {
    assert_eq!(
        tuning(&[0x7E, 0x7F, 0x08, 0x00, 0x05]),
        TuningMessage::DumpRequest {
            bank: None,
            program: U7::of(5)
        }
    );
    assert_eq!(
        tuning(&[0x7E, 0x7F, 0x08, 0x03, 0x02, 0x7F]),
        TuningMessage::DumpRequest {
            bank: Some(U7::of(2)),
            program: U7::of(0x7F)
        }
    );
}

/// `F0 7E <device ID> 08 01 tt <tuning name> [xx yy zz] x128 chksum F7`: all
/// 128 keys in order from note 0, and a name of 16 characters. "It is now
/// recommended that receivers may ignore the checksum in that message": a wrong
/// one is still read.
#[test]
fn a_bulk_tuning_dump_is_128_words_and_its_checksum_may_be_ignored() {
    let (data, table) = table_bytes();
    let mut payload = vec![0x7E, 0x7F, 0x08, 0x01, 0x03];
    payload.extend_from_slice(NAME);
    payload.extend(&data);
    payload.push(0x00); // no valid checksum
    let TuningMessage::KeyDump {
        bank,
        program,
        name,
        table: read_table,
    } = tuning(&payload)
    else {
        panic!()
    };
    assert_eq!((bank, program), (None, U7::of(3)));
    assert_eq!(read_table, table);
    assert_eq!(name.bytes().map(U7::get), *NAME);
}

/// A 127-byte dump (one word short) is not one.
#[test]
fn a_dump_that_is_short_of_a_word_is_unread() {
    let (data, _) = table_bytes();
    let mut payload = vec![0x7E, 0x7F, 0x08, 0x01, 0x03];
    payload.extend_from_slice(NAME);
    payload.extend(&data[..data.len() - 3]);
    payload.push(0);
    assert_eq!(parse(&payload), Read::Unread);
}

/// `08 04 bb tt <name> [xx yy zz] x128 chksum`, "identical to the current BULK
/// TUNING DUMP except for the addition of the bank select byte": and for this
/// message the checksum is the XOR of every byte between `F0` and itself.
#[test]
fn a_key_based_dump_names_a_bank_and_is_checked() {
    let (data, table) = table_bytes();
    let mut payload = vec![0x7E, 0x00, 0x08, 0x04, 0x01, 0x02];
    payload.extend_from_slice(NAME);
    payload.extend(&data);
    payload.push(checksum(&payload));
    let TuningMessage::KeyDump {
        bank,
        program,
        table: read_table,
        ..
    } = tuning(&payload)
    else {
        panic!()
    };
    assert_eq!((bank, program), (Some(U7::of(1)), U7::of(2)));
    assert_eq!(read_table, table);
}

/// A wrong checksum on one of the checked dumps is its own outcome: not the
/// message, and not "unread" either. The spec does not say what a receiver does;
/// this says what was wrong and what it should have been.
#[test]
fn a_dump_with_a_bad_checksum_is_a_typed_outcome() {
    let (data, _) = table_bytes();
    let mut payload = vec![0x7E, 0x00, 0x08, 0x04, 0x01, 0x02];
    payload.extend_from_slice(NAME);
    payload.extend(&data);
    let good = checksum(&payload);
    payload.push(good ^ 0x01);
    let Read::BadChecksum(BadChecksum {
        expected, found, ..
    }) = parse(&payload)
    else {
        panic!("{:?}", parse(&payload));
    };
    assert_eq!((expected.get(), found.get()), (good, good ^ 1));
}

/// `08 05 bb tt <name> [xx] x12 chksum` (1 byte) and `08 06` (2 byte, most
/// significant byte first): the Scale/Octave Tuning Dumps, checksummed.
#[test]
fn the_scale_octave_dumps_are_twelve_offsets_and_a_checksum() {
    let mut one = vec![0x7E, 0x7F, 0x08, 0x05, 0x00, 0x09];
    one.extend_from_slice(NAME);
    let bytes: [u8; 12] = [
        0x40, 0x41, 0x42, 0x00, 0x7F, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40,
    ];
    one.extend(bytes);
    one.push(checksum(&one));
    let TuningMessage::OctaveDump {
        bank,
        program,
        offsets,
        ..
    } = tuning(&one)
    else {
        panic!()
    };
    assert_eq!((bank.get(), program.get()), (0, 9));
    assert_eq!(offsets, OctaveOffsets::Cents(bytes.map(U7::of)));

    let mut two = vec![0x7E, 0x7F, 0x08, 0x06, 0x01, 0x00];
    two.extend_from_slice(NAME);
    for i in 0..12u8 {
        two.extend([0x40, i]);
    }
    two.push(checksum(&two));
    let TuningMessage::OctaveDump { offsets, .. } = tuning(&two) else {
        panic!()
    };
    let OctaveOffsets::Fine(words) = offsets else {
        panic!()
    };
    assert_eq!(words[3], U14::of(0x2000 + 3));
    // A flipped bit is a bad checksum.
    *two.last_mut().unwrap() ^= 0x10;
    assert!(matches!(parse(&two), Read::BadChecksum(_)));
}

/// `F0 7F <device ID> 08 02 tt ll [kk xx yy zz] F7` is real time, no bank, and
/// "the total length of the message equals 8 + (ll x 4) bytes": a count that
/// disagrees with the bytes is not the message.
#[test]
fn a_single_note_change_carries_ll_changes_and_ll_is_its_length() {
    let TuningMessage::NoteChange {
        timing,
        bank,
        program,
        changes,
    } = tuning(&[
        0x7F, 0x7F, 0x08, 0x02, 0x04, 0x02, 0x3C, 0x3D, 0x00, 0x00, 0x40, 0x41, 0x40, 0x00,
    ])
    else {
        panic!()
    };
    assert_eq!((timing, bank, program), (Timing::RealTime, None, U7::of(4)));
    let got: Vec<(u8, u32)> = changes
        .as_slice()
        .iter()
        .map(|c| (c.key.get(), c.frequency.get()))
        .collect();
    assert_eq!(got, [(0x3C, 0x3D << 14), (0x40, 0x41 << 14 | 0x40 << 7)]);
    // ll = 2 with one change's bytes, and ll = 1 with two.
    assert_eq!(
        parse(&[0x7F, 0x7F, 0x08, 0x02, 0x04, 0x02, 0x3C, 0x3D, 0x00, 0x00]),
        Read::Unread
    );
    assert_eq!(
        parse(&[
            0x7F, 0x7F, 0x08, 0x02, 0x04, 0x01, 0x3C, 0x3D, 0x00, 0x00, 0x40, 0x41, 0x40, 0x00,
        ]),
        Read::Unread
    );
    // No changes at all is a message with none.
    let TuningMessage::NoteChange { changes, .. } = tuning(&[0x7F, 0x7F, 0x08, 0x02, 0x04, 0x00])
    else {
        panic!()
    };
    assert!(changes.as_slice().is_empty());
}

/// `08 07 bb tt ll [..]` is the bank form, real time under `7F` ("WILL affect
/// currently sounding notes") and non-real time under `7E` ("will NOT update the
/// currently sounding notes"); the non-real-time form has no `08 02`.
#[test]
fn the_bank_form_of_the_note_change_is_real_or_non_real_time_by_its_header() {
    let body = [0x08, 0x07, 0x02, 0x03, 0x01, 0x3C, 0x3C, 0x20, 0x00];
    let mut rt = vec![0x7F, 0x7F];
    rt.extend(body);
    let mut nrt = vec![0x7E, 0x7F];
    nrt.extend(body);
    let (
        TuningMessage::NoteChange {
            timing: a,
            bank: ba,
            ..
        },
        TuningMessage::NoteChange {
            timing: b,
            bank: bb,
            program,
            ..
        },
    ) = (tuning(&rt), tuning(&nrt))
    else {
        panic!()
    };
    assert_eq!((a, b), (Timing::RealTime, Timing::NonRealTime));
    assert_eq!(
        (ba, bb, program),
        (Some(U7::of(2)), Some(U7::of(2)), U7::of(3))
    );
    // 08 02 is defined in real time only.
    assert_eq!(parse(&[0x7E, 0x7F, 0x08, 0x02, 0x00, 0x00]), Read::Unread);
}

/// The Scale/Octave Tuning channel field: `ff` bits 0 and 1 are channels 15 and
/// 16, `gg` bits 0 to 6 channels 8 to 14, `hh` bits 0 to 6 channels 1 to 7;
/// "5 additional bits in one of the channel bytes, which must be set to 0".
#[test]
fn the_channel_bytes_name_channels_by_bit() {
    let channels = |ff: u8, gg: u8, hh: u8| {
        let mut p = vec![0x7F, 0x7F, 0x08, 0x08, ff, gg, hh];
        p.extend([0x40; 12]);
        match parse(&p) {
            Read::Universal(a) => match a.message {
                Universal::Tuning(TuningMessage::OctaveTuning { channels, .. }) => Some(
                    channels
                        .channels()
                        .map(MidiChannel::get)
                        .collect::<Vec<_>>(),
                ),
                other => panic!("{other:?}"),
            },
            Read::Unread => None,
            other => panic!("{other:?}"),
        }
    };
    // Numbered from 1 in the spec, 0-based here.
    assert_eq!(channels(0, 0, 0b000_0001), Some(vec![0]), "channel 1");
    assert_eq!(channels(0, 0, 0b100_0000), Some(vec![6]), "channel 7");
    assert_eq!(channels(0, 0b000_0001, 0), Some(vec![7]), "channel 8");
    assert_eq!(channels(0, 0b100_0000, 0), Some(vec![13]), "channel 14");
    assert_eq!(channels(0b01, 0, 0), Some(vec![14]), "channel 15");
    assert_eq!(channels(0b10, 0, 0), Some(vec![15]), "channel 16");
    assert_eq!(
        channels(0b11, 0x7F, 0x7F).map(|c| c.len()),
        Some(16),
        "all sixteen"
    );
    assert_eq!(channels(0, 0, 0), Some(vec![]));
    // A reserved bit set is not a message this reads.
    assert_eq!(channels(0b100, 0, 0), None);
    assert_eq!(channels(0b100_0000, 0, 0), None);
    assert!(ChannelMask::ALL.contains(MidiChannel::of(15)));
}

/// `08 08` is the 1-byte form (`00` is -64 cents, `40` zero, `7F` +63) and `08 09`
/// the 2-byte form (`40 00` zero), each real time under `7F` and non-real time
/// under `7E`.
#[test]
fn scale_octave_tuning_has_two_forms_and_two_timings() {
    let mut one = vec![0x7F, 0x7F, 0x08, 0x08, 0x00, 0x00, 0x01];
    one.extend([
        0x00, 0x40, 0x7F, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40,
    ]);
    let TuningMessage::OctaveTuning {
        timing, offsets, ..
    } = tuning(&one)
    else {
        panic!()
    };
    assert_eq!(timing, Timing::RealTime);
    assert_eq!(offsets.offset(0).get(), -64.0);
    assert_eq!(offsets.offset(1).get(), 0.0);
    assert_eq!(offsets.offset(2).get(), 63.0);

    let mut two = vec![0x7E, 0x7F, 0x08, 0x09, 0x00, 0x00, 0x01];
    for i in 0..12u8 {
        two.extend(if i == 4 { [0x7F, 0x7F] } else { [0x40, 0x00] });
    }
    let TuningMessage::OctaveTuning {
        timing, offsets, ..
    } = tuning(&two)
    else {
        panic!()
    };
    assert_eq!(timing, Timing::NonRealTime);
    assert_eq!(offsets.offset(0).get(), 0.0);
    assert!((offsets.offset(4).get() - 100.0).abs() < 0.02);
    // Twelve offsets and no fewer.
    two.truncate(two.len() - 2);
    assert_eq!(parse(&two), Read::Unread);
}

/// A key above 127 cannot be on the wire; a note-number byte of 7F is key 127.
#[test]
fn key_127_is_the_top_key() {
    let TuningMessage::NoteChange { changes, .. } =
        tuning(&[0x7F, 0x7F, 0x08, 0x02, 0x00, 0x01, 0x7F, 0x7F, 0x00, 0x00])
    else {
        panic!()
    };
    assert_eq!(changes.as_slice()[0].key, NoteNumber::of(127));
    assert_eq!(
        changes.as_slice()[0].frequency.pitch().unwrap().get(),
        127.0
    );
}

/// 127 changes (the most `ll` can say) fit one message.
#[test]
fn a_message_of_127_changes_is_read() {
    let mut p = vec![0x7F, 0x7F, 0x08, 0x07, 0x00, 0x00, 127];
    for k in 0..127u8 {
        p.extend([k, k, 0x00, 0x00]);
    }
    let TuningMessage::NoteChange { changes, .. } = tuning(&p) else {
        panic!()
    };
    assert_eq!(changes.as_slice().len(), 127);
    assert_eq!(changes.as_slice()[126].frequency.get(), 126 << 14);
}
