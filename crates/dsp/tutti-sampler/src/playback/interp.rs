//! Shared, zero-alloc interpolation kernel for the sampler playback units.
//!
//! Both the in-memory [`SamplerUnit`](super::sampler_unit::SamplerUnit) and the
//! disk-streaming [`StreamingSamplerUnit`](super::streaming_sampler::StreamingSamplerUnit)
//! read fractional sample positions. Historically the in-memory path used a
//! 2-tap linear lerp while the streaming path used 4-tap cubic Hermite, so the
//! same clip sounded different (and worse) on the live timeline. This module is
//! the single source of truth: one `cubic_hermite`, one stereo frame reader.
//!
//! Everything here is pure per-sample arithmetic — no allocation, no locks —
//! so it is safe to call from `process`/`tick` hot paths.

use std::sync::Arc;
use tutti_core::{BeatDuration, BeatPosition, TransportReader, Wave};

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
/// Returns `None` when the transport is stopped, the playhead is before
/// `start_beat`, past `duration`, or the tempo is non-positive. Otherwise the
/// value is `beat_offset * 60 / tempo * file_sample_rate`.
///
/// Pure arithmetic: no allocation, no locks — safe from `tick`/`process` hot
/// paths.
#[inline]
pub fn transport_sample_offset(
    transport: &dyn TransportReader,
    start_beat: BeatPosition,
    duration: Option<BeatDuration>,
    file_sample_rate: f64,
) -> Option<f64> {
    if !transport.is_playing() {
        return None;
    }
    let beat_offset = transport.current_beat() - start_beat.get();
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
    Some(seconds_offset * file_sample_rate)
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

    if wave.channels() >= 2 {
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
    } else {
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
