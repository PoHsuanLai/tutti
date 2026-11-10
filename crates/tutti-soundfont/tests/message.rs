//! What a decoded message does to the synthesizer, pinned by figures the
//! synthesizer's own definitions give (a channel's volume gain is
//! `(cc7 * 128 / 16383)^2`, a pitch bend is `2 * (bend - 8192) / 8192`
//! semitones) and by invariants of the render, never by another render of the
//! code.
//!
//! Mutation record (each planted in `src/`, seen to fail at the named tests,
//! restored):
//! - not seen here: the `.max(1)` on the Note On velocity (`voices.rs`, and the
//!   region choice in `patch.rs`) removed. A voice struck at MIDI 1.0 velocity
//!   0 or 1 is far below what these renders resolve, so no test in this file
//!   tells them apart; the pedal tests pin Note On against Note Off instead;
//! - `widen`'s velocity-0 Note On guard removed (in `tutti-midi-protocol`) ->
//!   `a_midi_1_note_on_at_velocity_0_is_a_note_off`;
//! - `Player::apply` reading a MIDI 2.0 Note On at velocity 0 as a Note Off ->
//!   `a_midi_2_note_on_at_a_velocity_that_narrows_to_zero_is_a_note_on`;
//! - `narrow_value` (in `tutti-midi-protocol`) shifting by 24 ->
//!   `control_change_7_reaches_the_synth_at_its_value`;
//! - the bend's LSB and MSB swapped -> `a_bend_reaches_the_synth_at_its_value`;
//! - the bend's LSB argument replaced by 0 -> the same test (b = 127, 8319,
//!   16383 and the rest differ from the centre by under 3.1 cents of LSB);
//! - a Note On or a Note Off with a reserved attribute type dropped ->
//!   `a_reserved_attribute_type_is_ignored_not_a_reason_to_drop_the_note`;
//! - `Voice2::PolyPressure` made to release the note ->
//!   `messages_with_no_synthesizer_setter_change_nothing`.

use std::sync::Arc;

mod support;

use support::Hand;
use tutti_sf2::SoundFont;
use tutti_soundfont::{Settings, SoundFontUnit};
use tutti_types::SampleRate;
use tutti_ump::{
    Address, Attribute, CCNumber, Message, MidiChannel, MidiGroup, NoteNumber,
    RegisteredPerNoteController, U7, U14, Ump, Voice1, Voice2,
};

const RATE: SampleRate = SampleRate::SR_44K1;

fn load_test_soundfont() -> Arc<SoundFont> {
    let path = tutti_fixtures::asset("soundfonts/TimGM6mb.sf2");
    let mut file = std::fs::File::open(&path)
        .unwrap_or_else(|e| panic!("test soundfont missing at {}: {e}", path.display()));
    Arc::new(
        SoundFont::new(&mut file)
            .unwrap_or_else(|e| panic!("test soundfont at {} is malformed: {e}", path.display())),
    )
}

/// The unit's dry pair, which is linear in the channel gain.
fn unit() -> Hand {
    let settings = Settings::new(RATE);
    Hand::new(SoundFontUnit::new(load_test_soundfont(), &settings).unwrap())
}

/// A unit on GM program 16, a drawbar organ: it holds its level for as long as
/// the key is down, so a level that changes is a message's doing.
fn organ() -> Hand {
    let mut unit = unit();
    unit.program_change(MidiChannel::FIRST, U7::of(16));
    unit
}

fn event(message: Message) -> Ump {
    Ump::from(message)
}
fn address() -> Address {
    Address {
        group: MidiGroup::FIRST,
        channel: MidiChannel::FIRST,
    }
}
fn m1(voice: Voice1) -> Ump {
    event(Message::Midi1(address(), voice))
}
fn m2(voice: Voice2) -> Ump {
    event(Message::Midi2(address(), voice))
}
fn note_on_2(n: u8, velocity: u16) -> Ump {
    m2(Voice2::NoteOn {
        note: NoteNumber::of(n),
        velocity,
        attribute: Attribute::None,
    })
}
fn cc7_2(value: u32) -> Ump {
    m2(Voice2::ControlChange {
        controller: CCNumber::of(7),
        value,
    })
}

/// Plays `events` in the first block, then renders `frames` frames in all, as
/// the left channel.
fn render(unit: &mut Hand, events: &[Ump], frames: usize) -> Vec<f32> {
    unit.queue_midi(events);
    let mut out = Vec::with_capacity(frames);
    while out.len() < frames {
        let (left, _) = unit.block(support::BLOCK);
        out.extend_from_slice(left);
    }
    out
}

fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

/// RMS of the organ note after `events`, from 0.25 s to 0.5 s.
fn held_rms(events: &[Ump]) -> f32 {
    let out = render(&mut organ(), events, 22_050);
    rms(&out[11_025..])
}

/// A MIDI 1.0 Note On at velocity 0 is a Note Off (M2-104 App. D.3.1). The
/// evidence is the sustain pedal, as below: held down, it keeps a released
/// note sounding, where a Note On would strike the key again and end it; let
/// up, the note decays to silence.
#[test]
fn a_midi_1_note_on_at_velocity_0_is_a_note_off() {
    let end = m1(Voice1::NoteOn {
        note: NoteNumber::of(60),
        velocity: U7::of(0),
    });
    let pedal = |value| {
        m2(Voice2::ControlChange {
            controller: CCNumber::of(64),
            value,
        })
    };
    let mut unit = organ();
    let out = render(&mut unit, &[pedal(u32::MAX), note_on_2(60, 0xFFFF)], 22_050);
    let held = rms(&out[11_025..]);
    assert!(held > 0.001, "the note must sound at all");
    render(&mut unit, &[end], 22_050);
    let pedalled = rms(&render(&mut unit, &[], 11_025));
    assert!(
        pedalled > held * 0.5,
        "the pedal holds a Note Off: {pedalled} against {held}"
    );
    render(&mut unit, &[pedal(0)], 44_100);
    let after = rms(&render(&mut unit, &[], 1_000));
    assert!(after < held * 0.01, "{after} against {held}");
}

/// A 16-bit velocity below 0x0200, 0 included, narrows to 0, which MIDI 1.0
/// reads as a Note Off. A MIDI 2.0 Note On at any velocity is a Note On (M2-104
/// section 7.4.2) and must be played as one. The evidence is the sustain pedal (SF2 2.04 section 9.2, CC
/// 64): a Note Off on the held key would be stored and run when the pedal
/// lifts, so the note keeps sounding; a Note On strikes the key again, which
/// releases the earlier strike whatever holds it (the retrigger rule), and the
/// new note at that velocity is below what is rendered, so the note ends.
/// (The test used to hold the note without a pedal: a key struck again layered
/// then, and no longer does.)
///
/// Mutation (run): `Player::apply` reading a velocity below 0x200 as a Note Off leaves
/// the pedal-held note sounding, and the test fails.
#[test]
fn a_midi_2_note_on_at_a_velocity_that_narrows_to_zero_is_a_note_on() {
    for velocity in [0, 1, 0x01FF] {
        let mut unit = organ();
        let pedal = m2(Voice2::ControlChange {
            controller: CCNumber::of(64),
            value: u32::MAX,
        });
        let out = render(&mut unit, &[pedal, note_on_2(60, 0xFFFF)], 22_050);
        let held = rms(&out[11_025..]);
        assert!(held > 0.001, "the note must sound at all");
        render(&mut unit, &[note_on_2(60, velocity)], 22_050);
        let later = rms(&render(&mut unit, &[], 11_025));
        assert!(
            later < held * 0.01,
            "velocity {velocity:#x} was read as a Note Off, which the pedal holds: \
             {later} against {held}"
        );
    }
}

/// CC 7 at `v` scales the channel's gain to `(v * 128 / 16383)^2`, so the
/// held note's level at `v` against 127 is `(v / 127)^2`. It is `v` whether it
/// arrived as a 7-bit value or as any 32-bit value whose top 7 bits are `v`.
#[test]
fn control_change_7_reaches_the_synth_at_its_value() {
    let note = note_on_2(60, 0xFFFF);
    let full = held_rms(&[
        m1(Voice1::ControlChange {
            controller: CCNumber::of(7),
            value: U7::of(127),
        }),
        note,
    ]);
    assert!(full > 0.001);
    for v in [32u8, 64, 96, 120] {
        let want = (f32::from(v) / 127.0).powi(2);
        let wide = u32::from(v) << 25;
        let cases = [
            (
                "7-bit",
                m1(Voice1::ControlChange {
                    controller: CCNumber::of(7),
                    value: U7::of(v),
                }),
            ),
            ("32-bit, low bits clear", cc7_2(wide)),
            ("32-bit, low bits set", cc7_2(wide | 0x01FF_FFFF)),
        ];
        for (name, cc) in cases {
            let got = held_rms(&[cc, note]) / full;
            assert!((got - want).abs() < 1e-3, "{name} v={v}: {got} vs {want}");
        }
    }
    assert_eq!(held_rms(&[cc7_2(0), note]), 0.0);
}

/// The fundamental period, in frames, of `x` (at least 8000 frames longer than
/// the largest lag it tries). A coarse autocorrelation peak between `lo` and
/// `hi` finds the period to a frame; the peak at `K` periods then finds it
/// to `1/K` of that, refined by a parabola through the peak.
fn period(x: &[f32], lo: usize, hi: usize) -> f64 {
    const K: usize = 16;
    let n = x.len() - 8_000;
    let ac = |lag: usize| -> f64 {
        x[..n]
            .iter()
            .zip(&x[lag..])
            .map(|(a, b)| f64::from(*a) * f64::from(*b))
            .sum()
    };
    let peak = |range: std::ops::Range<usize>| {
        range
            .map(|lag| (lag, ac(lag)))
            .fold((0, f64::MIN), |m, c| if c.1 > m.1 { c } else { m })
            .0
    };
    let coarse = peak(lo..hi);
    let best = peak(K * coarse - 2 * K..K * coarse + 2 * K);
    let (a, b, c) = (ac(best - 1), ac(best), ac(best + 1));
    (best as f64 + 0.5 * (a - c) / (a - 2.0 * b + c)) / K as f64
}

/// A pitch bend of `b` (14 bits) sounds `2 * (b - 8192) / 8192` semitones from
/// the unbent note, as a MIDI 1.0 bend and as a native 32-bit bend whose top
/// 14 bits are `b`. The pitch is measured to about 0.001% and held to 0.005%
/// (0.09 cent), so the bend's low 7 bits (at most 3.1 cents) are seen: `b` includes values that
/// differ from a centre or an edge only there.
#[test]
fn a_bend_reaches_the_synth_at_its_value() {
    let sustained = |events: &[Ump]| {
        let out = render(&mut organ(), events, 40_000);
        out[8_000..].to_vec()
    };
    let key = m2(Voice2::NoteOn {
        note: NoteNumber::of(48),
        velocity: 0xC000,
        attribute: Attribute::None,
    });
    let still = period(&sustained(&[key]), 200, 500);
    for b in [
        0u16,
        127,
        4096,
        8192,
        8192 + 64,
        8192 + 127,
        12288,
        16256,
        16383,
    ] {
        let want = 2.0_f64.powf(2.0 * (f64::from(b) - 8192.0) / 8192.0 / 12.0);
        let cases = [
            ("7-bit", m1(Voice1::PitchBend(U14::of(b)))),
            (
                "32-bit",
                m2(Voice2::PitchBend(u32::from(b) << 18 | 0x3_FFFF)),
            ),
        ];
        for (name, bend) in cases {
            let got = still / period(&sustained(&[bend, key]), 200, 500);
            assert!(
                (got / want - 1.0).abs() < 5e-5,
                "{name} b={b}: {got} vs {want}"
            );
        }
    }
}

/// A full bend up spans the channel's sensitivity, RPN 0 (RP-003, M2-115
/// section 4.1: the semitones then the cents, `MSB << 7 | LSB` zero-extended):
/// +200 cents at the default and after an explicit 2, +700 at 7, and the
/// cents field is hundredths of a semitone (7 semitones 50 cents is +750,
/// 0 and 64 cents is +64, not the 50 a binary fraction would give). The bend is
/// the note logic's alone (`Voicing::step`).
///
/// Mutations (run, each seen to fail): the sensitivity read as Q7.25 (cents
/// 64 gives +50); the sensitivity ignored (every row is +200); the bend
/// applied twice (every row doubles).
#[test]
fn a_full_bend_spans_the_registered_sensitivity() {
    let sustained = |events: &[Ump]| {
        let out = render(&mut organ(), events, 40_000);
        out[8_000..].to_vec()
    };
    let key = m2(Voice2::NoteOn {
        note: NoteNumber::of(55),
        velocity: 0xC000,
        attribute: Attribute::None,
    });
    let full = m2(Voice2::PitchBend(u32::MAX));
    let still = period(&sustained(&[key]), 200, 500);
    let sensitivity = |semitones: u32, cents: u32| {
        m2(Voice2::Registered {
            bank: U7::of(0),
            index: U7::of(0),
            value: (semitones << 7 | cents) << 18,
        })
    };
    for (events, want_cents) in [
        (vec![full, key], 200.0),
        (vec![sensitivity(2, 0), full, key], 200.0),
        (vec![sensitivity(7, 0), full, key], 700.0),
        (vec![sensitivity(7, 50), full, key], 750.0),
        (vec![sensitivity(0, 64), full, key], 64.0),
    ] {
        let got = still / period(&sustained(&events), 200, 500);
        let want = 2.0_f64.powf(want_cents / 1200.0);
        assert!(
            (got / want - 1.0).abs() < 5e-5,
            "{want_cents} cents: {got} vs {want}"
        );
    }
}

/// Messages RustySynth has no setter for are decoded and dropped: the render
/// is the one without them.
#[test]
fn messages_with_no_synthesizer_setter_change_nothing() {
    let key = note_on_2(60, 0xC000);
    let n = NoteNumber::of(60);
    let dropped = [
        m1(Voice1::PolyPressure {
            note: n,
            pressure: U7::of(90),
        }),
        m1(Voice1::ChannelPressure(U7::of(90))),
        m2(Voice2::PolyPressure {
            note: n,
            value: u32::MAX,
        }),
        m2(Voice2::ChannelPressure(u32::MAX)),
        m2(Voice2::PerNotePitchBend {
            note: n,
            value: u32::MAX,
        }),
        m2(Voice2::RegisteredPerNote {
            note: n,
            controller: RegisteredPerNoteController::Volume,
            value: 0,
        }),
        m2(Voice2::AssignablePerNote {
            note: n,
            controller: 7,
            value: 0,
        }),
        m2(Voice2::PerNoteManagement {
            note: n,
            detach: true,
            reset: true,
        }),
        m2(Voice2::Registered {
            bank: U7::of(0),
            index: U7::of(0),
            value: u32::MAX,
        }),
        m2(Voice2::Assignable {
            bank: U7::of(0),
            index: U7::of(7),
            value: 0,
        }),
        m2(Voice2::RelativeRegistered {
            bank: U7::of(0),
            index: U7::of(0),
            delta: 1000,
        }),
        m2(Voice2::RelativeAssignable {
            bank: U7::of(0),
            index: U7::of(7),
            delta: -1000,
        }),
        event(Message::System(MidiGroup::FIRST, tutti_ump::System::Reset)),
    ];
    let reference = render(&mut unit(), &[key], 8_192);
    assert!(rms(&reference) > 0.001);
    for message in dropped {
        let got = render(&mut unit(), &[key, message], 8_192);
        assert_eq!(got, reference, "{message:?}");
    }
}

/// A Note On or Note Off packet with a chosen attribute type, group 0 and
/// channel 0: `decode` keeps an undefined type (M2-104 Table 8, 0x04 to 0xFF,
/// "Reserved") as `Attribute::Reserved`.
fn raw_note(on: bool, n: u8, attribute_type: u8, velocity: u16) -> Ump {
    let status: u32 = if on { 0x9 } else { 0x8 };
    Ump([
        0x4000_0000 | status << 20 | u32::from(n) << 8 | u32::from(attribute_type),
        u32::from(velocity) << 16 | 0x1234,
        0,
        0,
    ])
}

/// M2-104 7.4.14 Table 8 marks attribute types 0x04 to 0xFF reserved for
/// senders (2.1.3: do not use) and gives a receiver no rule for them. The
/// attribute is not read and the note plays: a Note On of any reserved type
/// sounds, and a Note Off of any releases the note.
#[test]
fn a_reserved_attribute_type_is_ignored_not_a_reason_to_drop_the_note() {
    for kind in [4u8, 9, 255] {
        let mut unit = organ();
        let held = rms(&render(&mut unit, &[raw_note(true, 60, kind, 0xFFFF)], 22_050)[11_025..]);
        assert!(held > 0.001, "type {kind}: the Note On was dropped");
        render(&mut unit, &[raw_note(false, 60, kind, 0)], 44_100);
        let after = rms(&render(&mut unit, &[], 1_000));
        assert!(
            after < held * 0.01,
            "type {kind}: the Note Off was dropped: {after} against {held}"
        );
    }
}

/// Every event of a block is read, however many it carries: a note after 600
/// controllers in one block sounds. The unit read a block through a fixed
/// scratch of 256 events before stage 3b and dropped the rest.
///
/// Mutation (run): `render_events` reading at most 256 events a block (a
/// `.take(256)` over the chunks' events) → the note is dropped → silence →
/// fails.
#[test]
fn a_block_with_more_events_than_any_scratch_reads_them_all() {
    let mut events = vec![
        m2(Voice2::ControlChange {
            controller: CCNumber::of(1),
            value: 0,
        });
        600
    ];
    events.push(note_on_2(60, 0xFFFF));
    let out = render(&mut organ(), &events, 22_050);
    assert!(rms(&out[11_025..]) > 0.001, "the last event sounds");
}
