//! Runtime-width surround → stereo / mono downmix matrices.
//!
//! When a signal is wider than the sink it feeds — a 5.1/7.1 master played on a
//! stereo device, or read back through a stereo terminal — the extra channels
//! must be *folded in*, not dropped. Taking channels 0/1 (front L/R) and
//! discarding C/LFE/surrounds loses the dialogue (center) and the ambience
//! (surrounds) entirely.
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
//! Channel order is the file/SMPTE order these signals speak:
//! `FL FR C LFE SL SR [BL BR]`.
//!
//! These take a **runtime-width slice** — both the live audio path (which knows
//! the graph-root width only at runtime) and the offline export path fold through
//! them, so the coefficients live in exactly one place. They allocate nothing (a
//! handful of indexed reads + arithmetic), so they are safe on the RT callback
//! thread.

use crate::ChannelLayout;

/// −3 dB attenuation (`1/√2`) for center and surround fold-in.
pub const M3DB: f32 = core::f32::consts::FRAC_1_SQRT_2;

/// Read channel `i` of `frame`, or `0.0` if the frame is narrower.
#[inline]
fn ch(frame: &[f32], i: usize) -> f32 {
    frame.get(i).copied().unwrap_or(0.0)
}

/// Fold one interleaved surround `frame` (width = `frame.len()`) to a stereo
/// `(Lo, Ro)` pair using the ITU / Dolby matrix for its width. Widths without a
/// defined surround matrix (already ≤ 2, or an unrecognized layout) fall back to
/// front-pair passthrough.
///
/// - **≤2ch**: `(ch0, ch1-or-ch0)` — nothing to fold.
/// - **4ch (quad, FL FR BL BR)**: `Lo = FL + −3dB·BL`, `Ro = FR + −3dB·BR`.
/// - **6ch (5.1)**: `Lo = FL + −3dB·C + −3dB·SL`, `Ro = FR + −3dB·C + −3dB·SR`,
///   LFE dropped.
/// - **8ch (7.1)**: 5.1 fold plus the rear pair (BL/BR) into the surrounds.
#[inline]
pub fn fold_frame_to_stereo(frame: &[f32]) -> (f32, f32) {
    match frame.len() {
        0 => (0.0, 0.0),
        1 => (frame[0], frame[0]),
        2 => (frame[0], frame[1]),
        4 => {
            // Quad: FL FR BL BR (no center, no LFE).
            let (fl, fr, bl, br) = (ch(frame, 0), ch(frame, 1), ch(frame, 2), ch(frame, 3));
            (fl + M3DB * bl, fr + M3DB * br)
        }
        6 => {
            // 5.1: FL FR C LFE SL SR — LFE (idx 3) omitted.
            let (fl, fr, c, sl, sr) = (
                ch(frame, 0),
                ch(frame, 1),
                ch(frame, 2),
                ch(frame, 4),
                ch(frame, 5),
            );
            (fl + M3DB * c + M3DB * sl, fr + M3DB * c + M3DB * sr)
        }
        8 => {
            // 7.1: FL FR C LFE SL SR BL BR — LFE omitted, rears fold into surrounds.
            let (fl, fr, c) = (ch(frame, 0), ch(frame, 1), ch(frame, 2));
            let (sl, sr, bl, br) = (ch(frame, 4), ch(frame, 5), ch(frame, 6), ch(frame, 7));
            (
                fl + M3DB * c + M3DB * sl + M3DB * bl,
                fr + M3DB * c + M3DB * sr + M3DB * br,
            )
        }
        12 => {
            // 7.1.4 Atmos: FL FR C LFE Lss Rss Lrs Rrs + 4 heights (8..11).
            // LFE (idx 3) omitted; every left-side channel folds into Lo, every
            // right-side into Ro, at −3 dB (heights collapse to their floor pair).
            let (fl, fr, c) = (ch(frame, 0), ch(frame, 1), ch(frame, 2));
            let (lss, rss, lrs, rrs) = (ch(frame, 4), ch(frame, 5), ch(frame, 6), ch(frame, 7));
            // Heights are paired L/R: (8,10) left, (9,11) right in SMPTE order.
            let (ltf, rtf, ltr, rtr) = (ch(frame, 8), ch(frame, 9), ch(frame, 10), ch(frame, 11));
            (
                fl + M3DB * (c + lss + lrs + ltf + ltr),
                fr + M3DB * (c + rss + rrs + rtf + rtr),
            )
        }
        // Unknown wide layout: best-effort front-pair passthrough.
        _ => (ch(frame, 0), ch(frame, 1)),
    }
}

/// Fold one interleaved `frame` to a single mono sample.
///
/// For a **≤2-channel** source this is the plain L/R *average* (`(l+r)/2`) — the
/// long-standing mono convention, unchanged. For a **surround** source it is the
/// stereo matrix downmix summed with a further −3 dB, so the center and surrounds
/// fold in with correct relative levels (and the LFE is dropped) rather than
/// every channel being averaged with equal weight.
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

/// Fold a `src`-width interleaved frame into a `dst`-width frame, writing every
/// channel of `dst`. This is the general N→M form the live output path uses to
/// match the device width:
///
/// - `dst.len() == 1` → [`fold_frame_to_mono`].
/// - `dst.len() == 2` → [`fold_frame_to_stereo`] (folds surround, passes stereo,
///   **duplicates mono** into both sides).
/// - `dst.len() >= src.len()` (`> 2`) → straight copy, extra `dst` channels
///   zero-filled (never a synthetic upmix — a stereo signal on a 5.1 device
///   leaves C/LFE/surrounds silent).
/// - narrowing to some other width (`2 < dst < src`) → front-`dst` passthrough
///   (rare; no standard matrix for an arbitrary intermediate width).
///
/// Allocation-free: only indexed reads/writes over the two slices.
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

/// Fold a whole interleaved buffer to mono, `layout`-wide frames in.
///
/// The buffer-level counterpart to [`fold_frame_to_mono`], for the cold-path
/// consumers that hand a mono buffer to an analysis or a decoder. Every one of
/// those wrote its own version, and the ones that read only channels 0 and 1
/// silently discarded the centre and surrounds of anything wider.
///
/// `Mono` input is returned as-is. A trailing partial frame is ignored.
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

/// Fold planar channels to mono — one slice per channel, rather than one
/// interleaved buffer.
///
/// The shape decoders hand back. Channels shorter than the longest read as
/// silence past their end, so a ragged decode is padded rather than truncated.
///
/// # Why this stays
///
/// Its keep was questioned once on the belief it had no callers. It has one,
/// and it is the decoder path this was written for:
/// `dawai-extension-runtime`'s `resolve_get_clip_stft` folds a `Wave::load`
/// decode down to mono before the STFT. That call site replaced a hand-rolled
/// average of channels 0 and 1 that discarded a 5.1 source's centre — the
/// dialogue — and its surrounds. Deleting this reintroduces that bug the next
/// time someone needs a planar fold.
///
/// It also anchors [`fold_buffer_to_mono`]: the two must agree, which
/// `fold_planar_matches_interleaved` asserts. That test is the proof the
/// interleaved and planar folds are one policy rather than two drifting
/// copies, and it cannot exist without both halves.
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
    fn mono_from_stereo_is_the_plain_average() {
        assert!((fold_frame_to_mono(&[1.0, 0.0]) - 0.5).abs() < 1e-6);
        assert!((fold_frame_to_mono(&[1.0, 1.0]) - 1.0).abs() < 1e-6);
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
        assert_eq!(fold_buffer_to_mono(&mono, ChannelLayout::Mono), mono);

        let stereo = [1.0, 3.0, -2.0, 2.0];
        assert_eq!(
            fold_buffer_to_mono(&stereo, ChannelLayout::Stereo),
            vec![2.0, 0.0]
        );

        assert!(fold_buffer_to_mono(&[], ChannelLayout::Stereo).is_empty());
    }

    /// The defect in the hand-rolled consumer copies: channels 2..N vanish.
    #[test]
    fn fold_buffer_to_mono_keeps_the_centre_channel() {
        // 5.1 with only the centre non-zero — an L/R-only downmix returns
        // silence and loses the dialogue.
        let frame = [0.0, 0.0, 1.0, 0.0, 0.0, 0.0];
        let folded = fold_buffer_to_mono(&frame, ChannelLayout::Multi(6));

        assert_eq!(folded.len(), 1);
        assert!(folded[0] > 0.0, "centre must survive, got {}", folded[0]);
    }

    #[test]
    fn fold_buffer_to_mono_ignores_a_trailing_partial_frame() {
        let samples = [1.0, 1.0, 2.0, 2.0, 3.0];
        assert_eq!(
            fold_buffer_to_mono(&samples, ChannelLayout::Stereo),
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
            fold_buffer_to_mono(&interleaved, ChannelLayout::Stereo)
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
