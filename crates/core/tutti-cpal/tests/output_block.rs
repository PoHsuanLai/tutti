//! The output callback, tested without a sound card.
//!
//! None of this was reachable before [`OutputBlock`] existed. The
//! `MAX_FRAMES` clamp, the zero-fill, the stereo metering fold and the
//! eight-way sample-format conversion all lived inside the closure handed to
//! `build_output_stream`, so the only thing that could run them was CPAL with
//! a device open — and this crate had six tests, none of which touched any of
//! it. That is most of the code a device-layer bug hides in.
//!
//! Expected values are stated from **each format's own arithmetic**, never by
//! calling `T::from_sample`. That is the argument `tutti-export`'s
//! `tests/roundtrip.rs:46` makes for recomputing a quantization independently:
//! asking the function under test what it should have produced is not a
//! second opinion.
//!
//! Mutations run against `block.rs`, and which test each broke:
//!
//! | mutation | fails |
//! |---|---|
//! | scale the conversion by 0.5 in `write_output` | `every_sample_format_carries_the_dc_level_it_was_given` |
//! | `frame < rendered_frames` → `true` (no tail silencing) | `an_oversized_callback_silences_its_tail` |
//! | drop `.min(MAX_FRAMES)` | `an_oversized_callback_silences_its_tail` (index panic) |
//! | `fold_frame(f, out)` → `out.copy_from_slice(&f[..2])` | `a_wide_device_meters_a_fold_not_the_first_two_channels` |
//! | delete the `layout == STEREO` memcpy branch | nothing — see `the_stereo_metering_shortcut_agrees_with_the_fold` |

mod support;

use support::{dc_state, rolling_state};
use tutti_core::ChannelLayout;
use tutti_cpal::{OutputBlock, MAX_FRAMES};

/// Full scale for an integer format, from the format's definition rather than
/// from the conversion under test.
fn dc_and_check<T: cpal::SizedSample + cpal::FromSample<f32> + PartialEq + std::fmt::Debug>(
    level: f32,
    expected: T,
    label: &str,
) {
    let mut block = OutputBlock::new(dc_state(level, 2), ChannelLayout::STEREO);
    let mut out = [T::from_sample(0.0f32); 64];
    block.render(&mut out);
    for (i, s) in out.iter().enumerate() {
        assert_eq!(
            *s, expected,
            "{label}: sample {i} of a constant {level} block should be \
             {expected:?}, got {s:?}"
        );
    }
}

/// **The eight-way sample-format matrix, one constant level through each.**
///
/// A constant-DC graph makes every sample in the block the same known value,
/// so the expected result is a single number per format, derived from that
/// format's range. `dasp`'s conversions are the thing being checked, so none
/// of these numbers comes from asking it.
#[test]
fn every_sample_format_carries_the_dc_level_it_was_given() {
    // Half of full scale. The integer expectations are `0.5 * 2^(bits-1)`,
    // derived from each format's width rather than from the conversion: the
    // scale factor is the *magnitude* 2^(bits-1), with the positive extreme
    // clamped, which is why i16 gives 16384 and not 16383. Stating it this way
    // is what makes a mutation to the scale factor visible.
    dc_and_check::<f32>(0.5, 0.5, "f32");
    dc_and_check::<f64>(0.5, 0.5, "f64");
    dc_and_check::<i16>(0.5, 1 << 14, "i16"); // 0.5 * 2^15
    dc_and_check::<i8>(0.5, 1 << 6, "i8"); //    0.5 * 2^7
    dc_and_check::<i32>(0.5, 1 << 30, "i32"); // 0.5 * 2^31
                                              // Unsigned formats are offset-binary: silence is mid-scale.
    dc_and_check::<u8>(0.0, 128, "u8 silence");
    dc_and_check::<u16>(0.0, 32768, "u16 silence");
    dc_and_check::<u32>(0.0, 2_147_483_648, "u32 silence");
}

/// **Silence is silence in every format**, including the offset-binary ones
/// where "zero" is not zero. A conversion that emitted a literal 0 for `u16`
/// would produce full-negative DC — a loud thump, not silence.
#[test]
fn silence_is_the_formats_own_zero_not_a_literal_zero() {
    dc_and_check::<i16>(0.0, 0, "i16");
    dc_and_check::<u8>(0.0, 128, "u8");
    dc_and_check::<u16>(0.0, 32768, "u16");
}

/// **An over-sized callback renders its head and silences its tail.**
///
/// The clamp exists so the callback never allocates. It is observable here
/// only because the `debug_assert` on the CPAL contract lives at the *driver*
/// boundary rather than inside `render` — it is a claim about CPAL, not about
/// the block, and keeping it out is what lets a debug build reach this.
#[test]
fn an_oversized_callback_silences_its_tail() {
    let mut block = OutputBlock::new(dc_state(1.0, 2), ChannelLayout::STEREO);

    let frames = MAX_FRAMES + 64;
    let mut out = vec![-1.0f32; frames * 2];
    block.render(&mut out);

    assert_eq!(
        block.last_rendered_frames(),
        MAX_FRAMES,
        "the clamp must report what it actually rendered, not what it was asked for"
    );
    assert!(
        out[..MAX_FRAMES * 2].iter().all(|&s| s == 1.0),
        "the head must carry the rendered signal"
    );
    assert!(
        out[MAX_FRAMES * 2..].iter().all(|&s| s == 0.0),
        "the tail past MAX_FRAMES must be silence, not the caller's stale \
         buffer contents"
    );
}

/// **A wide device meters a fold of every channel, not the first two.**
///
/// `meter_output` and the UI waveform assume stereo, so the callback folds the
/// device buffer down before metering. Taking the first two channels instead
/// would drop the centre and the surrounds entirely — a 5.1 mix whose
/// dialogue never appears on the meter and never reaches a master recording.
///
/// The signal is put on the **centre** channel with the front pair silent,
/// which is the only arrangement that separates the two behaviours: under a
/// uniform-DC graph a fold and a `[..2]` truncation agree exactly, and an
/// earlier draft of this test proved nothing for that reason.
///
/// Read through the tap rather than the meter: the tap carries the same folded
/// buffer frame by frame, where the meter reports a smoothed level.
#[test]
fn a_wide_device_meters_a_fold_not_the_first_two_channels() {
    // 5.1 order: L, R, C, LFE, Ls, Rs. Channel 2 is the centre.
    let (state, mut cons) = support::dc_on_channel(1.0, 2, 6);
    let mut block = OutputBlock::new(state, ChannelLayout::from(6usize));
    let mut out = vec![0.0f32; 64 * 6];
    block.render(&mut out);

    // The device buffer itself: silent front pair, loud centre.
    assert_eq!(out[0], 0.0, "left must be silent");
    assert_eq!(out[1], 0.0, "right must be silent");
    assert_eq!(out[2], 1.0, "the centre carries the signal");

    let (l, r) = cons
        .try_pop()
        .expect("the callback must have pushed a metered frame");
    assert!(
        l != 0.0 || r != 0.0,
        "the metered fold must carry the centre channel; it read ({l}, {r}), \
         which is what a `[..2]` truncation of this buffer would give"
    );
}

/// **The stereo memcpy shortcut is bit-equal to the general fold.**
///
/// `block.rs` claims the `layout == STEREO` branch "is a deliberate
/// optimization, not divergent logic", because `fold_frame`'s 2-wide arm is
/// already a passthrough, and warns that if that ever stops being true the
/// branch has to go rather than be patched. Nothing checked the claim.
///
/// Honest about what this can and cannot do: it pins `fold_frame`'s stereo arm
/// as a passthrough, which is the premise the shortcut rests on. It cannot
/// detect the shortcut being *deleted*, because both paths then agree — that
/// is a performance change, not a correctness one, and a test is the wrong
/// instrument for it.
#[test]
fn the_stereo_metering_shortcut_agrees_with_the_fold() {
    let mut folded = [0.0f32; 2];
    tutti_core::fold_frame(&[0.25, -0.75], &mut folded);
    assert_eq!(
        folded,
        [0.25, -0.75],
        "fold_frame's 2-wide arm must stay a passthrough — the callback's \
         stereo memcpy branch is only valid while it is. If this fails, delete \
         that branch rather than patching it (see block.rs)."
    );
}

/// The block reports the width it was built for, and renders at it.
#[test]
fn the_block_renders_at_the_width_it_was_built_for() {
    for width in [1usize, 2, 6, 8] {
        let mut block = OutputBlock::new(rolling_state(width).1, ChannelLayout::from(width));
        assert_eq!(block.channels(), width);
        let mut out = vec![0.0f32; 32 * width];
        block.render(&mut out);
        assert_eq!(block.last_rendered_frames(), 32);
    }
}

/// A zero-length callback is a no-op, not a panic.
///
/// Reachable in practice: some backends hand over an empty buffer on the first
/// callback after a device change.
#[test]
fn an_empty_callback_renders_nothing_and_does_not_panic() {
    let mut block = OutputBlock::new(dc_state(1.0, 2), ChannelLayout::STEREO);
    let mut out: [f32; 0] = [];
    block.render(&mut out);
    assert_eq!(block.last_rendered_frames(), 0);
}
