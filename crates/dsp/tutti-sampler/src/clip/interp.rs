//! Shared, zero-alloc interpolation kernel for the sampler playback units.
//!
//! Both the in-memory [`MemorySource`](super::memory_source::MemorySource) and the
//! disk-streaming [`StreamingClipReader`](super::streaming_sampler::StreamingClipReader)
//! read fractional sample positions, so both must interpolate the same way or
//! the same clip sounds different on the two tiers. Shared here: one
//! `cubic_hermite` kernel and one transport-placement gate, used by both.
//!
//! `read_frame` is the in-memory reader only — the streaming tier pulls from the
//! butler ring rather than an indexable `Wave`, so it feeds the same kernel from
//! its own 4-tap history. Same interpolation, different fetch.
//!
//! `read_frame` also owns the crate's **channel policy** (see its docs); the
//! butler's planar unpack in [`wave_io`](crate::butler::io::wave_io) follows the
//! same rule. Two implementations of a channel policy is how the two tiers drift
//! apart — the same failure mode this module already exists to prevent for
//! interpolation.
//!
//! Everything here is pure per-sample arithmetic — no allocation, no locks —
//! so it is safe to call from `process`/`tick` hot paths.

use std::sync::Arc;
use tutti_core::{fold_frame, Beat, BeatDuration, Timeline, Wave};

use crate::MAX_SAMPLER_CHANNELS;

/// Clip-relative sample offset the playhead sits at, or `None` when it is
/// outside the clip's transport window.
///
/// The single source of truth for the transport-placement gate shared by the
/// in-memory [`MemorySource`](super::memory_source::MemorySource) and the
/// disk-streaming
/// [`StreamingClipReader`](super::streaming_sampler::StreamingClipReader).
/// Callers differ only in how they obtain `file_sample_rate` (the in-memory
/// unit reads `wave.sample_rate()`, the streaming reader stores it), so it is
/// passed in to keep this source-agnostic.
///
/// # A placed clip has no position of its own
///
/// Position is **derived** from the playhead, never accumulated here — the same
/// model `tutti_core`'s transport uses, where `TransportClock` is the one node
/// that advances time (`current_beat += beat_per_sample`) and everything
/// downstream reads the result. A clip that also carried a read cursor would be
/// a second, competing clock, and the two would drift apart the moment the
/// transport looped, seeked, or changed tempo.
///
/// `read_rate` scales the derived offset rather than stepping a cursor: at 0.5
/// the clip is half as far into its material for a given playhead position,
/// which is what "half speed" means for something the timeline owns. That is why
/// varispeed belongs *here*, in the beat→sample mapping, and not as a per-unit
/// `+= speed` accumulator. Build it with
/// [`PlaybackRate::read_rate`](tutti_core::PlaybackRate::read_rate) so the
/// varispeed and sample-rate-conversion factors compose in exactly one place.
///
/// Returns `None` when the transport is stopped, the playhead is before
/// `start_beat`, past `duration`, or the tempo is non-positive. Otherwise the
/// value is `beat_offset * 60 / tempo * file_sample_rate * read_rate`.
///
/// Pure arithmetic: no allocation, no locks — safe from `tick`/`process` hot
/// paths.
#[inline]
pub fn transport_sample_offset(
    transport: &dyn Timeline,
    start_beat: Beat,
    duration: Option<BeatDuration>,
    file_sample_rate: f64,
    read_rate: f64,
) -> Option<f64> {
    if !transport.is_rolling() {
        return None;
    }
    let beat_offset = transport.beat().get() - start_beat.get();
    if beat_offset < 0.0 {
        return None;
    }
    if let Some(dur) = duration {
        if beat_offset >= dur.get() {
            return None;
        }
    }
    let tempo = transport.tempo().get();
    if tempo <= 0.0 {
        return None;
    }
    let seconds_offset = beat_offset * 60.0 / tempo;
    Some(seconds_offset * file_sample_rate * read_rate)
}

/// Catmull-Rom cubic Hermite interpolation across four consecutive taps.
///
/// `y1` is the sample at the integer position, `y0`/`y2`/`y3` its neighbours
/// (`y0` one behind, `y2`/`y3` ahead); `t` is the fractional offset in
/// `[0, 1)` between `y1` and `y2`.
#[inline]
pub fn cubic_hermite(y0: f32, y1: f32, y2: f32, y3: f32, t: f32) -> f32 {
    let c0 = y1;
    let c1 = 0.5 * (y2 - y0);
    let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
    let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
    ((c3 * t + c2) * t + c1) * t + c0
}

/// Read one `out.len()`-wide frame from `wave` at fractional position
/// `position` using 4-tap cubic Hermite interpolation per channel.
///
/// The four taps are `idx-1, idx, idx+1, idx+2` (where `idx = floor(position)`),
/// each clamped to the wave bounds so edges reuse the nearest valid sample.
/// Pure arithmetic: no allocation, so this is safe on the RT thread.
///
/// **Writes every element of `out`**, including the silent cases — a caller
/// never has to pre-zero, and a partial write can never leave a stale channel
/// from the previous block in a trailing slot.
///
/// # Channel policy
///
/// One rule: **a mono source has no channel identity and fans to every channel;
/// anything else has one and folds through [`fold_frame`].**
///
/// - **mono wave → N-wide frame**: fans to all N. A mono sample is a point
///   source whose speaker placement is the panner's job, not the reader's, so
///   dropping it into channel 0 alone would be wrong. This is also what the
///   stereo kernel always did (mono duplicated to both sides), so the behaviour
///   at width 2 is unchanged.
/// - **wider wave → narrower frame**: [`fold_frame`], i.e. the ITU-R BS.775
///   matrix at width 2. Front-pair passthrough would silently drop the centre
///   (dialogue) and the surrounds (ambience). The engine already owns these
///   coefficients in exactly one place, and a sampler that folded differently
///   would make the same file sound different depending on the node's width.
/// - **narrower (but not mono) wave → wider frame**: [`fold_frame`], which
///   copies straight through and zero-fills the extra channels. Deliberately
///   asymmetric with the mono case: stereo carries a real L/R image, and fanning
///   it into the centre and surrounds would smear that image and invent phantom
///   centre content.
#[inline]
pub fn read_frame(wave: &Arc<Wave>, position: f64, out: &mut [f32]) {
    if out.is_empty() {
        return;
    }
    let len = wave.len();
    let src_ch = wave.channels();
    if len == 0 || src_ch == 0 {
        out.fill(0.0);
        return;
    }

    let idx = position.floor() as usize;
    let frac = position.fract() as f32;

    let last = len - 1;
    // All four taps clamp to `last`. `im1` needs it as much as the others:
    // `saturating_sub` only guards the LOW end, so a `position` past
    // `len` leaves it past the end too, and `Wave::at` is an unchecked
    // index — that is a panic, not a bad sample. The one in-tree caller happens
    // to gate on `position >= len` first, which is why it never fired.
    let im1 = idx.saturating_sub(1).min(last);
    let i0 = idx.min(last);
    let i1 = (idx + 1).min(last);
    let i2 = (idx + 2).min(last);

    // One interpolated sample from channel `c`. Every caller below derives `c`
    // from a bound that is `<= src_ch`, which is what keeps `Wave::at` — an
    // unchecked `self.vec[channel][index]` — from panicking.
    let tap = |c: usize| {
        cubic_hermite(
            wave.at(c, im1),
            wave.at(c, i0),
            wave.at(c, i1),
            wave.at(c, i2),
            frac,
        )
    };

    // Mono fans out. Bound: `c` is unused, only channel 0 is read.
    if src_ch == 1 {
        out.fill(tap(0));
        return;
    }

    // Matched width — the common case, including plain stereo. Bound: `src_ch
    // == out.len()`, so `0..out.len()` is in range. Kept as a distinct path
    // rather than routed through `fold_frame` so the hot case stays a straight
    // per-channel read with no matrix dispatch.
    if src_ch == out.len() {
        for (c, o) in out.iter_mut().enumerate() {
            *o = tap(c);
        }
        return;
    }

    // Mismatched width: interpolate at the source width into a bounded stack
    // frame, then let the engine's one matrix decide the mapping. Bound:
    // `0..n` where `n <= src_ch`.
    let n = src_ch.min(MAX_SAMPLER_CHANNELS);
    debug_assert!(
        src_ch <= MAX_SAMPLER_CHANNELS,
        "wave has {src_ch} channels, past MAX_SAMPLER_CHANNELS ({MAX_SAMPLER_CHANNELS}); \
         channels {MAX_SAMPLER_CHANNELS}.. are dropped by the fold"
    );
    let mut src = [0.0f32; MAX_SAMPLER_CHANNELS];
    for (c, s) in src.iter_mut().enumerate().take(n) {
        *s = tap(c);
    }
    fold_frame(&src[..n], out);
}

/// Stereo shim over [`read_frame`], preserving the original 2-channel signature.
#[inline]
pub fn read_stereo_frame(wave: &Arc<Wave>, position: f64) -> (f32, f32) {
    let mut out = [0.0f32; 2];
    read_frame(wave, position, &mut out);
    (out[0], out[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ch0 = (i+1)*0.01, ch1 = -(i+1)*0.01 — every sample distinct and the two
    /// channels distinguishable by sign, so a channel swap is visible.
    fn stereo_ramp() -> Arc<Wave> {
        let mut w = Wave::zero(2, 44_100.0, 64.0 / 44_100.0);
        for i in 0..w.len() {
            w.set(0, i, (i as f32 + 1.0) * 0.01);
            w.set(1, i, -((i as f32 + 1.0) * 0.01));
        }
        Arc::new(w)
    }

    fn mono_ramp() -> Arc<Wave> {
        let mut w = Wave::zero(1, 44_100.0, 64.0 / 44_100.0);
        for i in 0..w.len() {
            w.set(0, i, (i as f32 + 1.0) * 0.01);
        }
        Arc::new(w)
    }

    /// `channel c` carries the constant `c + 1`, so a wrong-channel read yields a
    /// wrong *value* rather than a plausible one.
    fn indexed_wave(channels: usize) -> Arc<Wave> {
        let mut w = Wave::zero(channels, 44_100.0, 32.0 / 44_100.0);
        for i in 0..w.len() {
            for c in 0..channels {
                w.set(c, i, (c + 1) as f32);
            }
        }
        Arc::new(w)
    }

    /// Golden-vector gate: width-2 output must be **bit-identical** to what the
    /// pre-width-generic stereo kernel produced.
    ///
    /// `assert_eq` on `f32::to_bits`, not an epsilon compare: a reassociated
    /// `cubic_hermite` or a fold that multiplies by 1.0 changes the last mantissa
    /// bit, and that is a real (if inaudible) divergence worth knowing about.
    ///
    /// These constants were captured by running the stereo kernel on `main`
    /// before any part of the width change existed. **Regenerating them is not
    /// the fix if this fails** — it would erase the only evidence that stereo
    /// still behaves as it did, which is the load-bearing claim for every
    /// unchanged app-side call site.
    #[test]
    fn stereo_output_is_bit_identical_to_the_pre_width_kernel() {
        const EXPECTED: [(u32, u32); 16] = [
            (1011413796, 3158897444),
            (1025222116, 3172705764),
            (1031865893, 3179349541),
            (1035221336, 3182704984),
            (1038576779, 3186060427),
            (1041059808, 3188543456),
            (1042737529, 3190221177),
            (1044415251, 3191898899),
            (1046092972, 3193576620),
            (1047770693, 3195254341),
            (1049012207, 3196495855),
            (1049851069, 3197334717),
            (1050689929, 3198173577),
            (1051528791, 3199012439),
            (1052367651, 3199851299),
            (1053206511, 3200690159),
        ];
        let w = stereo_ramp();
        let got: Vec<(u32, u32)> = (0..16)
            .map(|k| {
                let (l, r) = read_stereo_frame(&w, k as f64 * 2.5 + 0.3);
                (l.to_bits(), r.to_bits())
            })
            .collect();
        assert_eq!(
            got.as_slice(),
            &EXPECTED[..],
            "stereo playback diverged from the pre-width-generic kernel"
        );
    }

    /// The mono fan-out is likewise pinned bit-for-bit — it is the one policy
    /// arm that deliberately does *not* defer to `fold_frame`'s zero-fill.
    #[test]
    fn mono_fan_out_is_bit_identical_to_the_pre_width_kernel() {
        const EXPECTED: [(u32, u32); 8] = [
            (1015590651, 1015590651),
            (1028309124, 1028309124),
            (1034416029, 1034416029),
            (1038778106, 1038778106),
            (1041663787, 1041663787),
            (1043844824, 1043844824),
            (1046025863, 1046025863),
            (1048206901, 1048206901),
        ];
        let w = mono_ramp();
        let got: Vec<(u32, u32)> = (0..8)
            .map(|k| {
                let (l, r) = read_stereo_frame(&w, k as f64 * 3.25 + 0.7);
                (l.to_bits(), r.to_bits())
            })
            .collect();
        assert_eq!(got.as_slice(), &EXPECTED[..]);
    }

    /// A 6-channel wave must deliver **all six** channels, each to its own slot —
    /// not channel 0 fanned, not the front pair with four zeros. This is the
    /// truncation the old `Stereo | Quad | Multi(_) => (at(0), at(1))` arm caused.
    #[test]
    fn six_channel_wave_reaches_all_six_outputs() {
        let w = indexed_wave(6);
        let mut out = [0.0f32; 6];
        read_frame(&w, 4.0, &mut out);
        for (c, &got) in out.iter().enumerate() {
            assert!(
                (got - (c + 1) as f32).abs() < 1e-4,
                "channel {c} should carry {}, got {got} — full frame {out:?}",
                c + 1
            );
        }
        assert!(
            out[2..].iter().all(|&s| s.abs() > 0.5),
            "channels 2..6 were dropped: {out:?}"
        );
    }

    /// Mono has no channel identity, so it fans to every channel of a wide frame.
    #[test]
    fn mono_wave_fans_to_every_channel_of_a_six_wide_frame() {
        let w = mono_ramp();
        let mut out = [0.0f32; 6];
        read_frame(&w, 8.0, &mut out);
        assert!(out[0].abs() > 0.0, "mono read produced silence");
        for c in 1..6 {
            assert_eq!(out[c], out[0], "channel {c} did not receive the mono fan");
        }
    }

    /// Stereo *does* have a channel identity, so it occupies the front pair and
    /// leaves the rest silent. Pinned because the asymmetry with mono is
    /// deliberate: fanning L into the centre and surrounds would smear the image
    /// and invent phantom centre content.
    #[test]
    fn stereo_wave_on_a_six_wide_frame_zero_fills_the_surrounds() {
        let w = indexed_wave(2);
        let mut out = [9.0f32; 6];
        read_frame(&w, 4.0, &mut out);
        assert!((out[0] - 1.0).abs() < 1e-4);
        assert!((out[1] - 2.0).abs() < 1e-4);
        for (c, &s) in out.iter().enumerate().skip(2) {
            assert_eq!(s, 0.0, "channel {c} must be exactly silent, got {s}");
        }
    }

    /// Narrowing goes through the engine's ITU-R BS.775 matrix, not a front-pair
    /// truncation: a centre-only 5.1 source must reach **both** stereo outputs at
    /// −3 dB. Cross-checked against `fold_frame` directly so this asserts "we used
    /// the engine's matrix", not a number I chose.
    #[test]
    fn six_channel_wave_on_a_stereo_frame_folds_through_the_itu_matrix() {
        // 5.1 order: L R C LFE Ls Rs — centre only.
        let mut w = Wave::zero(6, 44_100.0, 32.0 / 44_100.0);
        for i in 0..w.len() {
            w.set(2, i, 1.0);
        }
        let w = Arc::new(w);

        let mut out = [0.0f32; 2];
        read_frame(&w, 4.0, &mut out);

        let mut expected = [0.0f32; 2];
        fold_frame(&[0.0, 0.0, 1.0, 0.0, 0.0, 0.0], &mut expected);
        assert!(
            (out[0] - expected[0]).abs() < 1e-6 && (out[1] - expected[1]).abs() < 1e-6,
            "expected the engine fold {expected:?}, got {out:?}"
        );
        assert!(
            out[0] > 0.5 && out[1] > 0.5,
            "centre was dropped — this is front-pair truncation, not a fold: {out:?}"
        );
    }

    /// A wave narrower than the frame must not read past its own channel count.
    /// `Wave::at` is an unchecked index, so getting this wrong panics rather than
    /// producing bad audio.
    #[test]
    fn narrow_wave_on_a_wide_frame_does_not_read_out_of_bounds() {
        let w = indexed_wave(2);
        let mut out = [0.0f32; MAX_SAMPLER_CHANNELS];
        read_frame(&w, 4.0, &mut out); // must not panic
    }

    #[test]
    fn empty_wave_and_zero_width_frame_are_silent() {
        let w = Arc::new(Wave::zero(2, 44_100.0, 0.0));
        let mut out = [7.0f32; 2];
        read_frame(&w, 0.0, &mut out);
        assert_eq!(out, [0.0, 0.0], "empty wave must produce silence");

        let w = stereo_ramp();
        read_frame(&w, 1.0, &mut []); // zero-width: must not panic
    }

    /// `read_frame` is `pub`, so it must survive a position past the end of the
    /// wave rather than relying on its caller to gate first.
    ///
    /// `Wave::at` is an unchecked `self.vec[c][i]`, so an unclamped tap panics
    /// instead of returning a wrong sample. `im1` used `saturating_sub(1)`,
    /// which guards only the LOW end — every other tap was `.min(last)`.
    #[test]
    fn position_past_the_end_does_not_panic() {
        let w = stereo_ramp();
        let len = w.len() as f64;
        let mut out = [0.0f32; 2];
        for pos in [len - 0.5, len, len + 1.0, len * 4.0, 1e9] {
            read_frame(&w, pos, &mut out); // must not panic
        }
    }
}
