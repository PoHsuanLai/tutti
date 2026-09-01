//! Audio behaviour of [`SoundFontUnit`] against the committed `TimGM6mb.sf2`.
//!
//! These moved here from `bevy-tutti`'s `src/soundfont.rs`, where they had been
//! testing the engine unit from inside the Bevy adapter — none of them names a
//! `World`, an `App` or an asset handle. The adapter's own tests are about
//! asset loading and promotion; this file is about whether the unit sounds.
//!
//! The fixture is committed at `crates/tutti/assets/soundfonts/TimGM6mb.sf2`, so
//! a missing one is a broken checkout and fails loudly. The bevy-tutti copies
//! `return`ed silently instead, which meant a green run proved nothing.

use std::path::PathBuf;
use std::sync::Arc;

use tutti_core::dsp::AudioUnit;
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiChannel, MidiGroup};
use tutti_soundfont::{SoundFont, SoundFontUnit, SynthesizerSettings};

/// Path to the committed test SoundFont.
///
/// `CARGO_MANIFEST_DIR` is `crates/tutti/crates/dsp/tutti-soundfont`; the asset
/// lives three levels up under `assets/`.
fn test_soundfont_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent() // dsp/
        .unwrap()
        .parent() // crates/
        .unwrap()
        .parent() // tutti/
        .unwrap()
        .join("assets/soundfonts/TimGM6mb.sf2")
}

/// Load the test SoundFont, failing loudly if the checkout lacks it.
fn load_test_soundfont() -> Arc<SoundFont> {
    let path = test_soundfont_path();
    let mut file = std::fs::File::open(&path)
        .unwrap_or_else(|e| panic!("test soundfont missing at {}: {e}", path.display()));
    Arc::new(
        SoundFont::new(&mut file)
            .unwrap_or_else(|e| panic!("test soundfont at {} is malformed: {e}", path.display())),
    )
}

/// Calculate RMS of stereo samples
fn rms(samples: &[(f32, f32)]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = samples.iter().map(|(l, r)| l * l + r * r).sum();
    (sum_sq / (samples.len() * 2) as f32).sqrt()
}

/// Render N samples from a SoundFontUnit
fn render_samples(unit: &mut SoundFontUnit, count: usize) -> Vec<(f32, f32)> {
    let mut samples = Vec::with_capacity(count);
    for _ in 0..count {
        let mut output = [0.0f32; 2];
        unit.tick(&[], &mut output);
        samples.push((output[0], output[1]));
    }
    samples
}

/// Render `blocks` consecutive `process` calls of `size` frames, queueing
/// `events` into the unit's MIDI inbox before the first — the RT path the
/// engine drives (poll + apply each event at its `frame_offset`), not the
/// per-sample `tick` path. `BufferVec` holds exactly one SIMD block per
/// channel, so `size` is capped at [`MAX_BUFFER_SIZE`].
fn render_process_blocks(
    unit: &mut SoundFontUnit,
    size: usize,
    blocks: usize,
    events: &[MidiEvent],
) -> Vec<(f32, f32)> {
    assert!(
        size <= tutti_core::MAX_BUFFER_SIZE,
        "one BufferVec block only"
    );
    unit.midi_sender().queue(events);

    let mut out = Vec::with_capacity(size * blocks);
    for _ in 0..blocks {
        let mut buffer = tutti_core::BufferVec::new(2);
        let input = tutti_core::BufferRef::new(&[]);
        unit.process(size, &input, &mut buffer.buffer_mut());
        out.extend((0..size).map(|i| (buffer.at_f32(0, i), buffer.at_f32(1, i))));
    }
    out
}

/// A note-on's `frame_offset` delays when it sounds — but only to the
/// resolution rustysynth's internal render chunk allows, which is **64
/// samples**, not one.
///
/// # The resolution this pins, and why it is not sample-accurate
///
/// `process` does interleave event application with rendering correctly: it
/// applies each event at its own `pos`. But `next_output_sample` pulls from a
/// 64-sample buffer that `refill_buffers` fills in one `Synthesizer::render`
/// call, so a note applied at `pos` inside an already-rendered chunk cannot
/// affect that chunk. Its first audible sample is the start of the *next* one.
///
/// Measured on this fixture, offsets 16, 32 and 48 within a 64-frame block all
/// produce byte-identical output, delayed exactly one chunk against offset 0.
/// So the honest property is a **two-valued** one — offset 0 sounds in the
/// first chunk, any non-zero offset within the block does not — and asserting
/// finer would be asserting something the unit does not do.
///
/// The rest of the block is deliberately measured over several blocks: a piano
/// attack's first 64 samples is 1.5 ms and lands around RMS 1.6e-4, far too
/// close to the noise floor to carry a threshold. That near-zero level is what
/// let the previous version of this test pass while proving nothing.
///
/// Sub-chunk accuracy would need `refill_buffers` to render in shorter
/// segments split at each pending event's offset. That is a real change to
/// `SoundFontUnit`, not a test fix, so the limitation is pinned here rather
/// than papered over.
#[test]
fn process_honors_frame_offset_to_chunk_resolution() {
    let sf = load_test_soundfont();
    let settings = SynthesizerSettings::new(44100);
    const BLOCK: usize = 64;
    const BLOCKS: usize = 6;
    const OFFSET: u32 = 48;

    let note = |offset: u32| {
        MidiEvent::note_on_7bit(MidiGroup::FIRST, MidiChannel::FIRST, 60, 100)
            .with_frame_offset(offset)
    };

    // Note at offset 0 — sounds from the first chunk.
    let mut early = SoundFontUnit::new(Arc::clone(&sf), &settings).expect("create SoundFontUnit");
    let s_early = render_process_blocks(&mut early, BLOCK, BLOCKS, &[note(0)]);

    // Same note delayed to OFFSET — the first chunk must be untouched.
    let mut late = SoundFontUnit::new(sf, &settings).expect("create SoundFontUnit");
    let s_late = render_process_blocks(&mut late, BLOCK, BLOCKS, &[note(OFFSET)]);

    // The offset-0 note is audible in the first block; the delayed one is not
    // merely quieter there but *exactly* silent, because its chunk was rendered
    // before the event was applied.
    assert!(
        rms(&s_early[..BLOCK]) > 0.0,
        "an offset-0 note must sound in the first chunk"
    );
    assert_eq!(
        rms(&s_late[..BLOCK]),
        0.0,
        "a note offset into the block cannot affect the chunk already rendered"
    );

    // Both eventually sound, and the delayed one lags by exactly one chunk —
    // this is the equality that shows the offset is honored at chunk resolution
    // rather than dropped entirely.
    assert!(
        rms(&s_early[BLOCK..]) > 0.001,
        "the offset-0 note must go on sounding past the first chunk"
    );
    assert_eq!(
        s_late[BLOCK..],
        s_early[..BLOCK * (BLOCKS - 1)],
        "the delayed note must be the offset-0 render shifted by one chunk"
    );
}

/// The MIDI-1 boundary (`dispatch`) must scale 16-bit UMP velocity through
/// the spec Min-Center-Max downscaler, not an open-coded multiply. Assert
/// the representative spec vectors so a regression to `* 127 / 65535` (which
/// maps center `0x8000` to 63, not 64) is caught.
#[test]
fn midi1_boundary_uses_spec_downscalers() {
    use tutti_midi_types::convert::{midi2_cc_to_midi1, midi2_velocity_to_midi1};
    assert_eq!(midi2_velocity_to_midi1(0xFFFF), 127);
    assert_eq!(midi2_velocity_to_midi1(0x8000), 64); // center → center
    assert_eq!(midi2_velocity_to_midi1(0x0000), 0);
    assert_eq!(midi2_cc_to_midi1(0xFFFF_FFFF), 127);
    assert_eq!(midi2_cc_to_midi1(0x8000_0000), 64); // center → center
}

#[test]
fn test_note_on_produces_audio() {
    let sf = load_test_soundfont();

    let settings = SynthesizerSettings::new(44100);
    let mut unit = SoundFontUnit::new(sf, &settings).expect("Failed to create SoundFontUnit");

    // Play middle C
    unit.note_on(0, 60, 100);

    let samples = render_samples(&mut unit, 2000);
    let level = rms(&samples);

    assert!(level > 0.001, "Note should produce audio, RMS={}", level);
}

#[test]
fn test_velocity_affects_volume() {
    let sf = load_test_soundfont();

    // Soft note
    let settings = SynthesizerSettings::new(44100);
    let mut unit_soft =
        SoundFontUnit::new(Arc::clone(&sf), &settings).expect("Failed to create SoundFontUnit");
    unit_soft.note_on(0, 60, 30);
    let samples_soft = render_samples(&mut unit_soft, 2000);
    let rms_soft = rms(&samples_soft);

    // Loud note
    let mut unit_loud = SoundFontUnit::new(sf, &settings).expect("Failed to create SoundFontUnit");
    unit_loud.note_on(0, 60, 127);
    let samples_loud = render_samples(&mut unit_loud, 2000);
    let rms_loud = rms(&samples_loud);

    assert!(
        rms_loud > rms_soft,
        "Loud note (vel=127, RMS={}) should be louder than soft (vel=30, RMS={})",
        rms_loud,
        rms_soft
    );
}

#[test]
fn test_note_off_stops_sound() {
    let sf = load_test_soundfont();

    let settings = SynthesizerSettings::new(44100);
    let mut unit = SoundFontUnit::new(sf, &settings).expect("Failed to create SoundFontUnit");

    // Play note
    unit.note_on(0, 60, 100);
    let samples_playing = render_samples(&mut unit, 500);
    let rms_playing = rms(&samples_playing);

    // Release note
    unit.note_off(0, 60);

    // Wait for release to complete (longer for piano sounds)
    let _ = render_samples(&mut unit, 20000);

    // Now should be much quieter
    let samples_after = render_samples(&mut unit, 1000);
    let rms_after = rms(&samples_after);

    assert!(
        rms_after < rms_playing * 0.1,
        "After note off and decay, RMS={} should be much less than playing RMS={}",
        rms_after,
        rms_playing
    );
}

#[test]
fn test_polyphony_multiple_notes() {
    let sf = load_test_soundfont();

    let settings = SynthesizerSettings::new(44100);

    // Single note
    let mut unit_single =
        SoundFontUnit::new(Arc::clone(&sf), &settings).expect("Failed to create SoundFontUnit");
    unit_single.note_on(0, 60, 80);
    let samples_single = render_samples(&mut unit_single, 2000);
    let rms_single = rms(&samples_single);

    // Chord (3 notes)
    let mut unit_chord = SoundFontUnit::new(sf, &settings).expect("Failed to create SoundFontUnit");
    unit_chord.note_on(0, 60, 80); // C
    unit_chord.note_on(0, 64, 80); // E
    unit_chord.note_on(0, 67, 80); // G
    let samples_chord = render_samples(&mut unit_chord, 2000);
    let rms_chord = rms(&samples_chord);

    assert!(
        rms_chord > rms_single,
        "Chord RMS={} should be louder than single note RMS={}",
        rms_chord,
        rms_single
    );
}

#[test]
fn test_reset_silences_all_notes() {
    let sf = load_test_soundfont();

    let settings = SynthesizerSettings::new(44100);
    let mut unit = SoundFontUnit::new(sf, &settings).expect("Failed to create SoundFontUnit");

    // Play several notes
    unit.note_on(0, 60, 100);
    unit.note_on(0, 64, 100);
    unit.note_on(0, 67, 100);

    // Confirm audio is playing
    let samples_playing = render_samples(&mut unit, 500);
    let rms_playing = rms(&samples_playing);
    assert!(rms_playing > 0.001);

    // Reset
    unit.reset();

    // Wait for any release to complete
    let _ = render_samples(&mut unit, 20000);

    // Should be silent
    let samples_after = render_samples(&mut unit, 1000);
    let rms_after = rms(&samples_after);

    assert!(
        rms_after < 0.001,
        "After reset and decay, should be silent, RMS={}",
        rms_after
    );
}

#[test]
fn test_clone_creates_independent_instance() {
    let sf = load_test_soundfont();

    let settings = SynthesizerSettings::new(44100);
    let mut unit = SoundFontUnit::new(sf, &settings).expect("Failed to create SoundFontUnit");

    // Play note on original
    unit.note_on(0, 60, 100);
    let _ = render_samples(&mut unit, 100);

    // Clone
    let mut clone = unit.clone();

    // RustySynth clones the synthesizer state, so both start with the note
    // already sounding — a clone is not a fresh voice. Independence is
    // therefore checked by playing a *different* note on the clone and
    // asserting the two renders diverge.
    clone.note_on(0, 72, 100);

    let samples_original = render_samples(&mut unit, 1000);
    let samples_clone = render_samples(&mut clone, 1000);

    // Both should produce audio (unit has C4, clone has C4+C5)
    let rms_original = rms(&samples_original);
    let rms_clone = rms(&samples_clone);

    assert!(rms_original > 0.001);
    assert!(rms_clone > 0.001);
    // Clone has extra note, should be louder
    assert!(rms_clone > rms_original);
}
