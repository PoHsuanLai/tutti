//! What a decoded message does to the synthesizer, pinned by figures the
//! synthesizer's own definitions give (a channel's volume gain is
//! `(cc7 * 128 / 16383)^2`, a pitch bend is `2 * (bend - 8192) / 8192`
//! semitones) and by invariants of the render, never by another render of the
//! code.
//!
//! Mutation record (each planted in `src/`, seen to fail at the named tests,
//! restored):
//! - the `.max(1)` on the Note On velocity removed ->
//!   `a_nonzero_velocity_that_narrows_to_zero_still_sounds`;
//! - the `velocity: 0` Note On arm removed ->
//!   `a_note_on_at_velocity_0_leaves_no_voice_sounding`;
//! - `narrow_value` (in `tutti-midi-protocol`) shifting by 24 ->
//!   `control_change_7_reaches_the_synth_at_its_value`;
//! - the bend's LSB and MSB swapped -> `a_bend_reaches_the_synth_at_its_value`;
//! - `Voice2::PolyPressure` made to release the note ->
//!   `messages_with_no_synthesizer_setter_change_nothing`.

use std::sync::Arc;

mod support;

use support::Hand;
use tutti_midi_types::ump::MidiEvent;
use tutti_soundfont::{Settings, SoundFont, SoundFontUnit};
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

/// Without the effect lines, which are not linear in the channel gain.
fn unit() -> Hand {
    let mut settings = Settings::new(RATE);
    settings.reverb_and_chorus = false;
    Hand::new(SoundFontUnit::new(load_test_soundfont(), &settings).unwrap())
}

/// A unit on GM program 16, a drawbar organ: it holds its level for as long as
/// the key is down, so a level that changes is a message's doing.
fn organ() -> Hand {
    let mut unit = unit();
    unit.program_change(MidiChannel::FIRST, u7(16));
    unit
}

fn note(n: u8) -> NoteNumber {
    NoteNumber::new(n).unwrap()
}
fn u7(v: u8) -> U7 {
    U7::new(v).unwrap()
}

fn event(message: Message) -> MidiEvent {
    MidiEvent {
        frame_offset: 0,
        data: Ump::from(message).0,
    }
}
fn address() -> Address {
    Address {
        group: MidiGroup::FIRST,
        channel: MidiChannel::FIRST,
    }
}
fn m1(voice: Voice1) -> MidiEvent {
    event(Message::Midi1(address(), voice))
}
fn m2(voice: Voice2) -> MidiEvent {
    event(Message::Midi2(address(), voice))
}
fn note_on_2(n: u8, velocity: u16) -> MidiEvent {
    m2(Voice2::NoteOn {
        note: note(n),
        velocity,
        attribute: Attribute::None,
    })
}
fn cc7_2(value: u32) -> MidiEvent {
    m2(Voice2::ControlChange {
        controller: CCNumber::of(7),
        value,
    })
}

/// Plays `events` in the first block, then renders `frames` frames in all, as
/// the left channel.
fn render(unit: &mut Hand, events: &[MidiEvent], frames: usize) -> Vec<f32> {
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
fn held_rms(events: &[MidiEvent]) -> f32 {
    let out = render(&mut organ(), events, 22_050);
    rms(&out[11_025..])
}

/// A velocity-0 Note On is a Note Off, whether it arrived as a MIDI 1.0
/// message or a native one: the note it ends decays to silence.
#[test]
fn a_note_on_at_velocity_0_leaves_no_voice_sounding() {
    let end_1 = m1(Voice1::NoteOn {
        note: note(60),
        velocity: u7(0),
    });
    let end_2 = note_on_2(60, 0);
    for (name, end) in [("MIDI 1.0", end_1), ("MIDI 2.0", end_2)] {
        let mut unit = unit();
        let playing = rms(&render(&mut unit, &[note_on_2(60, 0xC000)], 22_050)[11_025..]);
        assert!(playing > 0.001, "{name}: the note must sound at all");
        render(&mut unit, &[end], 44_100);
        let after = rms(&render(&mut unit, &[], 1_000));
        assert!(after < playing * 0.01, "{name}: {after} against {playing}");
    }
}

/// A nonzero 16-bit velocity below 0x0200 narrows to 0, which RustySynth reads
/// as a Note Off. It must be a Note On: here it lands on a held key, and the
/// held note keeps sounding. (At 1 the new note itself is below what
/// RustySynth renders, so the held note is the evidence.)
#[test]
fn a_nonzero_velocity_that_narrows_to_zero_still_sounds() {
    for velocity in [1, 0x01FF] {
        let mut unit = organ();
        let out = render(&mut unit, &[note_on_2(60, 0xFFFF)], 22_050);
        let held = rms(&out[11_025..]);
        assert!(held > 0.001, "the note must sound at all");
        render(&mut unit, &[note_on_2(60, velocity)], 22_050);
        let later = rms(&render(&mut unit, &[], 11_025));
        assert!(
            later > held * 0.5,
            "velocity {velocity:#x} released the held note: {later} against {held}"
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
            value: u7(127),
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
                    value: u7(v),
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

/// The fundamental period, in frames, from the autocorrelation peak of `x`
/// between `lo` and `hi`, refined by a parabola through the peak.
fn period(x: &[f32], lo: usize, hi: usize) -> f32 {
    let ac = |lag: usize| -> f32 { x.iter().zip(&x[lag..]).map(|(a, b)| a * b).sum() };
    let (best, _) = (lo..hi)
        .map(|lag| (lag, ac(lag)))
        .fold((lo, f32::MIN), |m, c| if c.1 > m.1 { c } else { m });
    let (a, b, c) = (ac(best - 1), ac(best), ac(best + 1));
    best as f32 + 0.5 * (a - c) / (a - 2.0 * b + c)
}

/// A pitch bend of `b` (14 bits) sounds `2 * (b - 8192) / 8192` semitones from
/// the unbent note, at 0, the centre and the maximum, as a MIDI 1.0 bend and
/// as a native 32-bit bend whose top 14 bits are `b`.
#[test]
fn a_bend_reaches_the_synth_at_its_value() {
    let sustained = |events: &[MidiEvent]| {
        let out = render(&mut unit(), events, 20_000);
        out[8_000..].to_vec()
    };
    let key = m2(Voice2::NoteOn {
        note: note(48),
        velocity: 0xC000,
        attribute: Attribute::None,
    });
    let still = period(&sustained(&[key]), 200, 500);
    for b in [0u16, 4096, 8192, 12288, 16383] {
        let want = 2.0_f32.powf(2.0 * (f32::from(b) - 8192.0) / 8192.0 / 12.0);
        let cases = [
            ("7-bit", m1(Voice1::PitchBend(U14::new(b).unwrap()))),
            (
                "32-bit",
                m2(Voice2::PitchBend(u32::from(b) << 18 | 0x3_FFFF)),
            ),
        ];
        for (name, bend) in cases {
            let got = still / period(&sustained(&[bend, key]), 200, 500);
            assert!(
                (got / want - 1.0).abs() < 0.004,
                "{name} b={b}: {got} vs {want}"
            );
        }
    }
}

/// Messages RustySynth has no setter for are decoded and dropped: the render
/// is the one without them.
#[test]
fn messages_with_no_synthesizer_setter_change_nothing() {
    let key = note_on_2(60, 0xC000);
    let n = note(60);
    let dropped = [
        m1(Voice1::PolyPressure {
            note: n,
            pressure: u7(90),
        }),
        m1(Voice1::ChannelPressure(u7(90))),
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
            bank: u7(0),
            index: u7(0),
            value: u32::MAX,
        }),
        m2(Voice2::Assignable {
            bank: u7(0),
            index: u7(7),
            value: 0,
        }),
        m2(Voice2::RelativeRegistered {
            bank: u7(0),
            index: u7(0),
            delta: 1000,
        }),
        m2(Voice2::RelativeAssignable {
            bank: u7(0),
            index: u7(7),
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
