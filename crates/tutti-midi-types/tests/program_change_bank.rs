//! The Program Change builder and its readers agree with M2-104 Figure 64:
//! Bank Valid at bit 0 of word 1, then in word 2 the program (30-24), the bank
//! MSB (14-8) and the bank LSB (6-0).
//!
//! Mutation (run): `MidiEvent::program_change` writing the halves swapped (MSB
//! at 6-0, LSB at 14-8, as `midi2`'s `set_bank` did) -> the first and second
//! tests fail. `program_change_bank_of` reading the halves swapped -> the third
//! fails.

use tutti_midi_types::ump::MidiEvent;
use tutti_ump::{Bank, Message, Ump, Voice2, decode};
use tutti_ump::{MidiChannel, MidiGroup};

fn u7(v: u8) -> tutti_ump::U7 {
    tutti_ump::U7::new(v).unwrap()
}

#[test]
fn the_builder_writes_figure_64() {
    // Group 2, channel 3, program 9, bank MSB 0x34, LSB 0x24 (bank 0x1A24).
    let ev = MidiEvent::program_change(
        MidiGroup::of(2),
        MidiChannel::of(3),
        9,
        Some((0x34 << 7) | 0x24),
    );
    assert_eq!(ev.data_words(), &[0x42C3_0001, 0x0900_3424]);
    let none = MidiEvent::program_change(MidiGroup::of(2), MidiChannel::of(3), 9, None);
    assert_eq!(none.data_words(), &[0x42C3_0000, 0x0900_0000]);
}

#[test]
fn the_builder_and_tutti_ump_agree_on_msb_and_lsb() {
    let ev = MidiEvent::program_change(MidiGroup::FIRST, MidiChannel::of(1), 9, Some((5 << 7) | 1));
    let Ok(Message::Midi2(_, Voice2::ProgramChange { program, bank })) = decode(Ump(ev.data))
    else {
        panic!("a Program Change");
    };
    assert_eq!(program, u7(9));
    assert_eq!(
        bank,
        Some(Bank {
            msb: u7(5),
            lsb: u7(1)
        })
    );
}

#[test]
fn the_readers_take_msb_and_lsb_from_figure_64() {
    let ev = MidiEvent::from_ump(0, &[0x41C2_0001, 0x0900_3424]);
    let bytes: Vec<[u8; 3]> = ev.to_midi1_messages().map(|(b, _)| b).collect();
    assert_eq!(bytes, vec![[0xB2, 0, 0x34], [0xB2, 32, 0x24], [0xC2, 9, 0]]);
    match ev.message() {
        tutti_midi_types::MidiMessage::ProgramChange { bank, program, .. } => {
            assert_eq!((bank, program), (Some((0x34 << 7) | 0x24), 9));
        }
        other => panic!("{other:?}"),
    }
}
