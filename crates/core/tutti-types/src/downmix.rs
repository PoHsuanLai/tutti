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
/// - **any other width**: the front pair passes through.
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
/// order of operations live. [`fold_frame_to_stereo`] reads it from a
/// frame, [`fold_planar_channel`] from planar slices at one frame; with a
/// constant `width` the `match` folds away.
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
/// For a **≤2-channel** source this is the plain L/R *average* (`(l+r)/2`).
/// For a **surround** source it is the
/// stereo matrix downmix summed with a further −3 dB, so the center and surrounds
/// fold in with correct relative levels (and the LFE is dropped) rather than
/// every channel being averaged with equal weight.
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

/// Folds planar channels into one output channel: channel `dst_c` of the
/// `dst_width`-wide fold of `src_width` channels, `out.len()` frames long.
///
/// The planar counterpart of [`fold_frame`], for a graph node whose buffers
/// are one slice per channel: `src(c)` is input channel `c`, at least
/// `out.len()` frames long. Every output sample is bit-identical to the one
/// [`fold_frame`] writes for the same frame (the same coefficients, in the
/// same order of operations), but each width runs as its own loop over
/// whole slices, with no per-frame gather.
///
/// Allocation-free, so safe on the audio thread.
///
/// ```
/// use tutti_types::{fold_planar_channel, M3DB};
///
/// // 5.1 with only the centre active, folded to stereo: both sides get it.
/// let silent = [0.0f32; 4];
/// let centre = [1.0f32; 4];
/// let src = |c: usize| -> &[f32] { if c == 2 { &centre } else { &silent } };
/// let mut lo = [0.0f32; 4];
/// fold_planar_channel(src, 6, 2, 0, &mut lo);
/// assert_eq!(lo, [M3DB; 4]);
/// ```
///
/// # Panics
///
/// If `dst_c` is not below `dst_width`, or any of the first
/// `min(src_width, 12)` channels is shorter than `out`: those are all cut to
/// the block before the widths are matched, so this holds on the copy and
/// zero-fill paths too.
pub fn fold_planar_channel<'a>(
    src: impl Fn(usize) -> &'a [f32],
    src_width: usize,
    dst_width: usize,
    dst_c: usize,
    out: &mut [f32],
) {
    assert!(
        dst_c < dst_width,
        "output {dst_c} of a {dst_width}-wide fold"
    );
    let n = out.len();
    // The channels a stereo fold reads (at most 12), each cut to the block.
    let mut ch: [&[f32]; 12] = [&[]; 12];
    for (c, slot) in ch.iter_mut().enumerate().take(src_width) {
        *slot = &src(c)[..n];
    }
    match dst_width {
        1 => match src_width {
            0 => out.fill(0.0),
            1 => out.copy_from_slice(ch[0]),
            2 => {
                for ((o, &a), &b) in out.iter_mut().zip(ch[0]).zip(ch[1]) {
                    *o = (a + b) * 0.5;
                }
            }
            4 => mono::<4>(&ch, out),
            6 => mono::<6>(&ch, out),
            8 => mono::<8>(&ch, out),
            12 => mono::<12>(&ch, out),
            _ => mono::<2>(&ch, out),
        },
        2 => match src_width {
            0 => out.fill(0.0),
            1 => out.copy_from_slice(ch[0]),
            4 => side::<4>(&ch, dst_c, out),
            6 => side::<6>(&ch, dst_c, out),
            8 => side::<8>(&ch, dst_c, out),
            12 => side::<12>(&ch, dst_c, out),
            _ => out.copy_from_slice(ch[dst_c]),
        },
        // Equal width or upmix (zero-fill), or an intermediate narrowing
        // (front passthrough): a copy either way.
        _ if dst_c < src_width => out.copy_from_slice(&src(dst_c)[..n]),
        _ => out.fill(0.0),
    }
}

/// One side of the stereo fold of a `W`-wide source, over the block.
fn side<const W: usize>(ch: &[&[f32]; 12], s: usize, out: &mut [f32]) {
    for (i, o) in out.iter_mut().enumerate() {
        *o = stereo_side(W, s, |c| ch[c][i]);
    }
}

/// The mono fold of a `W`-wide surround source (`(Lo + Ro)·−3dB`), over the
/// block.
fn mono<const W: usize>(ch: &[&[f32]; 12], out: &mut [f32]) {
    for (i, o) in out.iter_mut().enumerate() {
        *o = (stereo_side(W, 0, |c| ch[c][i]) + stereo_side(W, 1, |c| ch[c][i])) * M3DB;
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

    #[test]
    fn fold_frame_equal_width_is_a_straight_copy() {
        let src = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let mut dst = [0.0; 6];
        fold_frame(&src, &mut dst);
        assert_eq!(dst, src);
    }

    /// The matrix of `src` → `dst`, written out: `m[o][k]` is how much of
    /// input channel `k` reaches output `o`.
    fn matrix(src: usize, dst: usize) -> Vec<Vec<f32>> {
        const M: f32 = M3DB;
        let stereo: [Vec<f32>; 2] = match src {
            1 => [vec![1.0], vec![1.0]],
            4 => [vec![1.0, 0.0, M, 0.0], vec![0.0, 1.0, 0.0, M]],
            6 => [
                vec![1.0, 0.0, M, 0.0, M, 0.0],
                vec![0.0, 1.0, M, 0.0, 0.0, M],
            ],
            8 => [
                vec![1.0, 0.0, M, 0.0, M, 0.0, M, 0.0],
                vec![0.0, 1.0, M, 0.0, 0.0, M, 0.0, M],
            ],
            12 => [
                vec![1.0, 0.0, M, 0.0, M, 0.0, M, 0.0, M, 0.0, M, 0.0],
                vec![0.0, 1.0, M, 0.0, 0.0, M, 0.0, M, 0.0, M, 0.0, M],
            ],
            // Every other width passes the front pair.
            n => {
                let unit = |c: usize| (0..n).map(|k| f32::from(u8::from(k == c))).collect();
                [unit(0), unit(1)]
            }
        };
        match dst {
            1 if src == 1 => vec![vec![1.0]],
            1 if src == 2 => vec![vec![0.5, 0.5]],
            1 => vec![stereo[0]
                .iter()
                .zip(&stereo[1])
                .map(|(l, r)| (l + r) * M)
                .collect()],
            2 => stereo.to_vec(),
            _ => (0..dst)
                .map(|o| (0..src).map(|k| f32::from(u8::from(k == o))).collect())
                .collect(),
        }
    }

    /// The planar fold is the downmix matrix: a block whose input channel
    /// `k` holds a unit pulse at frame `k` (and silence elsewhere) gives,
    /// at frame `k` of output `o`, the matrix entry `m[o][k]`, at every
    /// width the folds name and at some they do not.
    ///
    /// Mutation (run): swap `side::<8>` for `side::<6>` → the 7.1 rears
    /// vanish → fails; drop the `* M3DB` in `mono` → fails; read
    /// `ch[2 + s]` for the 5.1 surround → fails; zero-fill an equal-width
    /// fold → fails.
    #[test]
    fn the_planar_fold_applies_the_matrix_to_unit_vectors() {
        for src in [1usize, 2, 3, 4, 5, 6, 8, 12, 13] {
            let pulses: Vec<Vec<f32>> = (0..src)
                .map(|k| (0..src).map(|i| f32::from(u8::from(i == k))).collect())
                .collect();
            for dst in [1usize, 2, 3, 6, 8, 14] {
                let m = matrix(src, dst);
                for (o, row) in m.iter().enumerate() {
                    let mut out = vec![f32::NAN; src];
                    fold_planar_channel(|c| &pulses[c][..], src, dst, o, &mut out);
                    for (k, (&got, &want)) in out.iter().zip(row).enumerate() {
                        assert!(
                            (got - want).abs() < 1e-6,
                            "{src}->{dst}: m[{o}][{k}] is {got}, want {want}"
                        );
                    }
                }
            }
        }
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
