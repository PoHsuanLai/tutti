//! In-RAM vs disk-streaming tier policy — one threshold, shared by every
//! playback path (timeline clips *and* browser preview).
//!
//! A clip/preview whose decoded duration is at or below [`IN_MEMORY_SECS`]
//! loads whole into RAM and plays through a `SamplerUnit`; anything longer
//! streams incrementally from disk via the butler. dawai-model's
//! `TieringPolicy` defaults from this constant, so there is exactly one
//! source of truth for the threshold.

/// Maximum decoded duration (seconds) that plays in-RAM. Longer streams.
pub const IN_MEMORY_SECS: f64 = 30.0;

/// `true` if a source of `len_samples` at `sample_rate` should stream from
/// disk rather than load whole into RAM.
///
/// A zero/invalid `sample_rate` yields `false` (treat as in-RAM) — the
/// duration is unknowable, and the in-RAM path is the safe default.
#[inline]
pub fn should_stream(len_samples: usize, sample_rate: f64) -> bool {
    if sample_rate <= 0.0 {
        return false;
    }
    (len_samples as f64 / sample_rate) > IN_MEMORY_SECS
}
