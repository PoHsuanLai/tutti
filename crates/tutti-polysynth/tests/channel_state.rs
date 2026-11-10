//! Per-channel and per-key state, heard at the output: a retriggered key, the
//! pedal and channel-mode controllers, and pitch bend with its sensitivity.
//!
//! Figures are analytic (a sine's pitch ratio is 2^(semitones/12), silence is
//! silence once the release has run), never a comparison against another
//! render. MIDI 1.0 behaviours are cited by section name of the MIDI 1.0
//! Detailed Specification and of Recommended Practice RP-015.

mod support;

use support::{BLOCK, Hand};
use tutti_fixtures::midi;
use tutti_graph::Node;
use tutti_midi_types::mpe::PitchBendSensitivity;
use tutti_midi_types::translation::scaling::{midi1_cc_to_midi2, midi1_velocity_to_midi2};
use tutti_midi_types::ump::{RPN_BANK_MPE, RPN_INDEX_CHANNEL_PITCH_BEND_SENSITIVITY};
use tutti_polysynth::{
    Adsr, AllocationStrategy, OscillatorType, PolySynth, SynthConfig, VoiceMode,
};
use tutti_types::{Amplitude, SampleRate, Seconds, Semitones};
use tutti_ump::{CCNumber, MidiChannel, MidiGroup, Ump};

const SR: f64 = 48_000.0;
/// The envelope's release, seconds.
const RELEASE: f64 = 0.1;
/// Blocks in `seconds`.
fn blocks(seconds: f64) -> usize {
    (seconds * SR / BLOCK as f64).ceil() as usize
}

fn config() -> SynthConfig {
    SynthConfig {
        sample_rate: SampleRate::from(SR),
        oscillator: OscillatorType::Sine,
        envelope: Adsr {
            attack: Seconds(0.001),
            decay: Seconds(0.0),
            sustain: Amplitude(1.0),
            release: Seconds(RELEASE),
        },
        max_voices: 8,
        ..Default::default()
    }
}

fn hand(config: SynthConfig) -> Hand {
    Hand::new(PolySynth::new(config).expect("a valid config"))
}

fn on(channel: u8, note: u8) -> Ump {
    midi::note_on(
        MidiGroup::FIRST,
        MidiChannel::of(channel),
        note,
        midi1_velocity_to_midi2(100),
    )
}

fn off(channel: u8, note: u8) -> Ump {
    midi::note_off(MidiGroup::FIRST, MidiChannel::of(channel), note, 0)
}

fn cc(channel: u8, number: CCNumber, value: u8) -> Ump {
    midi::cc(
        MidiGroup::FIRST,
        MidiChannel::of(channel),
        number,
        midi1_cc_to_midi2(value),
    )
}

/// Full wheel up.
fn bend_up(channel: u8) -> Ump {
    midi::pitch_bend(MidiGroup::FIRST, MidiChannel::of(channel), u32::MAX)
}

/// Pitch Bend Sensitivity (RPN 0) of whole `semitones`.
fn sensitivity(channel: u8, semitones: u8) -> Ump {
    midi::registered_controller(
        MidiGroup::FIRST,
        MidiChannel::of(channel),
        RPN_BANK_MPE,
        RPN_INDEX_CHANNEL_PITCH_BEND_SENSITIVITY,
        PitchBendSensitivity::of(semitones).to_rpn_bits(),
    )
}

/// Plays `events` in the next block, then `seconds` more; the left channel.
fn play(hand: &mut Hand, events: &[Ump], seconds: f64) -> Vec<f32> {
    hand.queue_midi(events);
    let mut out = Vec::new();
    for _ in 0..blocks(seconds).max(1) {
        out.extend_from_slice(hand.block(BLOCK).0);
    }
    out
}

fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0, |a, &b| a.max(b.abs()))
}

/// A sine's frequency from the span of its rising zero crossings, which
/// resolves far finer than a percent over a few thousand cycles.
fn hz(x: &[f32]) -> f64 {
    let crossings: Vec<f64> = x
        .windows(2)
        .enumerate()
        .filter(|(_, w)| w[0] < 0.0 && w[1] >= 0.0)
        .map(|(i, w)| i as f64 + f64::from(-w[0] / (w[1] - w[0])))
        .collect();
    assert!(
        crossings.len() > 20,
        "not a tone: {} crossings",
        crossings.len()
    );
    let cycles = (crossings.len() - 1) as f64;
    SR * cycles / (crossings[crossings.len() - 1] - crossings[0])
}

const A4: f64 = 440.0;

fn ratio(semitones: f64) -> f64 {
    2f64.powf(semitones / 12.0)
}

/// Asserts `x` sounds at `expected` Hz within 0.05 %.
#[track_caller]
fn assert_hz(x: &[f32], expected: f64) {
    let got = hz(x);
    assert!(
        (got / expected - 1.0).abs() < 5e-4,
        "{got} Hz, expected {expected} Hz"
    );
}

/// Bug 1. A key struck again while it sounds replaces its voice (the
/// allocator releases the old slot), but nothing gated the old voice off, so
/// it sustained for ever: peak 0.55 a second after the note-off.
///
/// Mutation (run): `handle_note_on` without the `note_off` of the replaced
/// voice fails the final assertion (peak stays at the sustain level).
#[test]
fn a_retriggered_key_is_silent_after_its_release() {
    let mut h = hand(config());
    play(&mut h, &[on(0, 60)], 0.1);
    play(&mut h, &[on(0, 60)], 0.01);
    assert!(peak(&play(&mut h, &[], 0.05)) > 0.1, "the key sounds");
    play(&mut h, &[off(0, 60)], RELEASE + 0.05);
    assert_eq!(peak(&play(&mut h, &[], 0.1)), 0.0, "a voice hung");
    assert_eq!(h.active_voice_count(), 0);
}

/// Reset All Controllers (RP-015) puts the sustain pedal up, which lets go
/// of the notes it held. Bug 2: CC 121 left the pedal down.
///
/// Mutation (run): the CC 121 arm as it was, without the pedal resets, fails
/// the assertion that the pedal is up.
#[test]
fn reset_all_controllers_lifts_the_sustain_pedal() {
    let mut h = hand(config());
    play(&mut h, &[cc(0, CCNumber::SUSTAIN, 127), on(0, 60)], 0.05);
    play(&mut h, &[off(0, 60)], RELEASE + 0.1);
    assert!(peak(&play(&mut h, &[], 0.05)) > 0.1, "the pedal holds it");

    play(&mut h, &[cc(0, CCNumber::RESET_ALL, 0)], RELEASE + 0.05);
    assert_eq!(peak(&play(&mut h, &[], 0.05)), 0.0, "the pedal is up");

    // The pedal itself is up, not just the held note let go.
    play(&mut h, &[on(0, 62)], 0.05);
    play(&mut h, &[off(0, 62)], RELEASE + 0.05);
    assert_eq!(peak(&play(&mut h, &[], 0.05)), 0.0, "a later note is held");
}

/// Reset All Controllers acts on its own channel's pedal.
///
/// Mutation (run): resetting every channel's pedal fails this.
#[test]
fn reset_all_controllers_leaves_another_channels_pedal() {
    let mut h = hand(config());
    play(&mut h, &[cc(1, CCNumber::SUSTAIN, 127), on(1, 60)], 0.05);
    play(
        &mut h,
        &[off(1, 60), cc(0, CCNumber::RESET_ALL, 0)],
        RELEASE + 0.1,
    );
    assert!(peak(&play(&mut h, &[], 0.05)) > 0.1);
}

/// All Notes Off (CC 123, MIDI 1.0 Channel Mode Messages) lets go of the keys
/// and a pedal that is down still holds its notes. Bug 3: it released them.
///
/// Mutations (run): the old `all_notes_off`, which released and cleared the
/// pedal's hold, and one that ignores the pedal, each fail the first
/// assertion.
#[test]
fn all_notes_off_respects_the_sustain_pedal() {
    let mut h = hand(config());
    play(&mut h, &[cc(0, CCNumber::SUSTAIN, 127), on(0, 60)], 0.05);
    play(&mut h, &[cc(0, CCNumber::ALL_NOTES_OFF, 0)], RELEASE + 0.1);
    assert!(peak(&play(&mut h, &[], 0.05)) > 0.1, "the pedal holds it");

    play(&mut h, &[cc(0, CCNumber::SUSTAIN, 0)], RELEASE + 0.05);
    assert_eq!(peak(&play(&mut h, &[], 0.05)), 0.0, "the pedal lifts it");
}

/// With no pedal, All Notes Off releases a held key; another channel's
/// keys are untouched.
///
/// Mutation (run): matching every channel fails the second assertion.
#[test]
fn all_notes_off_releases_the_channels_keys_only() {
    let mut h = hand(config());
    play(&mut h, &[on(0, 60), on(1, 67)], 0.05);
    let out = play(&mut h, &[cc(0, CCNumber::ALL_NOTES_OFF, 0)], RELEASE + 0.1);
    assert!(peak(&out) > 0.1);
    // Channel 0 is gone; what sounds now is channel 1's G4 alone.
    assert_hz(&play(&mut h, &[], 0.5), A4 * ratio(-2.0));
    assert_eq!(h.active_voice_count(), 1);
}

/// All Sound Off (CC 120) cuts at once whatever the release or the pedal.
///
/// Mutation (run): ignoring CC 120 leaves the held note sounding.
#[test]
fn all_sound_off_is_immediate_and_beats_the_pedal() {
    let mut h = hand(config());
    play(&mut h, &[cc(0, CCNumber::SUSTAIN, 127), on(0, 60)], 0.05);
    play(&mut h, &[off(0, 60)], 0.01);
    let out = play(&mut h, &[cc(0, CCNumber::ALL_SOUND_OFF, 0)], 0.01);
    assert_eq!(peak(&out), 0.0);
    assert_eq!(h.active_voice_count(), 0);
}

/// Damper Pedal (CC 64, MIDI 1.0 Control Change Messages): 0 to 63 is off,
/// 64 to 127 is on.
///
/// Mutations (run): `value > 0.5` fails the 64 case; `value >= 0.49` fails
/// the 63 case.
#[test]
fn sustain_is_down_from_64() {
    for (value, held) in [(63, false), (64, true)] {
        let mut h = hand(config());
        play(&mut h, &[cc(0, CCNumber::SUSTAIN, value), on(0, 60)], 0.05);
        play(&mut h, &[off(0, 60)], RELEASE + 0.1);
        let sounding = peak(&play(&mut h, &[], 0.05)) > 0.1;
        assert_eq!(sounding, held, "CC 64 at {value}");
    }
}

/// Pitch Bend Sensitivity (RPN 0, MIDI 1.0 Registered Parameter Numbers)
/// sets the range a full bend spans. Bug 4: it was ignored, and the range
/// came from the config alone.
///
/// Mutations (run): ignoring RPN 0 fails the 0-semitone case (the config's 2
/// sounds); applying it to every channel fails the last assertion.
#[test]
fn rpn_0_sets_the_bend_range_per_channel() {
    for semitones in [0u8, 2, 7, 12] {
        let mut h = hand(config());
        play(
            &mut h,
            &[sensitivity(0, semitones), on(0, 69), bend_up(0)],
            0.2,
        );
        let tone = play(&mut h, &[], 0.5);
        // Full wheel is 1 - 2^-31 of the travel.
        assert_hz(&tone, A4 * ratio(f64::from(semitones)));
    }

    // The range belongs to its channel: channel 1's does not move channel 0's.
    let mut h = hand(config());
    play(&mut h, &[sensitivity(1, 12), on(0, 69), bend_up(0)], 0.2);
    assert_hz(&play(&mut h, &[], 0.5), A4 * ratio(2.0));
}

/// Without an RPN the config's range is the default.
#[test]
fn the_configured_range_is_the_default() {
    let mut h = hand(SynthConfig {
        pitch_bend_range: Semitones(5.0),
        ..config()
    });
    play(&mut h, &[on(0, 69), bend_up(0)], 0.2);
    assert_hz(&play(&mut h, &[], 0.5), A4 * ratio(5.0));
}

/// Bug 5: a bend moved every note whatever its channel, while pedals were
/// already per channel. A bend on channel 1 leaves channel 0's note alone, and
/// channel 0's own bend moves it.
///
/// Mutation (run): a bend that reaches every channel fails the first
/// assertion.
#[test]
fn a_bend_moves_its_own_channel() {
    let mut h = hand(config());
    play(&mut h, &[on(0, 69), bend_up(1)], 0.2);
    assert_hz(&play(&mut h, &[], 0.5), A4);

    play(&mut h, &[bend_up(0)], 0.2);
    assert_hz(&play(&mut h, &[], 0.5), A4 * ratio(2.0));
}

/// A note struck on a bent channel starts bent, and Reset All Controllers
/// centres its own channel's wheel only.
///
/// Mutation (run): centring every channel fails the second assertion.
#[test]
fn reset_all_controllers_centres_its_own_wheel() {
    let mut h = hand(config());
    play(&mut h, &[bend_up(0), bend_up(1), on(0, 69)], 0.2);
    assert_hz(&play(&mut h, &[], 0.5), A4 * ratio(2.0));

    play(&mut h, &[on(1, 81)], 0.2);
    play(&mut h, &[off(0, 69)], RELEASE + 0.05);
    // Channel 1's A5 is still bent.
    assert_hz(
        &play(&mut h, &[cc(0, CCNumber::RESET_ALL, 0)], 0.5),
        2.0 * A4 * ratio(2.0),
    );

    play(&mut h, &[cc(1, CCNumber::RESET_ALL, 0)], 0.2);
    assert_hz(&play(&mut h, &[], 0.5), 2.0 * A4);
}

/// Under MPE the master channel's bend is the zone's: it reaches a note on a
/// member channel, and the note's own per-note bend multiplies it, so the two
/// add in semitones. Mutation (run): a master bend that reaches only its own
/// channel leaves the note at the per-note bend alone (12 semitones, not 14).
#[test]
fn under_mpe_the_master_bend_and_the_per_note_bend_add() {
    let mut h = hand(SynthConfig {
        mpe_enabled: true,
        mpe_pitch_bend_range: Semitones(48.0),
        ..config()
    });
    // Per-note bend of a quarter of the wheel's travel: 12 of 48 semitones.
    let per_note = midi::per_note_pitch_bend(MidiGroup::FIRST, MidiChannel::of(1), 69, 0xA000_0000);
    play(&mut h, &[on(1, 69), bend_up(0), per_note], 0.2);
    assert_hz(&play(&mut h, &[], 0.5), A4 * ratio(14.0));
}

/// Under MPE only the master channel's Reset All Controllers reaches the
/// zone; a member's touches that member (RP-015 per channel).
///
/// Mutation (run): `zone_wide` true for every channel under MPE makes the
/// member's reset centre channel 1 too, and the first assertion fails.
#[test]
fn under_mpe_a_members_reset_touches_only_the_member() {
    let mut h = hand(SynthConfig {
        mpe_enabled: true,
        ..config()
    });
    play(&mut h, &[bend_up(0), on(1, 69)], 0.2);
    assert_hz(&play(&mut h, &[], 0.5), A4 * ratio(2.0));

    // Another member's reset leaves channel 1 bent.
    play(&mut h, &[cc(2, CCNumber::RESET_ALL, 0)], 0.2);
    assert_hz(&play(&mut h, &[], 0.5), A4 * ratio(2.0));

    // The master's centres the zone.
    play(&mut h, &[cc(0, CCNumber::RESET_ALL, 0)], 0.2);
    assert_hz(&play(&mut h, &[], 0.5), A4);
}

/// `Node::reset` is a freshly prepared synth's state: a raised wheel and a
/// bend range set by RPN 0 do not survive it.
///
/// Mutation (run): without `self.bends.reset()` in `reset_voices` the first
/// assertion fails (the wheel is still up); resetting only the positions
/// fails the second (the range is still 7).
#[test]
fn reset_returns_the_wheel_and_the_range_to_their_defaults() {
    let mut h = hand(config());
    play(&mut h, &[sensitivity(0, 7), bend_up(0)], 0.01);
    Node::reset(&mut *h);
    play(&mut h, &[on(0, 69)], 0.2);
    assert_hz(&play(&mut h, &[], 0.5), A4);

    play(&mut h, &[bend_up(0)], 0.2);
    assert_hz(&play(&mut h, &[], 0.5), A4 * ratio(2.0));
}

/// `Node::reset` also returns the per-note sensitivity (RPN 7) to the
/// configured one.
///
/// Mutation (run): without the `set_mpe_pitch_bend_range` in `reset_voices` a
/// quarter-travel bend sounds 3 semitones (range 12), not 12 (range 48).
#[test]
fn reset_returns_the_per_note_range_to_its_default() {
    let mut h = hand(SynthConfig {
        mpe_enabled: true,
        mpe_pitch_bend_range: Semitones(48.0),
        ..config()
    });
    let rpn7 = midi::registered_controller(
        MidiGroup::FIRST,
        MidiChannel::of(1),
        RPN_BANK_MPE,
        tutti_midi_types::ump::RPN_INDEX_PER_NOTE_PITCH_BEND_SENSITIVITY,
        PitchBendSensitivity::of(12).to_rpn_bits(),
    );
    play(&mut h, &[rpn7], 0.01);
    Node::reset(&mut *h);
    // A quarter of the travel: 12 of 48 semitones.
    let per_note = midi::per_note_pitch_bend(MidiGroup::FIRST, MidiChannel::of(1), 69, 0xA000_0000);
    play(&mut h, &[on(1, 69), per_note], 0.2);
    assert_hz(&play(&mut h, &[], 0.5), A4 * ratio(12.0));
}

fn config_with(mode: VoiceMode, strategy: AllocationStrategy, voices: usize) -> SynthConfig {
    SynthConfig {
        voice_mode: mode,
        allocation_strategy: strategy,
        max_voices: voices,
        ..config()
    }
}

/// A key struck again under the sustain pedal: the old note releases at once
/// (the pedal does not keep it) and the new note takes the pedal's hold. The
/// MIDI 1.0 spec is silent; this is the synth's choice.
///
/// Mutation (run): not gating the replaced voice off leaves two voices
/// (count 2) after the first wait.
#[test]
fn a_retrigger_under_the_pedal_releases_the_old_note_and_the_new_one_is_held() {
    let mut h = hand(config());
    play(
        &mut h,
        &[cc(0, CCNumber::SUSTAIN, 127), on(0, 60), on(0, 60)],
        RELEASE + 0.1,
    );
    assert_eq!(h.active_voice_count(), 1, "the old note released at once");

    play(&mut h, &[off(0, 60)], RELEASE + 0.1);
    assert!(
        peak(&play(&mut h, &[], 0.05)) > 0.1,
        "the pedal holds the new"
    );
    play(&mut h, &[cc(0, CCNumber::SUSTAIN, 0)], RELEASE + 0.1);
    assert_eq!(h.active_voice_count(), 0);
}

/// A key the sostenuto pedal captured, struck again: the captured note is
/// cut (released), and the new note, struck after the pedal went down, is not
/// captured, so its note-off releases it.
///
/// Mutation (run): not gating the replaced voice off leaves two voices.
#[test]
fn a_retrigger_of_a_sostenuto_key_cuts_the_captured_note() {
    let mut h = hand(config());
    play(&mut h, &[on(0, 60), cc(0, CCNumber::SOSTENUTO, 127)], 0.05);
    play(&mut h, &[on(0, 60)], RELEASE + 0.1);
    assert_eq!(h.active_voice_count(), 1, "the captured note is cut");
    play(&mut h, &[off(0, 60)], RELEASE + 0.1);
    assert_eq!(h.active_voice_count(), 0, "the new note was not captured");
}

/// Mono and legato reuse the one voice for a retriggered key: its note-off
/// ends the note.
///
/// Mutation (run): gating the replaced voice off even when the new note
/// reuses its slot leaves the retriggered mono note silent (the first
/// assertion fails).
#[test]
fn a_mono_or_legato_retrigger_sounds_and_ends_with_its_note_off() {
    for mode in [VoiceMode::Mono, VoiceMode::Legato] {
        let mut h = hand(config_with(mode, AllocationStrategy::Oldest, 4));
        play(&mut h, &[on(0, 60)], 0.1);
        play(&mut h, &[on(0, 60)], 0.1);
        assert!(peak(&play(&mut h, &[], 0.05)) > 0.1, "{mode:?} sounds");
        play(&mut h, &[off(0, 60)], RELEASE + 0.1);
        assert_eq!(peak(&play(&mut h, &[], 0.05)), 0.0, "{mode:?} ends");
        assert_eq!(h.active_voice_count(), 0);
    }
}

/// With every voice in use and `NoSteal`, a key struck again is released and
/// finds no slot, so it goes silent while the other note sounds on.
///
/// Mutation (run): not gating the replaced voice off keeps the key sounding
/// (two voices).
#[test]
fn a_retrigger_in_a_full_nosteal_pool_silences_the_key() {
    let mut h = hand(config_with(VoiceMode::Poly, AllocationStrategy::NoSteal, 2));
    play(&mut h, &[on(0, 60), on(0, 67)], 0.1);
    play(&mut h, &[on(0, 60)], RELEASE + 0.1);
    assert_eq!(h.active_voice_count(), 1);
    assert_hz(&play(&mut h, &[], 0.5), A4 * ratio(-2.0));
}
