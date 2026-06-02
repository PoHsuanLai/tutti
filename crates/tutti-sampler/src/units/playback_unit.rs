//! Shared trait for in-memory and streaming playback units.

use tutti_core::AudioUnit;

/// Common parameter surface for in-memory and streaming playback units.
///
/// Covers operations that apply identically regardless of whether
/// the unit reads from RAM (`SamplerUnit`) or from a disk-streaming
/// ring buffer (`StreamingSamplerUnit`). The audio `process()` loop
/// still branches on the concrete type because random-access sampling
/// and sequential streaming are fundamentally different models.
pub trait PlaybackUnit: AudioUnit {
    fn set_gain(&mut self, gain: f32);
    fn set_speed(&mut self, speed: f32);
    fn play(&self);
    fn stop(&self);
    fn is_playing(&self) -> bool;
}
