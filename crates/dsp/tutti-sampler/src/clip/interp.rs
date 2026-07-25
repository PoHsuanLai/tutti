//! Shared, zero-alloc interpolation kernel for the sampler playback units.
//!
//! Both the in-memory [`SamplerUnit`](super::sampler_unit::SamplerUnit) and the
//! disk-streaming [`StreamingClipReader`](super::streaming_sampler::StreamingClipReader)
//! read fractional sample positions, so both must interpolate the same way or
//! the same clip sounds different on the two tiers. Shared here: one
//! `cubic_hermite` kernel and one transport-placement gate, used by both.
//!
//! `read_stereo_frame` is the in-RAM reader only — the streaming tier pulls
//! from the butler ring rather than an indexable `Wave`, so it feeds the same
//! kernel from its own 4-tap history. Same interpolation, different fetch.
//!
//! Everything here is pure per-sample arithmetic — no allocation, no locks —
//! so it is safe to call from `process`/`tick` hot paths.

use std::sync::Arc;
use tutti_core::{Beat, BeatDuration, ChannelLayout, Timeline, Wave};

/// Clip-relative sample offset the playhead sits at, or `None` when it is
/// outside the clip's transport window.
///
/// The single source of truth for the transport-placement gate shared by the
/// in-memory [`SamplerUnit`](super::sampler_unit::SamplerUnit) and the
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

/// Read one stereo frame from `wave` at fractional position `position` using
/// 4-tap cubic Hermite interpolation, with mono → stereo fan-out.
///
/// The four taps are `idx-1, idx, idx+1, idx+2` (where `idx = floor(position)`),
/// each clamped to the wave bounds so edges reuse the nearest valid sample.
/// Pure arithmetic: no allocation.
#[inline]
pub fn read_stereo_frame(wave: &Arc<Wave>, position: f64) -> (f32, f32) {
    let len = wave.len();
    if len == 0 {
        return (0.0, 0.0);
    }

    let idx = position.floor() as usize;
    let frac = position.fract() as f32;

    let last = len - 1;
    let im1 = idx.saturating_sub(1);
    let i0 = idx.min(last);
    let i1 = (idx + 1).min(last);
    let i2 = (idx + 2).min(last);

    // Stereo (or wider — we read the first two channels as L/R and ignore the
    // rest); mono fans the single channel out to both sides. A degenerate
    // 0-channel wave can't occur here (the `len == 0` guard above covers empty).
    match ChannelLayout::from(wave.channels()) {
        ChannelLayout::Stereo | ChannelLayout::Quad | ChannelLayout::Multi(_) => {
            let left = cubic_hermite(
                wave.at(0, im1),
                wave.at(0, i0),
                wave.at(0, i1),
                wave.at(0, i2),
                frac,
            );
            let right = cubic_hermite(
                wave.at(1, im1),
                wave.at(1, i0),
                wave.at(1, i1),
                wave.at(1, i2),
                frac,
            );
            (left, right)
        }
        ChannelLayout::Mono => {
            let mono = cubic_hermite(
                wave.at(0, im1),
                wave.at(0, i0),
                wave.at(0, i1),
                wave.at(0, i2),
                frac,
            );
            (mono, mono)
        }
    }
}
