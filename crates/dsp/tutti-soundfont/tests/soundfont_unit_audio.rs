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

use tutti_core::AudioUnit;
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

/// A note-on's `frame_offset` shifts the render by exactly that many frames.
///
/// # The exact assertion, and why it is this one
///
/// For offsets 16, 32 and 48 within a 64-frame block, the rendered output from
/// frame `offset` onward is **byte-identical** to the offset-0 render from
/// frame 0 onward. Not "similar", not "within epsilon": the same note played
/// `N` frames later through a deterministic synthesizer produces the same
/// samples `N` frames later, so equality is the honest assertion and anything
/// weaker would pass on a unit that merely delayed by a chunk.
///
/// The three offsets are chosen at multiples of 8 deliberately — that is the
/// resolution `SoundFontUnit` actually achieves. `process` splits the block at
/// each event offset exactly, but rustysynth serves frames from an internal
/// `block_size` chunk it fills whole, and 8 is the smallest `block_size` it
/// accepts. Offsets 16 and 20 would still collide; 16 and 24 do not.
///
/// # What this used to assert
///
/// The previous version of this test pinned the **defect**: it asserted that
/// offsets 16, 32 and 48 were byte-identical to each other and each equal to
/// the offset-0 render delayed by exactly one 64-frame chunk, because
/// `refill_buffers` rendered rustysynth's whole chunk in one call before any
/// mid-block event could reach it. Measured on this fixture, the offset-0 note
/// first sounded at frame 0 and every non-zero offset first sounded at frame
/// 64 regardless of its value. After the fix the same four renders first sound
/// at frames 41, 57, 73 and 89 — each exactly `offset + 41`, the 41 being the
/// fixture's own attack ramp.
///
/// # Mutation
///
/// Both halves of the fix were reverted independently and this test caught each:
///
/// - Render the whole block ignoring offsets (apply every event, then one
///   `render_range(0..size)`) → fails.
/// - Keep the split but restore rustysynth's default `block_size` of 64 → also
///   fails, which is the half that is easy to miss: the split alone does not
///   fix the defect, because the internal chunk is still filled whole.
#[test]
fn process_honors_frame_offset_within_block() {
    let sf = load_test_soundfont();
    let settings = SynthesizerSettings::new(44100);
    const BLOCK: usize = 64;
    const BLOCKS: usize = 6;
    const TOTAL: usize = BLOCK * BLOCKS;
    /// Multiples of the 8-frame resolution `SoundFontUnit` resolves.
    const OFFSETS: [usize; 3] = [16, 32, 48];

    let note = |offset: u32| {
        MidiEvent::note_on_7bit(MidiGroup::FIRST, MidiChannel::FIRST, 60, 100)
            .with_frame_offset(offset)
    };

    let render_at = |offset: u32| {
        let mut unit =
            SoundFontUnit::new(Arc::clone(&sf), &settings).expect("create SoundFontUnit");
        render_process_blocks(&mut unit, BLOCK, BLOCKS, &[note(offset)])
    };

    let base = render_at(0);
    assert!(
        rms(&base) > 0.001,
        "the offset-0 note must sound at all, else every comparison below is vacuous"
    );

    let shifted: Vec<Vec<(f32, f32)>> =
        OFFSETS.iter().map(|&o| render_at(o as u32)).collect();

    for (i, &offset) in OFFSETS.iter().enumerate() {
        // The whole property: offset N is offset 0, N frames later, exactly.
        assert_eq!(
            shifted[i][offset..],
            base[..TOTAL - offset],
            "offset {offset} must be the offset-0 render shifted by exactly {offset} frames"
        );
        // And it is silent before its own offset — the event cannot reach back.
        assert_eq!(
            rms(&shifted[i][..offset]),
            0.0,
            "nothing may sound before frame {offset}"
        );
    }

    // The offsets must differ from one another. This is what the old
    // chunk-resolution behaviour failed: 16, 32 and 48 were byte-identical.
    for i in 1..OFFSETS.len() {
        assert_ne!(
            shifted[i], shifted[i - 1],
            "offsets {} and {} must not render identically",
            OFFSETS[i - 1],
            OFFSETS[i]
        );
    }
}

/// Two events at different offsets in one block are each applied at their own
/// offset, not both at the first (or both at the block start).
///
/// Rendering key 60 at offset 0 together with key 67 at offset 32 must equal
/// the sample-wise sum of two **independently constructed** references: key 60
/// rendered at offset 0, and key 67 rendered at offset 0 then *shifted in the
/// test* by 32 frames. Voices mix additively ahead of the master gain, so if
/// either event landed at the wrong frame the sum would not match.
///
/// # Why the reference is shifted here rather than rendered at the offset
///
/// The obvious version renders the second note alone *at offset 32* and sums.
/// That version passes even when `process` ignores offsets entirely, because
/// the solo reference then suffers exactly the same collapse as the combined
/// render and the two errors cancel. It was written that way first and survived
/// the mutation below, which is what forced this shape: the reference must not
/// depend on the behaviour under test. Shifting a known-good offset-0 render in
/// the test harness is that independent reference.
///
/// This is also strictly stronger than "louder than one note" — a unit that
/// collapsed both events to offset 0 would still be louder.
///
/// Mutation: render whole block ignoring offsets → fails (the second note lands
/// at frame 0 instead of 32, so it diverges from the shifted reference).
#[test]
fn two_events_in_one_block_apply_at_their_own_offsets() {
    let sf = load_test_soundfont();
    // Reverb and chorus off: both are shared, stateful sends fed by *all*
    // voices, so with them on the render of two notes together is genuinely not
    // the sum of each alone — the additivity this test rests on is a property
    // of the dry voice mix. Every other setting is the default.
    let mut settings = SynthesizerSettings::new(44100);
    settings.enable_reverb_and_chorus = false;
    const BLOCK: usize = 64;
    // Long enough for a piano attack to clear the noise floor: the fixture's
    // envelope takes ~41 frames to leave zero and several hundred more to reach
    // an RMS a threshold can see. Six blocks (384 frames) is not enough, and a
    // precondition that fails is what caught it.
    const BLOCKS: usize = 48;
    const SECOND_OFFSET: u32 = 32;

    let note = |key: u8, offset: u32| {
        MidiEvent::note_on_7bit(MidiGroup::FIRST, MidiChannel::FIRST, key, 100)
            .with_frame_offset(offset)
    };

    let render = |events: &[MidiEvent]| {
        let mut unit =
            SoundFontUnit::new(Arc::clone(&sf), &settings).expect("create SoundFontUnit");
        render_process_blocks(&mut unit, BLOCK, BLOCKS, events)
    };

    // Distinct keys so the synthesizer allocates two voices rather than
    // retriggering one slot — a retrigger would not sum.
    let both = render(&[note(60, 0), note(67, SECOND_OFFSET)]);
    let first_alone = render(&[note(60, 0)]);

    // The independent reference: key 67 at offset 0, delayed by SECOND_OFFSET
    // frames *here*, not by the unit. Padding the head with silence is what the
    // unit is supposed to produce before the event fires.
    let shift = SECOND_OFFSET as usize;
    let second_at_zero = render(&[note(67, 0)]);
    let second_shifted: Vec<(f32, f32)> = core::iter::repeat_n((0.0, 0.0), shift)
        .chain(second_at_zero.iter().copied())
        .take(both.len())
        .collect();

    assert!(
        rms(&first_alone) > 0.001 && rms(&second_at_zero) > 0.001,
        "both notes must sound alone, else the sum below proves nothing"
    );

    // Voices mix additively ahead of the shared master gain, so the sum is
    // exact up to f32 rounding of a different summation order. The tolerance is
    // relative to the signal's own scale rather than an absolute epsilon: an
    // absolute one either trips on rounding at peak amplitude or is so loose it
    // stops discriminating in the quiet attack.
    let peak = both
        .iter()
        .flat_map(|&(l, r)| [l.abs(), r.abs()])
        .fold(0.0f32, f32::max);
    let tolerance = peak * 1e-3;
    for i in 0..both.len() {
        let expected = (
            first_alone[i].0 + second_shifted[i].0,
            first_alone[i].1 + second_shifted[i].1,
        );
        assert!(
            (both[i].0 - expected.0).abs() <= tolerance
                && (both[i].1 - expected.1).abs() <= tolerance,
            "frame {i}: two events in one block must render as the sum of each alone \
             at its own offset — got {:?}, expected {expected:?} (tolerance {tolerance:e})",
            both[i]
        );
    }

    // And the second note genuinely arrives late: before its offset, the
    // combined render must equal the first note alone.
    assert_eq!(
        both[..SECOND_OFFSET as usize],
        first_alone[..SECOND_OFFSET as usize],
        "before frame {SECOND_OFFSET} only the offset-0 note may be audible"
    );
}

/// An event at the last frame of a block is applied within that block, not
/// dropped and not deferred to the next one.
///
/// The boundary case for the split loop: the final segment is `[63, 64)`, one
/// frame long. A regression that rendered `[pos, next)` with `next <= pos`, or
/// that clamped the offset to `size` rather than `size - 1`, would either
/// panic on an inverted range or silently lose the event.
///
/// Mutation: render the whole block ignoring offsets → fails (the note lands at
/// frame 0, so the "silent before frame 63" assertion trips).
///
/// Note what this one does **not** catch: restoring rustysynth's default
/// `block_size` of 64 while keeping the split leaves it green, because at that
/// resolution offset 63 and offset 0 both round into the same chunk and the
/// remaining assertions are inequalities rather than equalities. That half of
/// the fix is pinned by `process_honors_frame_offset_within_block`; this test
/// is about the split loop's boundary arithmetic, not the resolution.
#[test]
fn event_at_last_frame_of_block_still_applies() {
    let sf = load_test_soundfont();
    let settings = SynthesizerSettings::new(44100);
    const BLOCK: usize = 64;
    const BLOCKS: usize = 6;
    const LAST: u32 = BLOCK as u32 - 1;

    let note = |offset: u32| {
        MidiEvent::note_on_7bit(MidiGroup::FIRST, MidiChannel::FIRST, 60, 100)
            .with_frame_offset(offset)
    };

    let mut unit = SoundFontUnit::new(Arc::clone(&sf), &settings).expect("create SoundFontUnit");
    let late = render_process_blocks(&mut unit, BLOCK, BLOCKS, &[note(LAST)]);

    let mut unit0 = SoundFontUnit::new(sf, &settings).expect("create SoundFontUnit");
    let base = render_process_blocks(&mut unit0, BLOCK, BLOCKS, &[note(0)]);

    // Applied, not dropped: the note sounds inside the render.
    assert!(
        rms(&late) > 0.001,
        "an event at the last frame of a block must still fire"
    );
    // Applied at frame 63 and not earlier.
    assert_eq!(
        rms(&late[..LAST as usize]),
        0.0,
        "an event at frame {LAST} may not sound before frame {LAST}"
    );
    // The 8-frame chunk floor rounds 63 down to 56, so this is not a
    // frame-exact shift — assert the weaker, true property: it is strictly
    // later than offset 0 and strictly earlier than a whole block late.
    let first_late = late.iter().position(|&(l, r)| l != 0.0 || r != 0.0);
    let first_base = base.iter().position(|&(l, r)| l != 0.0 || r != 0.0);
    let (first_late, first_base) = (
        first_late.expect("late note sounds"),
        first_base.expect("base note sounds"),
    );
    assert!(
        first_late > first_base,
        "offset {LAST} must sound later than offset 0 ({first_late} vs {first_base})"
    );
    assert!(
        first_late <= first_base + BLOCK,
        "offset {LAST} must land within this block, not a whole block late \
         ({first_late} vs {first_base})"
    );
}

/// A `set_sample_rate` call mid-stream is a documented no-op, and must not
/// disturb rendering — the unit keeps producing the same audio it would have.
///
/// The rate is fixed at construction (rustysynth cannot be re-rated), so the
/// property is *continuity*: rendering, calling `set_sample_rate`, then
/// rendering on must equal rendering straight through. The fix removed the
/// unit's own buffer-position state, and this pins that no stale-cursor bug
/// took its place.
///
/// Mutation: make `set_sample_rate` touch render state (a single
/// `render_range(0..1)` in the body) → fails, since the second half is then one
/// frame out of step with the straight-through render.
#[test]
fn set_sample_rate_mid_stream_does_not_disturb_rendering() {
    let sf = load_test_soundfont();
    let settings = SynthesizerSettings::new(44100);
    const BLOCK: usize = 64;
    const BLOCKS: usize = 6;

    let note =
        || MidiEvent::note_on_7bit(MidiGroup::FIRST, MidiChannel::FIRST, 60, 100);

    let mut straight = SoundFontUnit::new(Arc::clone(&sf), &settings).expect("create unit");
    let expected = render_process_blocks(&mut straight, BLOCK, BLOCKS, &[note()]);

    let mut interrupted = SoundFontUnit::new(sf, &settings).expect("create unit");
    let mut got = render_process_blocks(&mut interrupted, BLOCK, BLOCKS / 2, &[note()]);
    // The documented no-op, called between blocks.
    interrupted.set_sample_rate(tutti_core::SampleRate::from(48_000.0));
    got.extend(render_process_blocks(
        &mut interrupted,
        BLOCK,
        BLOCKS / 2,
        &[],
    ));

    assert!(rms(&expected) > 0.001, "the note must sound at all");
    assert_eq!(
        got, expected,
        "set_sample_rate is a no-op and must not perturb the render"
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
