//! Runtime-width surround → stereo / mono downmix matrices.
//!
//! When a signal is wider than the sink it feeds — a 5.1/7.1 master played on a
//! stereo device, or read back through a stereo terminal — the extra channels
//! must be *folded in*, not dropped. Taking channels 0/1 (front L/R) and
//! discarding C/LFE/surrounds loses the dialogue (center) and the ambience
//! (surrounds) entirely.
//!
//! # The coefficients
//!
//! These are the ITU-R BS.775 / Dolby consumer downmix coefficients, the same
//! ones a receiver applies when playing surround content on two speakers:
//!
//! ```text
//!   Lo = FL + (−3dB)·C + (−3dB)·SL         (5.1 → stereo)
//!   Ro = FR + (−3dB)·C + (−3dB)·SR
//! ```
//!
//! where −3 dB = `1/√2 ≈ 0.7071`. The LFE is **omitted** from the consumer
//! stereo/mono downmix (it carries no program-critical content and its
//! reproduction on small speakers is undefined). 7.1 folds the rear pair into
//! the side surrounds first. Mono sums the stereo downmix with a further −3 dB.
//!
//! # Limits
//!
//! - **Not normalised.** Each side adds its coefficients without scaling the
//!   sum back, so a 5.1 frame at full scale on every channel folds to
//!   `1 + 2·0.7071 = 2.414` per side, which clips. Headroom is the caller's.
//! - **Only some widths fold.** Widths 1, 2, 4, 6, 8 and 12 have a matrix. Any
//!   other width (3, 5, 7, 9 to 11, 13 and up, which include LCR, 5.0 and 7.0)
//!   passes the front pair through and drops the centre, surrounds and heights
//!   without warning. A width names a count, not a layout, so 6.0 and 5.1 are
//!   the same width, read as 5.1.
//! - **Mono is not continuous across widths.** Two channels average, three or
//!   more take the stereo fold times −3 dB: `[0.1, 0.2]` gives `0.15` and
//!   `[0.1, 0.2, 0.0]` gives `0.212`.
//!
//! # Channel order and RT safety
//!
//! Channel order is the file/SMPTE order these signals speak:
//! `FL FR C LFE SL SR [BL BR]`.
//!
//! These take a **runtime-width slice** — both the live audio path (which knows
//! the graph-root width only at runtime) and the offline export path fold through
//! them, so the coefficients live in exactly one place. They allocate nothing (a
//! handful of indexed reads + arithmetic), so they are safe on the RT callback
//! thread.

use crate::ChannelLayout;

/// The −3 dB attenuation (`1/√2 ≈ 0.7071`) the downmix folds use for the
/// centre and surround channels.
pub const M3DB: f32 = core::f32::consts::FRAC_1_SQRT_2;

/// Read channel `i` of `frame`, or `0.0` if the frame is narrower.
#[inline]
fn ch(frame: &[f32], i: usize) -> f32 {
    frame.get(i).copied().unwrap_or(0.0)
}

/// Folds one interleaved surround frame to a stereo `(Lo, Ro)` pair.
///
/// Replaced by [`Fold`](crate::Fold) for any caller that knows its topology.
///
/// The frame's width is `frame.len()`. When a signal is wider than the sink it
/// feeds, the extra channels are folded in rather than dropped, with the
/// ITU-R BS.775 / Dolby consumer downmix coefficients a receiver applies to
/// play surround on two speakers (−3 dB is [`M3DB`]). Channel order is
/// SMPTE: `FL FR C LFE SL SR [BL BR]`. The LFE is omitted: it carries no
/// program-critical content.
///
/// - **0–2 ch**: nothing to fold; mono is duplicated to both sides.
/// - **4 ch (quad, FL FR BL BR)**: `Lo = FL + −3dB·BL`, `Ro = FR + −3dB·BR`.
/// - **6 ch (5.1)**: `Lo = FL + −3dB·C + −3dB·SL`, `Ro = FR + −3dB·C + −3dB·SR`.
/// - **8 ch (7.1)**: the 5.1 fold plus the rear pair (BL/BR) into its side.
/// - **12 ch (7.1.4)**: every left-side channel, heights included, into `Lo`
///   and every right-side one into `Ro`, all at −3 dB.
/// - **any other width** (3, 5, 7, 9 to 11, 13 and up): the front pair passes
///   through, and the centre, surrounds and heights are **dropped silently**.
///
/// The matrix is **not normalised**: a 5.1 frame at full scale on every channel
/// gives `1 + 2·`[`M3DB`]` = 2.414` per side, which clips. Leave headroom or
/// scale before folding.
///
/// Allocation-free, so safe on the audio thread.
///
/// ```
/// use tutti_types::{fold_frame_to_stereo, M3DB};
///
/// // 5.1 with only the centre (dialogue) channel active.
/// let (lo, ro) = fold_frame_to_stereo(&[0.0, 0.0, 1.0, 0.0, 0.0, 0.0]);
/// assert_eq!((lo, ro), (M3DB, M3DB));
/// ```
#[inline]
pub fn fold_frame_to_stereo(frame: &[f32]) -> (f32, f32) {
    let n = frame.len();
    (
        stereo_side(n, 0, |c| frame[c]),
        stereo_side(n, 1, |c| frame[c]),
    )
}

/// Side `s` (0 left, 1 right) of the stereo fold of a `width`-wide frame
/// whose channel `c` is `get(c)`: the one place the coefficients and their
/// order of operations live. [`fold_frame_to_stereo`] reads it from a frame.
///
/// Reads only channels below `width`. Channel order is SMPTE:
/// `FL FR C LFE SL SR [BL BR]`, the LFE (3) never read.
#[inline(always)]
fn stereo_side(width: usize, s: usize, get: impl Fn(usize) -> f32) -> f32 {
    match width {
        0 => 0.0,
        // Mono is duplicated to both sides.
        1 => get(0),
        // Quad: FL FR BL BR (no center, no LFE).
        4 => get(s) + M3DB * get(2 + s),
        // 5.1: FL FR C LFE SL SR.
        6 => get(s) + M3DB * get(2) + M3DB * get(4 + s),
        // 7.1: FL FR C LFE SL SR BL BR, the rears into their side.
        8 => get(s) + M3DB * get(2) + M3DB * get(4 + s) + M3DB * get(6 + s),
        // 7.1.4: FL FR C LFE Lss Rss Lrs Rrs Ltf Rtf Ltr Rtr. Every channel
        // on a side, heights included, folds into it at −3 dB (the heights
        // pair L/R as (8, 10) and (9, 11)).
        12 => get(s) + M3DB * (get(2) + get(4 + s) + get(6 + s) + get(8 + s) + get(10 + s)),
        // Stereo, and any unknown width: the front pair passes through.
        _ => get(s),
    }
}

/// Folds one interleaved frame to a single mono sample.
///
/// Replaced by [`Fold`](crate::Fold) for any caller that knows its topology.
///
/// For a **≤2-channel** source this is the plain L/R *average* (`(l+r)/2`).
/// For a **surround** source it is the
/// stereo matrix downmix summed with a further −3 dB, so the center and surrounds
/// fold in with correct relative levels (and the LFE is dropped) rather than
/// every channel being averaged with equal weight.
///
/// The two rules do not meet: `[0.1, 0.2]` gives `0.15` and a three-wide
/// `[0.1, 0.2, 0.0]` gives `0.212`, so a signal that changes width changes
/// level. The unnormalised stereo matrix and the dropped channels of the
/// unfolded widths carry over from [`fold_frame_to_stereo`].
///
/// Allocation-free, so safe on the audio thread.
#[inline]
pub fn fold_frame_to_mono(frame: &[f32]) -> f32 {
    match frame.len() {
        0 => 0.0,
        1 => frame[0],
        2 => (frame[0] + frame[1]) * 0.5,
        _ => {
            let (lo, ro) = fold_frame_to_stereo(frame);
            (lo + ro) * M3DB
        }
    }
}

/// Folds a `src`-width interleaved frame into a `dst`-width frame, writing
/// every channel of `dst`.
///
/// What [`Fold::between`](crate::Fold::between) replaces: a fold by width, not by speaker.
///
/// This is the general N→M form the live output path uses to match the device
/// width:
///
/// - `dst.len() == 1` → [`fold_frame_to_mono`].
/// - `dst.len() == 2` → [`fold_frame_to_stereo`] (folds surround, passes stereo,
///   **duplicates mono** into both sides).
/// - `dst.len() >= src.len()` (`> 2`) → straight copy, extra `dst` channels
///   zero-filled (never a synthetic upmix — a stereo signal on a 5.1 device
///   leaves C/LFE/surrounds silent).
/// - narrowing to some other width (`2 < dst < src`) → front-`dst` passthrough
///   (there is no standard matrix for an arbitrary intermediate width).
///
/// The mono and stereo targets inherit the limits in the module docs: the
/// matrix is not normalised, and a source width with no matrix loses its
/// centre and surrounds.
///
/// Allocation-free (only indexed reads and writes over the two slices), so
/// safe on the audio thread.
#[inline]
pub fn fold_frame(src: &[f32], dst: &mut [f32]) {
    match dst.len() {
        0 => {}
        1 => dst[0] = fold_frame_to_mono(src),
        2 => {
            let (l, r) = fold_frame_to_stereo(src);
            dst[0] = l;
            dst[1] = r;
        }
        _ => {
            // Equal width or upmix (zero-fill): straight copy.
            for (i, o) in dst.iter_mut().enumerate() {
                *o = ch(src, i);
            }
        }
    }
}

/// Folds a whole interleaved buffer of `layout`-wide frames to mono.
///
/// The buffer-level counterpart to [`fold_frame_to_mono`], for cold-path
/// consumers that hand a mono buffer to an analysis or a decoder. Allocates the
/// result, so keep it off the audio thread.
///
/// `Mono` input is returned as a copy. A trailing partial frame is ignored.
pub fn fold_buffer_to_mono(samples: &[f32], layout: ChannelLayout) -> Vec<f32> {
    match layout.count() {
        0 => Vec::new(),
        1 => samples.to_vec(),
        n => samples
            .chunks_exact(n as usize)
            .map(fold_frame_to_mono)
            .collect(),
    }
}

/// Folds planar channels to mono — one slice per channel, rather than one
/// interleaved buffer.
///
/// The shape decoders hand back. Channels shorter than the longest read as
/// silence past their end, so a ragged decode is padded rather than truncated.
/// Gives the same result as [`fold_buffer_to_mono`] over the same audio
/// interleaved. Allocates the result, so keep it off the audio thread.
pub fn fold_planar_to_mono(channels: &[&[f32]]) -> Vec<f32> {
    match channels {
        [] => Vec::new(),
        [only] => only.to_vec(),
        _ => {
            let len = channels.iter().map(|c| c.len()).max().unwrap_or(0);
            let mut frame = vec![0.0f32; channels.len()];
            (0..len)
                .map(|i| {
                    for (slot, ch) in frame.iter_mut().zip(channels) {
                        *slot = ch.get(i).copied().unwrap_or(0.0);
                    }
                    fold_frame_to_mono(&frame)
                })
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The −3 dB coefficient is `1/√2`, 0.7071, and its square is a half
    /// (−3.0103 dB).
    ///
    /// Mutation (run): `M3DB` → `0.9 · FRAC_1_SQRT_2`, then → `0.5` → fails.
    #[test]
    #[allow(
        clippy::approx_constant,
        reason = "the figure is pinned independently of the constant under test"
    )]
    fn the_minus_3_db_coefficient_is_the_itu_figure() {
        assert!((M3DB - 0.7071).abs() < 1e-4, "{M3DB}");
        assert!((M3DB * M3DB - 0.5).abs() < 1e-7);
        assert!((20.0 * M3DB.log10() + 3.0103).abs() < 1e-3);
        // And the fold applies it as written, not through the constant alone.
        let (lo, ro) = fold_frame_to_stereo(&[0.0, 0.0, 1.0, 0.0, 0.0, 0.0]);
        assert!((lo - 0.7071).abs() < 1e-4 && lo == ro);
    }

    /// The matrix is not normalised: full scale on every 5.1 channel folds to
    /// `1 + 2 · 0.7071 = 2.4142` per side (the LFE is dropped).
    ///
    /// Mutation (run): normalise by dividing the 5.1 sum by `1 + 2·M3DB` →
    /// `1.0` → fails.
    #[test]
    fn full_scale_five_one_folds_above_full_scale() {
        let (lo, ro) = fold_frame_to_stereo(&[1.0; 6]);
        assert!((lo - 2.414_213_6).abs() < 1e-6, "{lo}");
        assert_eq!(lo, ro);
    }

    /// Widths with no matrix pass the front pair through and drop the rest.
    #[test]
    fn widths_without_a_matrix_drop_the_centre_and_surrounds() {
        // LCR, 5.0 and 7.0: the centre and surrounds vanish.
        assert_eq!(fold_frame_to_stereo(&[0.1, 0.2, 1.0]), (0.1, 0.2));
        assert_eq!(fold_frame_to_stereo(&[0.1, 0.2, 1.0, 1.0, 1.0]), (0.1, 0.2));
        assert_eq!(
            fold_frame_to_stereo(&[0.1, 0.2, 1.0, 1.0, 1.0, 1.0, 1.0]),
            (0.1, 0.2)
        );
        assert_eq!(
            fold_frame_to_stereo(&[0.1, 0.2, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0]),
            (0.1, 0.2)
        );
    }

    /// Mono is the average at two channels and the stereo fold times −3 dB from
    /// three up, so the level steps when the width does.
    #[test]
    fn mono_level_steps_between_two_and_three_channels() {
        assert!((fold_frame_to_mono(&[0.1, 0.2]) - 0.15).abs() < 1e-7);
        assert!((fold_frame_to_mono(&[0.1, 0.2, 0.0]) - 0.212_132).abs() < 1e-6);
    }

    #[test]
    fn stereo_and_below_pass_through() {
        assert_eq!(fold_frame_to_stereo(&[0.3, -0.4]), (0.3, -0.4));
        assert_eq!(fold_frame_to_stereo(&[0.5]), (0.5, 0.5));
        assert_eq!(fold_frame_to_stereo(&[]), (0.0, 0.0));
    }

    #[test]
    fn five_one_folds_center_and_surrounds_drops_lfe() {
        // FL FR C LFE SL SR
        let frame = [1.0, 2.0, 0.5, 9.9 /*LFE, ignored*/, 0.2, 0.4];
        let (lo, ro) = fold_frame_to_stereo(&frame);
        assert!((lo - (1.0 + M3DB * 0.5 + M3DB * 0.2)).abs() < 1e-6);
        assert!((ro - (2.0 + M3DB * 0.5 + M3DB * 0.4)).abs() < 1e-6);
        assert!(lo < 5.0 && ro < 5.0, "LFE leaked into the downmix");
    }

    #[test]
    fn center_source_splits_equally_to_both() {
        let frame = [0.0, 0.0, 1.0, 0.0, 0.0, 0.0];
        let (lo, ro) = fold_frame_to_stereo(&frame);
        assert!((lo - M3DB).abs() < 1e-6);
        assert!((ro - M3DB).abs() < 1e-6);
        assert!((lo - ro).abs() < 1e-6, "center must be symmetric");
    }

    #[test]
    fn seven_one_folds_rears_into_surrounds() {
        // FL FR C LFE SL SR BL BR — energy only in the rears.
        let frame = [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 2.0];
        let (lo, ro) = fold_frame_to_stereo(&frame);
        assert!((lo - M3DB * 1.0).abs() < 1e-6);
        assert!((ro - M3DB * 2.0).abs() < 1e-6);
    }

    #[test]
    fn quad_folds_backs_into_front() {
        // FL FR BL BR — a rear-only source folds −3dB into the front pair.
        let frame = [0.0, 0.0, 1.0, 2.0];
        let (lo, ro) = fold_frame_to_stereo(&frame);
        assert!((lo - M3DB * 1.0).abs() < 1e-6);
        assert!((ro - M3DB * 2.0).abs() < 1e-6);
    }

    #[test]
    fn atmos_7_1_4_folds_heights_and_surrounds_drops_lfe() {
        // FL FR C LFE Lss Rss Lrs Rrs Ltf Rtf Ltr Rtr — one unit in each channel.
        let frame = [1.0; 12];
        let (lo, ro) = fold_frame_to_stereo(&frame);
        // Lo = FL + .707·(C + Lss + Lrs + Ltf + Ltr) = 1 + .707·5; LFE dropped.
        let expect = 1.0 + M3DB * 5.0;
        assert!((lo - expect).abs() < 1e-6, "lo={lo} expect={expect}");
        assert!((ro - expect).abs() < 1e-6, "ro={ro} expect={expect}");
        // Symmetric, and the LFE (a 6th unit into each side) did NOT leak in.
        assert!((lo - ro).abs() < 1e-6);
        assert!(lo < 1.0 + M3DB * 6.0, "LFE leaked into the 7.1.4 downmix");
    }

    #[test]
    fn atmos_7_1_4_left_height_folds_left_only() {
        // Energy only in the top-front-LEFT height (idx 8) → left downmix only.
        let mut frame = [0.0; 12];
        frame[8] = 1.0;
        let (lo, ro) = fold_frame_to_stereo(&frame);
        assert!((lo - M3DB).abs() < 1e-6, "left height folds into Lo");
        assert!(ro.abs() < 1e-6, "left height must not reach Ro");
    }

    #[test]
    fn mono_from_surround_sums_the_matrix_downmix() {
        // 5.1 with front L/R only: stereo = (1,1) → mono = (1+1)*.707.
        let frame = [1.0, 1.0, 0.0, 0.0, 0.0, 0.0];
        assert!((fold_frame_to_mono(&frame) - 2.0 * M3DB).abs() < 1e-6);
    }

    #[test]
    fn fold_frame_matches_the_pair_helpers() {
        let src = [1.0, 2.0, 0.5, 0.0, 0.2, 0.4];
        // → stereo
        let mut st = [0.0; 2];
        fold_frame(&src, &mut st);
        assert_eq!((st[0], st[1]), fold_frame_to_stereo(&src));
        // → mono
        let mut mo = [0.0; 1];
        fold_frame(&src, &mut mo);
        assert!((mo[0] - fold_frame_to_mono(&src)).abs() < 1e-6);
    }

    #[test]
    fn fold_frame_upmix_zero_fills_extra_channels() {
        // Stereo source into a 6-wide dst: front pair copied, C/LFE/surrounds silent.
        let src = [0.7, -0.3];
        let mut dst = [9.9; 6];
        fold_frame(&src, &mut dst);
        assert_eq!(dst[0], 0.7);
        assert_eq!(dst[1], -0.3);
        for &s in &dst[2..] {
            assert_eq!(s, 0.0, "extra device channels must be silent, not upmixed");
        }
    }

    /// A rear-only 7.1 frame, which a front-pair truncation would fold to
    /// silence, survives the mono fold.
    ///
    /// Mutation (run): folding only `src[..2]` to mono fails.
    #[test]
    fn fold_frame_to_mono_keeps_the_surround_rears() {
        let rears_only = [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0];
        let mut mono = [0.0; 1];
        fold_frame(&rears_only, &mut mono);
        assert!((mono[0] - 2.0 * M3DB * M3DB).abs() < 1e-6, "{}", mono[0]);
    }

    /// A stereo frame into four channels keeps the pair and zero-fills the
    /// rest over stale data, so a caller emitting `dst` as a frame never
    /// emits a short or stale one. (`matrix` pins the planar fold only.)
    ///
    /// Mutation (run): the wide branch skipping its copy fails.
    #[test]
    fn fold_frame_zero_fills_a_wider_destination_exactly() {
        let mut dst = [9.9f32; 4];
        fold_frame(&[0.25, -0.25], &mut dst);
        assert_eq!(dst, [0.25, -0.25, 0.0, 0.0]);
    }

    /// Stereo into mono averages: an asymmetric pair tells the average from a
    /// dropped right channel.
    ///
    /// Mutation (run): `fold_frame` mono arm taking `src[0]` fails.
    #[test]
    fn fold_frame_stereo_to_mono_averages_asymmetric_input() {
        let mut mono = [0.0f32; 1];
        fold_frame(&[0.5, 0.9], &mut mono);
        assert!((mono[0] - 0.7).abs() < 1e-6, "{}", mono[0]);
    }

    #[test]
    fn fold_frame_equal_width_is_a_straight_copy() {
        let src = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let mut dst = [0.0; 6];
        fold_frame(&src, &mut dst);
        assert_eq!(dst, src);
    }

    #[test]
    fn fold_buffer_to_mono_handles_each_layout() {
        let mono = [1.0, 2.0, 3.0];
        assert_eq!(fold_buffer_to_mono(&mono, ChannelLayout::MONO), mono);

        let stereo = [1.0, 3.0, -2.0, 2.0];
        assert_eq!(
            fold_buffer_to_mono(&stereo, ChannelLayout::STEREO),
            vec![2.0, 0.0]
        );

        assert!(fold_buffer_to_mono(&[], ChannelLayout::STEREO).is_empty());
    }

    /// The defect in the hand-rolled consumer copies: channels 2..N vanish.
    #[test]
    fn fold_buffer_to_mono_keeps_the_centre_channel() {
        // 5.1 with only the centre non-zero — an L/R-only downmix returns
        // silence and loses the dialogue.
        let frame = [0.0, 0.0, 1.0, 0.0, 0.0, 0.0];
        let folded = fold_buffer_to_mono(&frame, ChannelLayout::from(6u16));

        assert_eq!(folded.len(), 1);
        assert!(folded[0] > 0.0, "centre must survive, got {}", folded[0]);
    }

    #[test]
    fn fold_buffer_to_mono_ignores_a_trailing_partial_frame() {
        let samples = [1.0, 1.0, 2.0, 2.0, 3.0];
        assert_eq!(
            fold_buffer_to_mono(&samples, ChannelLayout::STEREO),
            vec![1.0, 2.0]
        );
    }

    #[test]
    fn fold_planar_matches_interleaved() {
        let left = [1.0f32, -2.0, 3.0];
        let right = [3.0f32, 2.0, 1.0];
        let interleaved = [1.0, 3.0, -2.0, 2.0, 3.0, 1.0];

        assert_eq!(
            fold_planar_to_mono(&[&left, &right]),
            fold_buffer_to_mono(&interleaved, ChannelLayout::STEREO)
        );
    }

    #[test]
    fn fold_planar_pads_ragged_channels_rather_than_truncating() {
        let long = [1.0f32, 1.0, 1.0];
        let short = [1.0f32];
        let folded = fold_planar_to_mono(&[&long, &short]);

        assert_eq!(folded.len(), 3, "the longer channel sets the length");
        assert_eq!(folded[0], 1.0);
        assert_eq!(folded[1], 0.5, "missing samples read as silence");
    }
}
