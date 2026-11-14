//! Universal System Exclusive on the unit: Master Volume and Balance (M1 p.57),
//! Master Fine and Coarse Tuning (CA-025) and General MIDI System On and Off
//! (M1 p.52, RP-003, GM2 section 4.9), read from the Group's SysEx7 packets and
//! filtered by device id (M1 p.40). The font's sample is a looped sine, 441 Hz at
//! key 69.

use crate::support;
use tutti_ump::ControlValue;

use std::sync::Arc;
use support::font::{Inst, Preset, font};
use support::rig::Rig;
use tutti_fixtures::sf2::generator;
use tutti_midi_protocol::SysexDeviceId;
use tutti_sf2::SoundFont;
use tutti_soundfont::Settings;
use tutti_ump::{MidiChannel, MidiGroup, Rpn, SysexMessage, Ump, Voice2};

/// `0:0` and `0:1` are the plain sine and an octave above it; the percussion
/// bank's `128:0` is one or the other, as `drums_up` says.
fn font_with(drums_up: bool) -> Arc<SoundFont> {
    let preset = |name, bank, program, instrument| Preset {
        name,
        bank,
        program,
        zones: vec![(vec![], instrument)],
    };
    font(
        &[
            preset("plain", 0, 0, 0),
            preset("octave", 0, 1, 1),
            preset("drums", 128, 0, usize::from(drums_up)),
        ],
        &[
            Inst {
                name: "plain",
                zones: vec![vec![]],
            },
            Inst {
                name: "octave",
                zones: vec![vec![(generator::COARSE_TUNE, 12)]],
            },
        ],
    )
}

fn sysex(group: u8, bytes: &[u8]) -> Vec<Ump> {
    SysexMessage::from_framed(bytes)
        .expect("a framed message")
        .packets(MidiGroup::of(group))
        .collect()
}

/// A Universal Real Time Device Control message to `device`.
fn device_control(device: u8, sub_id: u8, lsb: u8, msb: u8) -> Vec<u8> {
    vec![0xF0, 0x7F, device, 0x04, sub_id, lsb, msb, 0xF7]
}

const GM_ON: [u8; 6] = [0xF0, 0x7E, 0x7F, 0x09, 0x01, 0xF7];
const GM_OFF: [u8; 6] = [0xF0, 0x7E, 0x7F, 0x09, 0x02, 0xF7];

fn send(rig: &mut Rig, bytes: &[u8]) {
    rig.hand.queue_midi(&sysex(0, bytes));
}

/// The rising zero crossings in a second of key 69 on `channel`: the pitch in Hz.
fn pitch(rig: &mut Rig, channel: u8) -> usize {
    rig.note_on(channel, 69, 100);
    let (left, _) = rig.render(44_100);
    rig.cut_all();
    rig.render(512);
    left.windows(2)
        .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
        .count()
}

fn near(got: usize, want: usize, what: &str) {
    assert!(got.abs_diff(want) <= 3, "{what}: {got} Hz, wanted {want}");
}

/// The energy of the left and right sides over ten periods of the sine after
/// the note has settled.
fn energy(rig: &mut Rig) -> (f32, f32) {
    rig.render(4410);
    rig.energy_lr(4410)
}

/// GM2 section 4.1: the volume is the square of the value, and the all-call
/// reaches the unit whatever its own device id.
#[test]
fn master_volume_scales_every_plane_by_the_square_of_the_value() {
    let font = font_with(false);
    let settings = Settings::new().with_device(SysexDeviceId::of(5));
    let mut rig = Rig::with(&font, settings);
    rig.note_on(0, 69, 100);
    let (l0, r0) = energy(&mut rig);
    send(&mut rig, &device_control(0x7F, 0x01, 0x00, 0x40));
    rig.render(512);
    let (l1, r1) = energy(&mut rig);
    let expected = (8192.0_f32 / 16383.0).powi(2);
    assert!((l1 / l0 - expected).abs() < 0.01, "{}", l1 / l0);
    assert!((r1 / r0 - expected).abs() < 0.01, "{}", r1 / r0);
}

/// M1 p.40: a message for another device id is not this unit's; its own is.
#[test]
fn only_the_all_call_and_the_units_own_id_are_heard() {
    let font = font_with(false);
    let settings = Settings::new().with_device(SysexDeviceId::of(5));
    let mut rig = Rig::with(&font, settings);
    rig.note_on(0, 69, 100);
    let (l0, _) = energy(&mut rig);
    send(&mut rig, &device_control(6, 0x01, 0x00, 0x00));
    rig.render(512);
    let (same, _) = energy(&mut rig);
    assert!((same / l0 - 1.0).abs() < 0.01, "device 6 is not 5");
    send(&mut rig, &device_control(5, 0x01, 0x00, 0x00));
    rig.render(512);
    let (off, _) = energy(&mut rig);
    assert!(off < l0 * 0.001, "its own id turns the volume off");
}

/// M1 p.57: balance `7F 7F` is hard right: the left side is gone.
#[test]
fn master_balance_hard_right_silences_the_left_side_only() {
    let mut rig = Rig::dry(&font_with(false));
    rig.note_on(0, 69, 100);
    let (l0, r0) = energy(&mut rig);
    assert!(l0 > 1.0 && r0 > 1.0);
    send(&mut rig, &device_control(0x7F, 0x02, 0x7F, 0x7F));
    rig.render(512);
    let (l, r) = energy(&mut rig);
    assert!(l < l0 * 0.001);
    assert!((r / r0 - 1.0).abs() < 0.01);
}

/// CA-025: Master Coarse Tuning `00 4C` is +12 semitones and Master Fine Tuning
/// `00 00` is -100 cents, for a note struck after the message.
#[test]
fn master_tuning_moves_the_notes_of_a_melodic_channel() {
    let font = font_with(false);
    let mut rig = Rig::dry(&font);
    send(&mut rig, &device_control(0x7F, 0x04, 0x00, 0x4C));
    near(pitch(&mut rig, 0), 882, "coarse +12");
    send(&mut rig, &device_control(0x7F, 0x04, 0x00, 0x40));
    send(&mut rig, &device_control(0x7F, 0x03, 0x00, 0x00));
    // 441 Hz down 100 cents.
    near(pitch(&mut rig, 0), 416, "fine -100 cents");
}

/// CA-025 and GM2 section 4.2: the percussion channel keeps its pitch (a key
/// there is another sound, not another pitch).
#[test]
fn master_tuning_leaves_the_percussion_channel() {
    let mut rig = Rig::dry(&font_with(false));
    send(&mut rig, &device_control(0x7F, 0x04, 0x00, 0x4C));
    near(pitch(&mut rig, 9), 441, "channel 10");
    near(pitch(&mut rig, 0), 882, "channel 1");
}

/// M1 p.52 and RP-003: General MIDI System On returns every channel to program
/// 0 of bank 0, and the percussion channel (channel 10) to the drum bank, 128;
/// master tuning goes back to A440.
#[test]
fn general_midi_on_returns_programs_banks_and_tuning_to_the_defaults() {
    let font = font_with(true);
    let mut rig = Rig::dry(&font);
    near(pitch(&mut rig, 9), 882, "the drum bank is the octave one");
    // The host puts the percussion channel on a melodic preset, and channel 1 on
    // program 1 (the octave instrument), and tunes the device up.
    rig.hand
        .select(MidiChannel::of(9), &font.presets()[0])
        .unwrap();
    rig.program(0, 1);
    send(&mut rig, &device_control(0x7F, 0x04, 0x00, 0x4C));
    rig.render(512);
    near(pitch(&mut rig, 9), 441 * 2, "channel 10 melodic, tuned up");
    send(&mut rig, &GM_ON);
    rig.render(512);
    near(pitch(&mut rig, 0), 441, "program 0 of bank 0, at A440");
    near(pitch(&mut rig, 9), 882, "channel 10 is the drum bank again");
}

/// GM2 section 4.9: General MIDI On mutes every sounding note.
#[test]
fn general_midi_on_mutes_the_sounding_notes() {
    let mut rig = Rig::dry(&font_with(false));
    rig.note_on(0, 60, 100);
    rig.note_on(5, 64, 100);
    assert!(rig.energy(512) > 1.0);
    send(&mut rig, &GM_ON);
    rig.render(64);
    assert_eq!(rig.energy(512), 0.0);
}

/// GM2 section 4.9.3: General MIDI Off returns the device to its initialized
/// mode, here the same unit with nothing selected: programs and the master
/// tuning back at their defaults.
#[test]
fn general_midi_off_also_returns_to_the_initial_status() {
    let mut rig = Rig::dry(&font_with(false));
    rig.program(0, 1);
    send(&mut rig, &device_control(0x7F, 0x04, 0x00, 0x4C));
    rig.render(512);
    send(&mut rig, &GM_OFF);
    rig.render(512);
    near(pitch(&mut rig, 0), 441, "program 0 at A440");
}

/// Another Group's System Exclusive is not this unit's.
#[test]
fn another_groups_message_is_not_this_units() {
    let mut rig = Rig::dry(&font_with(false));
    rig.hand
        .queue_midi(&sysex(1, &device_control(0x7F, 0x04, 0x00, 0x4C)));
    near(pitch(&mut rig, 0), 441, "the first Group is untouched");
}

// ---- MIDI Tuning Standard (MIDI L7b part 2) ----

/// A Single Note Tuning Change (`08 02` real time under `7F`, the bank form
/// `08 07` under `7E`, bank 0 and program 0) for key 69 to the semitone
/// `semitone` (the frequency data `semitone 00 00`).
fn retune_69(realtime: bool, semitone: u8) -> Vec<u8> {
    if realtime {
        vec![
            0xF0, 0x7F, 0x7F, 0x08, 0x02, 0x00, 0x01, 69, semitone, 0, 0, 0xF7,
        ]
    } else {
        vec![
            0xF0, 0x7E, 0x7F, 0x08, 0x07, 0x00, 0x00, 0x01, 69, semitone, 0, 0, 0xF7,
        ]
    }
}

/// The rising zero crossings in a second of what `rig` renders now.
fn crossings(rig: &mut Rig) -> usize {
    let (left, _) = rig.render(44_100);
    left.windows(2)
        .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
        .count()
}

/// MTS: a real-time Single Note Tuning Change retunes a sounding voice without
/// a new attack (key 69, 441 Hz, to the pitch of key 81: an octave), and one
/// struck after has it.
#[test]
fn a_real_time_note_change_retunes_a_sounding_voice_and_the_next() {
    let mut rig = Rig::dry(&font_with(false));
    rig.note_on(0, 69, 100);
    rig.render(4410);
    near(crossings(&mut rig), 441, "before");
    send(&mut rig, &retune_69(true, 81));
    rig.render(4410);
    near(crossings(&mut rig), 882, "the sounding note, an octave up");
    near(pitch(&mut rig, 1), 882, "a note struck after");
}

/// CA-020: the non-real-time form leaves the sounding notes alone: the voice
/// stays at 441 Hz and the next note is at the new pitch.
#[test]
fn a_non_real_time_note_change_leaves_the_sounding_voice() {
    let mut rig = Rig::dry(&font_with(false));
    rig.note_on(0, 69, 100);
    rig.render(4410);
    send(&mut rig, &retune_69(false, 81));
    rig.render(4410);
    near(crossings(&mut rig), 441, "the sounding note");
    rig.cut_all();
    rig.render(512);
    near(pitch(&mut rig, 0), 882, "the next note");
}

/// MTS: RPN 3 selects the channel's tuning, per basic channel: channel 0 goes
/// to program 1 (key 69 an octave up) while channel 1 stays on program 0.
#[test]
fn a_tuning_program_select_is_per_channel_and_instant() {
    let mut rig = Rig::dry(&font_with(false));
    let program1 = [
        0xF0, 0x7E, 0x7F, 0x08, 0x07, 0x00, 0x01, 0x01, 69, 81, 0, 0, 0xF7,
    ];
    send(&mut rig, &program1);
    rig.note_on(0, 69, 100);
    rig.render(4410);
    rig.voice2(
        0,
        Voice2::Registered {
            bank: Rpn::TUNING_PROGRAM_SELECT.bank(),
            index: Rpn::TUNING_PROGRAM_SELECT.index(),
            value: ControlValue::new(1 << 25),
        },
    );
    rig.render(4410);
    near(crossings(&mut rig), 882, "channel 0, on program 1");
    rig.cut_all();
    rig.render(512);
    near(pitch(&mut rig, 1), 441, "channel 1, on program 0");
}

/// CA-021: Scale/Octave Tuning, 1 byte, real time, for channel 1 only (`hh` bit
/// 0): +50 cents on A (pitch class 9). The 2-byte form states the same offset
/// as `60 00` (+50 of 100 cents).
#[test]
fn scale_octave_tuning_offsets_the_named_channel_by_pitch_class() {
    let mut rig = Rig::dry(&font_with(false));
    let mut one = vec![0xF0, 0x7F, 0x7F, 0x08, 0x08, 0x00, 0x00, 0x01];
    one.extend([
        0x40,
        0x40,
        0x40,
        0x40,
        0x40,
        0x40,
        0x40,
        0x40,
        0x40,
        0x40 + 50,
        0x40,
        0x40,
    ]);
    one.push(0xF7);
    send(&mut rig, &one);
    // 441 Hz up 50 cents.
    near(pitch(&mut rig, 0), 454, "channel 1, +50 cents on A");
    near(pitch(&mut rig, 1), 441, "channel 2 is not named");
    let mut two = vec![0xF0, 0x7F, 0x7F, 0x08, 0x09, 0x00, 0x00, 0x01];
    for class in 0..12 {
        two.extend(if class == 9 {
            [0x60, 0x00]
        } else {
            [0x40, 0x00]
        });
    }
    two.push(0xF7);
    send(&mut rig, &two);
    near(pitch(&mut rig, 0), 454, "the 2-byte form, the same offset");
}
