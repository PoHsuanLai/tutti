//! The typed `Synthesizer` surface over the reference SoundFont.

use std::fs::File;
use std::sync::Arc;

use tutti_fixtures::asset;
use tutti_rustysynth::{Stop, Synthesizer, SynthesizerError, SynthesizerSettings};
use tutti_sf2::SoundFont;
use tutti_types::{SampleRate, Samples};
use tutti_ump::{MidiChannel, NoteNumber, U7};

const RATE: SampleRate = SampleRate::SR_44K1;

fn timgm6mb() -> Arc<SoundFont> {
    let mut file = File::open(asset("soundfonts/TimGM6mb.sf2")).unwrap();
    Arc::new(SoundFont::new(&mut file).unwrap())
}

/// A synthesizer with no effect lines, so a stopped voice leaves true silence.
fn dry() -> Synthesizer {
    let mut settings = SynthesizerSettings::new(RATE);
    settings.enable_reverb_and_chorus = false;
    Synthesizer::new(&timgm6mb(), &settings).unwrap()
}

fn key(n: u8) -> NoteNumber {
    NoteNumber::new(n).unwrap()
}

fn channel(n: u8) -> MidiChannel {
    MidiChannel::new(n).unwrap()
}

fn velocity(n: u8) -> U7 {
    U7::new(n).unwrap()
}

/// The summed magnitude of the next `frames` frames.
fn energy(synthesizer: &mut Synthesizer, frames: usize) -> f32 {
    let mut left = vec![0.0; frames];
    let mut right = vec![0.0; frames];
    synthesizer.render(&mut left, &mut right);
    left.iter().chain(&right).map(|s| s.abs()).sum()
}

fn refused(settings: SynthesizerSettings) -> SynthesizerError {
    Synthesizer::new(&timgm6mb(), &settings).unwrap_err()
}

// Mutation (run): widening the lower bound in `validate` to 15_000 lets
// 15_999 Hz through and fails the first assertion.
#[test]
fn a_rate_outside_16_to_192_khz_is_refused_with_the_rate() {
    for hz in [15_999_u32, 192_001] {
        let rate = SampleRate::from(hz);
        let error = refused(SynthesizerSettings::new(rate));
        assert!(matches!(error, SynthesizerError::SampleRateOutOfRange(r) if r == rate));
    }
    for hz in [16_000_u32, 192_000] {
        let settings = SynthesizerSettings::new(SampleRate::from(hz));
        assert!(Synthesizer::new(&timgm6mb(), &settings).is_ok());
    }
}

#[test]
fn a_block_or_polyphony_outside_its_range_is_refused_with_the_value() {
    for frames in [7, 1025] {
        let mut settings = SynthesizerSettings::new(RATE);
        settings.block_size = Samples(frames);
        assert!(
            matches!(refused(settings), SynthesizerError::BlockSizeOutOfRange(b) if b == Samples(frames))
        );
    }
    for voices in [7, 257] {
        let mut settings = SynthesizerSettings::new(RATE);
        settings.maximum_polyphony = voices;
        assert!(
            matches!(refused(settings), SynthesizerError::MaximumPolyphonyOutOfRange(v) if v == voices)
        );
    }
}

#[test]
fn the_error_names_the_value_it_refused() {
    let message = refused(SynthesizerSettings::new(SampleRate::from(8_000))).to_string();
    assert!(message.contains("8000"), "{message}");
}

#[test]
fn the_rate_is_whole_hertz() {
    let synthesizer =
        Synthesizer::new(&timgm6mb(), &SynthesizerSettings::new(SampleRate(44_100.4)));
    assert_eq!(synthesizer.unwrap().sample_rate(), RATE);
}

// Mutation (run): swapping the `Cut` and `Release` arms of `note_off_all`
// leaves the cut note ringing and fails the first assertion.
#[test]
fn cut_silences_at_once_and_release_rings_out() {
    let mut cut = dry();
    cut.note_on(channel(0), key(60), velocity(100));
    assert!(energy(&mut cut, 512) > 1.0, "the note sounds");
    cut.note_off_all(Stop::Cut);
    assert_eq!(energy(&mut cut, 512), 0.0);

    let mut release = dry();
    release.note_on(channel(0), key(60), velocity(100));
    energy(&mut release, 512);
    release.note_off_all(Stop::Release);
    assert!(energy(&mut release, 512) > 0.0, "the release tail sounds");
}

// Mutation (run): comparing the voice's channel against the key number in
// `note_off_all_channel` cuts the wrong voices and fails one of the two
// assertions.
#[test]
fn a_channel_cut_leaves_the_other_channels_sounding() {
    let mut synthesizer = dry();
    synthesizer.note_on(channel(0), key(60), velocity(100));
    synthesizer.note_on(channel(1), key(64), velocity(100));
    energy(&mut synthesizer, 512);
    synthesizer.note_off_all_channel(channel(0), Stop::Cut);
    assert!(
        energy(&mut synthesizer, 512) > 1.0,
        "channel 1 still sounds"
    );
    synthesizer.note_off_all_channel(channel(1), Stop::Cut);
    assert_eq!(energy(&mut synthesizer, 512), 0.0);
}

// Mutation (run): deleting the velocity-0 early return in `note_on` starts a
// second voice instead of releasing the first, which sustains for the whole
// render and fails the last assertion.
#[test]
fn a_note_on_at_velocity_zero_is_a_note_off() {
    let mut synthesizer = dry();
    // The violin loops its sustain: only a release ends it.
    synthesizer.program_change(channel(0), velocity(40));
    synthesizer.note_on(channel(0), key(69), velocity(100));
    energy(&mut synthesizer, 44_100 * 3);
    assert!(energy(&mut synthesizer, 512) > 1.0, "the note sustains");

    synthesizer.note_on(channel(0), key(69), velocity(0));
    // Released, not cut: the tail is still there in the next block.
    assert!(energy(&mut synthesizer, 64) > 0.0);
    energy(&mut synthesizer, 44_100 * 20);
    assert_eq!(
        energy(&mut synthesizer, 512),
        0.0,
        "the released voice has ended"
    );
}
