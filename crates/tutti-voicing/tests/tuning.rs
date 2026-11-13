//! The MIDI Tuning Standard in the note logic: the tuning programs, Tuning
//! Program and Bank Select (RPN 3 and 4), Single Note Tuning Change and
//! Scale/Octave Tuning, and what each says to a voice that sounds and one that
//! starts. A key's tuning is its offset from equal temperament in semitones.

use tutti_midi_protocol::{
    Addressee, ChannelMask, Heard, KeyChange, KeyChanges, KeyTable, OctaveOffsets, Timing,
    TuningFrequency, TuningMessage, TuningName, Universal,
};
use tutti_types::{Amplitude, Seconds};
use tutti_ump::ControlValue;
use tutti_ump::{Attribute, CCNumber, MidiChannel, NoteNumber, NoteVelocity, Rpn, U7, U14, Voice2};
use tutti_voicing::{
    Command, NoSuchTuning, Patch, Rules, TuningCapacity, VoiceId, VoiceSpec, Voicing,
};

struct One;
impl Patch for One {
    fn voices(&self, _: MidiChannel, _: NoteNumber, _: u16, spec: &mut dyn FnMut(VoiceSpec)) {
        spec(VoiceSpec {
            group: None,
            scope: 0,
            tag: 0,
        });
    }
}

fn step(core: &mut Voicing, channel: u8, message: Voice2) -> Vec<Command> {
    core.step(MidiChannel::of(channel), message, &One, |_| {
        Some(Amplitude(1.0))
    })
    .to_vec()
}

fn on(core: &mut Voicing, channel: u8, key: u8) -> Vec<Command> {
    step(
        core,
        channel,
        Voice2::NoteOn {
            note: NoteNumber::of(key),
            velocity: NoteVelocity::new(0x8000),
            attribute: Attribute::None,
        },
    )
}

fn say(core: &mut Voicing, message: TuningMessage) -> Vec<Command> {
    core.universal(Heard {
        to: Addressee::All,
        message: Universal::Tuning(message),
    })
    .to_vec()
}

fn rpn(core: &mut Voicing, channel: u8, which: Rpn, value: u8) -> Vec<Command> {
    step(
        core,
        channel,
        Voice2::Registered {
            bank: which.bank(),
            index: which.index(),
            value: ControlValue::new(u32::from(value) << 25),
        },
    )
}

/// The tuning a note's `Start` carries.
fn start_tune(commands: &[Command]) -> f32 {
    commands
        .iter()
        .find_map(|c| match c {
            Command::Start { tune, .. } => Some(tune.0),
            _ => None,
        })
        .expect("a Start")
}

/// What the `Tune` commands say, by voice.
fn tunes(commands: &[Command]) -> Vec<(u64, f32)> {
    commands
        .iter()
        .filter_map(|c| match c {
            Command::Tune { id, tune } => Some((id_of(*id), tune.0)),
            _ => None,
        })
        .collect()
}

fn id_of(id: VoiceId) -> u64 {
    id.get()
}

/// The frequency data word of a pitch in semitones and steps of 1/16384.
fn word(semitone: u8, steps: u16) -> TuningFrequency {
    TuningFrequency::from_parts(U7::of(semitone), U14::of(steps))
}

fn change(key: u8, frequency: TuningFrequency) -> KeyChange {
    KeyChange {
        key: NoteNumber::of(key),
        frequency,
    }
}

fn note_change(
    timing: Timing,
    bank: Option<u8>,
    program: u8,
    changes: &[KeyChange],
) -> TuningMessage {
    let mut all = KeyChanges::new();
    for &c in changes {
        assert!(all.push(c));
    }
    TuningMessage::NoteChange {
        timing,
        bank: bank.map(U7::of),
        program: U7::of(program),
        changes: all,
    }
}

fn octave(timing: Timing, channels: u16, offsets: OctaveOffsets) -> TuningMessage {
    TuningMessage::OctaveTuning {
        timing,
        channels: ChannelMask::from_bits(channels),
        offsets,
    }
}

/// 1-byte offsets: `cents` over C to B, `0x40` being zero.
fn cents(of: &[(usize, i8)]) -> OctaveOffsets {
    let mut bytes = [U7::of(0x40); 12];
    for &(class, c) in of {
        bytes[class] = U7::of((0x40 + c) as u8);
    }
    OctaveOffsets::Cents(bytes)
}

const SAMPLE: Rules = Rules::poly(8);

/// RP-020: "the initial tuning of the instrument is equal temperament": every
/// key of every program starts at its own number, so a note is not retuned.
#[test]
fn every_program_starts_in_equal_temperament() {
    let mut core = Voicing::new(SAMPLE);
    assert_eq!(start_tune(&on(&mut core, 0, 61)), 0.0);
    let table = core.tuning(U7::of(0), U7::of(127)).unwrap();
    assert_eq!(table, &KeyTable::equal_temperament());
}

/// MTS, Single Note Tuning Change (real time, `08 02`, no bank): bank 0 is the
/// one without a bank byte. `3D 00 00` for key 60 is note 61's pitch: a
/// semitone up. "No change" for another key leaves it where it was. A note
/// struck afterwards on the channel (which plays program 0 of bank 0) has it.
#[test]
fn a_single_note_change_retunes_the_next_note_on_the_programs_channels() {
    let mut core = Voicing::new(SAMPLE);
    let out = say(
        &mut core,
        note_change(
            Timing::RealTime,
            None,
            0,
            &[
                change(60, word(61, 0)),
                change(62, TuningFrequency::NO_CHANGE),
            ],
        ),
    );
    assert!(out.is_empty(), "nothing sounds");
    assert_eq!(start_tune(&on(&mut core, 0, 60)), 1.0);
    assert_eq!(start_tune(&on(&mut core, 3, 62)), 0.0, "no change");
    assert_eq!(start_tune(&on(&mut core, 0, 61)), 0.0, "another key");
}

/// The fraction is a hundredth of a cent's worth of semitone: `45 40 00` is half
/// a semitone (50 cents) above note 69's pitch.
#[test]
fn the_fraction_of_a_word_is_a_fraction_of_a_semitone() {
    let mut core = Voicing::new(SAMPLE);
    say(
        &mut core,
        note_change(Timing::RealTime, None, 0, &[change(69, word(69, 0x2000))]),
    );
    assert_eq!(start_tune(&on(&mut core, 0, 69)), 0.5);
}

/// MTS: "When a sounding note is affected by either real-time tuning message,
/// the note should instantly be re-tuned [...] while it continues to sound": a
/// real-time change tells the voice (`Tune`, no new attack, no cut), once, and
/// only the voices of the key and of channels that play the program.
#[test]
fn a_real_time_change_retunes_the_sounding_note_and_only_it() {
    let mut core = Voicing::new(SAMPLE);
    on(&mut core, 0, 60);
    on(&mut core, 0, 64);
    on(&mut core, 1, 60);
    // Channel 1 plays program 1.
    rpn(&mut core, 1, Rpn::TUNING_PROGRAM_SELECT, 1);
    let out = say(
        &mut core,
        note_change(
            Timing::RealTime,
            Some(0),
            0,
            &[change(60, word(60, 0x1000))],
        ),
    );
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(tunes(&out), [(1, 0.25)], "voice 1: key 60 on channel 0");
    // Said again, nothing moves.
    assert!(
        say(
            &mut core,
            note_change(
                Timing::RealTime,
                Some(0),
                0,
                &[change(60, word(60, 0x1000))]
            )
        )
        .is_empty()
    );
}

/// CA-020: the non-real-time form "will NOT update the currently sounding
/// notes": the sounding voice is left, the next Note On has the new tuning.
#[test]
fn a_non_real_time_change_leaves_the_sounding_note() {
    let mut core = Voicing::new(SAMPLE);
    on(&mut core, 0, 60);
    let out = say(
        &mut core,
        note_change(Timing::NonRealTime, Some(0), 0, &[change(60, word(62, 0))]),
    );
    assert!(out.is_empty(), "{out:?}");
    assert_eq!(start_tune(&on(&mut core, 0, 60)), 2.0);
}

/// MTS, Changing Tuning Programs: RPN 3 and 4 select the "current" tuning
/// "instantly", on the channel. A sounding note of the channel follows (`Tune`),
/// a note of another channel does not, and a note struck after is by the new
/// program. "The basic channel number" discriminates between active tunings.
#[test]
fn rpn_3_and_4_select_the_channels_tuning_and_retune_its_sounding_notes() {
    let mut core =
        Voicing::new(Rules::poly(8).with_tuning_capacity(TuningCapacity::new(2, 4).unwrap()));
    // Program 2 of bank 1 raises key 60 by a semitone; program 2 of bank 0 by two.
    for (bank, up) in [(0, 62), (1, 61)] {
        say(
            &mut core,
            note_change(
                Timing::NonRealTime,
                Some(bank),
                2,
                &[change(60, word(up, 0))],
            ),
        );
    }
    on(&mut core, 0, 60);
    on(&mut core, 1, 60);
    let out = rpn(&mut core, 0, Rpn::TUNING_PROGRAM_SELECT, 2);
    assert_eq!(tunes(&out), [(1, 2.0)]);
    let out = rpn(&mut core, 0, Rpn::TUNING_BANK_SELECT, 1);
    assert_eq!(tunes(&out), [(1, 1.0)], "bank 1 now");
    assert_eq!(start_tune(&on(&mut core, 0, 60)), 1.0);
    assert_eq!(
        start_tune(&on(&mut core, 2, 60)),
        0.0,
        "channel 2 is on program 0"
    );
    assert_eq!(core.channel(MidiChannel::of(0)).tuning_program(), U7::of(2));
    assert_eq!(core.channel(MidiChannel::of(0)).tuning_bank(), U7::of(1));
}

/// MTS: "If an instrument receives a Tuning Program or Bank number for which it
/// has no Program or Bank, it should ignore that message." A core of one bank of
/// 4 programs ignores program 4 and bank 1, and counts each.
#[test]
fn a_program_or_bank_the_core_has_none_of_is_ignored_and_counted() {
    let mut core =
        Voicing::new(Rules::poly(4).with_tuning_capacity(TuningCapacity::new(1, 4).unwrap()));
    on(&mut core, 0, 60);
    assert!(rpn(&mut core, 0, Rpn::TUNING_PROGRAM_SELECT, 4).is_empty());
    assert!(rpn(&mut core, 0, Rpn::TUNING_BANK_SELECT, 1).is_empty());
    assert_eq!(core.tuning_refused(), 2);
    assert_eq!(core.channel(MidiChannel::of(0)).tuning_program(), U7::of(0));
    assert_eq!(core.channel(MidiChannel::of(0)).tuning_bank(), U7::of(0));
    // The last program there is is taken.
    rpn(&mut core, 0, Rpn::TUNING_PROGRAM_SELECT, 3);
    assert_eq!(core.channel(MidiChannel::of(0)).tuning_program(), U7::of(3));
    assert_eq!(core.tuning_refused(), 2);
}

/// The same refusal for what is stored: a Single Note Tuning Change or a dump
/// for a program past the capacity changes nothing and is counted; a host's own
/// `store_tuning` answers with the error instead.
#[test]
fn a_change_for_a_program_there_is_no_room_for_is_refused_and_counted() {
    let mut core =
        Voicing::new(Rules::poly(4).with_tuning_capacity(TuningCapacity::new(1, 4).unwrap()));
    let refused = |core: &mut Voicing, bank: Option<u8>, program: u8| {
        let before = core.tuning_refused();
        say(
            core,
            note_change(
                Timing::NonRealTime,
                bank,
                program,
                &[change(60, word(70, 0))],
            ),
        );
        core.tuning_refused() - before
    };
    assert_eq!(refused(&mut core, Some(0), 4), 1);
    assert_eq!(refused(&mut core, Some(1), 0), 1);
    assert_eq!(refused(&mut core, None, 99), 1);
    assert_eq!(refused(&mut core, Some(0), 3), 0, "the last one exists");
    assert_eq!(
        core.store_tuning(U7::of(0), U7::of(4), KeyTable::equal_temperament()),
        Err(NoSuchTuning {
            bank: U7::of(0),
            program: U7::of(4)
        })
    );
    assert_eq!(core.tuning_refused(), 3, "the host's call is not counted");
    assert!(
        core.store_tuning(U7::of(0), U7::of(3), KeyTable::equal_temperament())
            .is_ok()
    );
}

/// `TuningCapacity` is 1 to 128 of each.
#[test]
fn a_capacity_is_one_to_128_banks_and_programs() {
    assert!(TuningCapacity::new(0, 1).is_none());
    assert!(TuningCapacity::new(1, 0).is_none());
    assert!(TuningCapacity::new(129, 1).is_none());
    assert!(TuningCapacity::new(1, 129).is_none());
    assert_eq!(TuningCapacity::new(128, 128).unwrap().banks(), 128);
    assert_eq!(
        TuningCapacity::DEFAULT,
        TuningCapacity::new(1, 128).unwrap()
    );
}

/// CA-021: Scale/Octave Tuning "will offset the tuning from the currently
/// selected preset"; the offsets repeat every octave (key 61 and key 73 are both
/// C#), and "each message that is received will offset from the original preset,
/// not from the modified tuning": a second message replaces the first.
#[test]
fn scale_octave_offsets_repeat_every_octave_and_replace_the_last_message() {
    let mut core = Voicing::new(SAMPLE);
    say(
        &mut core,
        octave(Timing::NonRealTime, 0b1, cents(&[(1, 30)])),
    );
    assert!((start_tune(&on(&mut core, 0, 61)) - 0.30).abs() < 1e-6);
    assert!((start_tune(&on(&mut core, 0, 73)) - 0.30).abs() < 1e-6);
    assert_eq!(start_tune(&on(&mut core, 0, 62)), 0.0);
    say(
        &mut core,
        octave(Timing::NonRealTime, 0b1, cents(&[(2, -64)])),
    );
    assert_eq!(start_tune(&on(&mut core, 0, 61)), 0.0, "not accumulated");
    assert!((start_tune(&on(&mut core, 0, 62)) + 0.64).abs() < 1e-6);
}

/// CA-021: a message "can update multiple MIDI channels simultaneously", one bit
/// a channel; the others keep theirs. Real time: "notes that are already
/// sounding should be updated", on the named channels only.
#[test]
fn the_channel_mask_picks_the_channels_and_real_time_moves_their_sounding_notes() {
    let mut core = Voicing::new(SAMPLE);
    on(&mut core, 0, 60);
    on(&mut core, 1, 60);
    on(&mut core, 15, 60);
    // Channels 1 and 16 (bits 0 and 15), 50 cents on C: the 2-byte `60 00`.
    let mut words = [U14::of(0x2000); 12];
    words[0] = U14::of(0x3000);
    let out = say(
        &mut core,
        octave(Timing::RealTime, 0x8001, OctaveOffsets::Fine(words)),
    );
    assert_eq!(tunes(&out), [(1, 0.5), (3, 0.5)]);
    assert_eq!(
        start_tune(&on(&mut core, 1, 60)),
        0.0,
        "channel 2 is not named"
    );
    assert_eq!(start_tune(&on(&mut core, 15, 60)), 0.5);
}

/// CA-021: the non-real-time form is a setup message: it does not move a
/// sounding note.
#[test]
fn non_real_time_scale_octave_leaves_the_sounding_notes() {
    let mut core = Voicing::new(SAMPLE);
    on(&mut core, 0, 60);
    let out = say(
        &mut core,
        octave(Timing::NonRealTime, 0xFFFF, cents(&[(0, 10)])),
    );
    assert!(out.is_empty());
    assert!((start_tune(&on(&mut core, 0, 60)) - 0.10).abs() < 1e-6);
}

/// A key's tuning is its program's and its channel's offset, summed: key 60 up a
/// semitone in the table and 20 cents by the octave offset.
#[test]
fn the_programs_tuning_and_the_channels_octave_offset_add() {
    let mut core = Voicing::new(SAMPLE);
    say(
        &mut core,
        note_change(Timing::NonRealTime, None, 0, &[change(60, word(61, 0))]),
    );
    say(
        &mut core,
        octave(Timing::NonRealTime, 0b1, cents(&[(0, 20)])),
    );
    assert!((start_tune(&on(&mut core, 0, 60)) - 1.2).abs() < 1e-6);
}

/// `08 01`/`08 04`, the dumps, store all 128 keys, with "no change" words leaving
/// what is stored, and sound at the next Note On, not now.
#[test]
fn a_key_dump_stores_the_table_and_no_change_keeps_a_key() {
    let mut core = Voicing::new(SAMPLE);
    say(
        &mut core,
        note_change(Timing::NonRealTime, None, 5, &[change(50, word(53, 0))]),
    );
    let mut words = [TuningFrequency::NO_CHANGE; 128];
    for k in 0..128u8 {
        if k != 50 {
            words[usize::from(k)] = word(k.saturating_sub(1), 0);
        }
    }
    on(&mut core, 0, 70);
    rpn(&mut core, 0, Rpn::TUNING_PROGRAM_SELECT, 5);
    let out = say(
        &mut core,
        TuningMessage::KeyDump {
            bank: Some(U7::of(0)),
            program: U7::of(5),
            name: TuningName::new([U7::of(b'a'); 16]),
            table: KeyTable::new(words),
        },
    );
    assert!(out.is_empty(), "a dump is a setup message");
    assert_eq!(start_tune(&on(&mut core, 0, 70)), -1.0);
    assert_eq!(start_tune(&on(&mut core, 0, 50)), 3.0, "no change kept it");
}

/// The Scale/Octave Tuning Dump stores a program as equal temperament with the
/// twelve offsets; a channel that plays it hears them.
#[test]
fn an_octave_dump_stores_equal_temperament_plus_the_offsets() {
    let mut core = Voicing::new(SAMPLE);
    say(
        &mut core,
        TuningMessage::OctaveDump {
            bank: U7::of(0),
            program: U7::of(2),
            name: TuningName::new([U7::of(b'x'); 16]),
            offsets: cents(&[(9, -20)]),
        },
    );
    rpn(&mut core, 0, Rpn::TUNING_PROGRAM_SELECT, 2);
    // A is class 9: key 69 is 20 cents flat, to the word's resolution.
    assert!((start_tune(&on(&mut core, 0, 69)) + 0.20).abs() < 1e-3);
    assert_eq!(start_tune(&on(&mut core, 0, 70)), 0.0);
}

/// A Bulk Tuning Dump Request is parsed and answered with nothing: an instrument
/// has no output to reply on.
#[test]
fn a_dump_request_does_nothing_here() {
    let mut core = Voicing::new(SAMPLE);
    on(&mut core, 0, 60);
    let out = say(
        &mut core,
        TuningMessage::DumpRequest {
            bank: Some(U7::of(0)),
            program: U7::of(0),
        },
    );
    assert!(out.is_empty());
    assert_eq!(core.tuning_refused(), 0);
}

/// The percussion channel is key-based: its keys are sounds, not pitches, so
/// no tuning of a key reaches it (as the master tuning does not).
#[test]
fn the_percussion_channel_takes_no_key_tuning() {
    let mut core = Voicing::new(Rules::poly(4).with_percussion(MidiChannel::of(9)));
    say(
        &mut core,
        note_change(Timing::NonRealTime, None, 0, &[change(40, word(45, 0))]),
    );
    say(
        &mut core,
        octave(Timing::NonRealTime, 0xFFFF, cents(&[(4, 33)])),
    );
    assert_eq!(start_tune(&on(&mut core, 9, 40)), 0.0);
    assert_ne!(start_tune(&on(&mut core, 0, 40)), 0.0);
}

/// The master tuning and a key's tuning are two terms: the master moves the
/// channel's `Retune`, the key's tuning is the voice's, and neither contains the
/// other.
#[test]
fn a_keys_tuning_is_apart_from_the_channels_pitch() {
    let mut core = Voicing::new(SAMPLE);
    say(
        &mut core,
        note_change(Timing::NonRealTime, None, 0, &[change(60, word(61, 0))]),
    );
    let out = core
        .universal(Heard {
            to: Addressee::All,
            message: Universal::MasterCoarseTuning(U7::of(0x42)),
        })
        .to_vec();
    assert!(
        out.iter()
            .all(|c| matches!(c, Command::Retune { bend, .. } if bend.0 == 2.0))
    );
    assert_eq!(start_tune(&on(&mut core, 0, 60)), 1.0);
}

/// A legato move to another key is at that key's tuning, and a glide leaves the
/// key it leaves at that key's tuning.
#[test]
fn a_retarget_and_a_glide_carry_the_tuning_of_their_keys() {
    let mut core = Voicing::new(Rules::legato());
    say(
        &mut core,
        note_change(
            Timing::NonRealTime,
            None,
            0,
            &[change(60, word(60, 0x1000)), change(64, word(63, 0))],
        ),
    );
    // Portamento on: the glide leaves the key last struck.
    step(
        &mut core,
        0,
        Voice2::ControlChange {
            controller: CCNumber::PORTAMENTO_SWITCH,
            value: ControlValue::new(127 << 25),
        },
    );
    assert_eq!(start_tune(&on(&mut core, 0, 60)), 0.25);
    let out = on(&mut core, 0, 64);
    let Some(Command::Retarget { tune, glide, .. }) =
        out.iter().find(|c| matches!(c, Command::Retarget { .. }))
    else {
        panic!("{out:?}");
    };
    assert_eq!(tune.0, -1.0, "key 64 sounds as 63");
    let glide = glide.expect("portamento");
    assert_eq!(glide.from.get(), 60);
    assert_eq!(glide.from_tune.0, 0.25);
    assert!(glide.time > Seconds(0.0));
}

/// MTS stores tuning programs, not channel controls: General MIDI On returns the
/// channels to their initial status, program 0 of bank 0 and no octave offset
/// (RP-020's default), and what the programs hold stays.
#[test]
fn general_midi_on_returns_the_channels_to_program_zero_and_keeps_the_tables() {
    let mut core = Voicing::new(SAMPLE);
    say(
        &mut core,
        note_change(Timing::NonRealTime, None, 3, &[change(60, word(65, 0))]),
    );
    rpn(&mut core, 0, Rpn::TUNING_PROGRAM_SELECT, 3);
    say(&mut core, octave(Timing::NonRealTime, 1, cents(&[(0, 5)])));
    assert_eq!(start_tune(&on(&mut core, 0, 60)), 5.05);
    core.universal(Heard {
        to: Addressee::All,
        message: Universal::GeneralMidi(tutti_midi_protocol::GeneralMidi::On),
    });
    assert_eq!(core.channel(MidiChannel::of(0)).tuning_program(), U7::of(0));
    assert_eq!(start_tune(&on(&mut core, 0, 60)), 0.0);
    assert_eq!(
        core.tuning(U7::of(0), U7::of(3))
            .unwrap()
            .get(NoteNumber::of(60)),
        word(65, 0)
    );
}

/// A Relative Registered Controller (M2-104 7.4.8) nudges the program: +1 of the
/// MSB selects program 1.
#[test]
fn a_relative_registered_controller_steps_the_program() {
    let mut core = Voicing::new(SAMPLE);
    say(
        &mut core,
        note_change(Timing::NonRealTime, None, 1, &[change(60, word(61, 0))]),
    );
    step(
        &mut core,
        0,
        Voice2::RelativeRegistered {
            bank: Rpn::TUNING_PROGRAM_SELECT.bank(),
            index: Rpn::TUNING_PROGRAM_SELECT.index(),
            delta: 1 << 25,
        },
    );
    assert_eq!(core.channel(MidiChannel::of(0)).tuning_program(), U7::of(1));
    assert_eq!(start_tune(&on(&mut core, 0, 60)), 1.0);
}

/// The selection is controller state a successor takes over by chase (RPN 3 and
/// 4 as Registered Controllers on the channel), the tables are not.
#[test]
fn the_chase_carries_the_selected_program_and_bank() {
    let mut core =
        Voicing::new(Rules::poly(4).with_tuning_capacity(TuningCapacity::new(3, 8).unwrap()));
    rpn(&mut core, 5, Rpn::TUNING_PROGRAM_SELECT, 6);
    rpn(&mut core, 5, Rpn::TUNING_BANK_SELECT, 2);
    let mut sent = Vec::new();
    core.chase(tutti_ump::MidiGroup::FIRST, |ump| sent.push(ump));
    let mut heir =
        Voicing::new(Rules::poly(4).with_tuning_capacity(TuningCapacity::new(3, 8).unwrap()));
    for ump in sent {
        let tutti_ump::Message::Midi2(address, message) = ump.decode().unwrap() else {
            continue;
        };
        heir.step(address.channel, message, &One, |_| None);
    }
    let channel = heir.channel(MidiChannel::of(5));
    assert_eq!(
        (channel.tuning_program(), channel.tuning_bank()),
        (U7::of(6), U7::of(2))
    );
}
