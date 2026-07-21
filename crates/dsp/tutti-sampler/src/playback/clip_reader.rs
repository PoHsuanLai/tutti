//! The `ClipReader` control trait — one cold-path command surface shared by the
//! two clip-playback backends.
//!
//! Both the in-RAM [`SamplerUnit`](super::sampler_unit::SamplerUnit) and the
//! disk-streaming [`StreamingClipReader`](super::streaming_sampler::StreamingClipReader)
//! carry the same *control* verbs (gain, placement, speed, loop, seek, wave swap,
//! play/stop). The two backends' per-sample **reads** diverge essentially — one
//! indexes an `Arc<Wave>`, the other pops the butler ring — and that stays behind
//! the `ClipSource` enum's monomorphized match, never a trait object. This trait
//! covers only the control ops, which are applied on the cold command-drain path.
//!
//! # Not for the hot path
//!
//! `ClipReader` is **never** invoked per-sample. It exists so the command drain
//! speaks one control vocabulary instead of branching per backend. The RT
//! `tick`/`process` path continues to match on the `ClipSource` enum directly and
//! must stay alloc-free / lock-free.
//!
//! # Object-safety
//!
//! Every method takes concrete newtype params (no `impl Into`, no generics, no
//! `Self`-return) so the trait stays object-safe — a boxed `dyn ClipReader` is a
//! valid control handle even though the hot path never boxes the source.

use std::sync::Arc;

use tutti_core::{AudioUnit, BeatDuration, BeatPosition, Linear, Ratio, SamplePosition, Wave};

use super::sampler_unit::LoopSetting;

/// One cold-path control surface over a clip-playback backend.
///
/// See the [module docs](self) for why the per-sample read is deliberately
/// *not* part of this trait.
pub trait ClipReader: AudioUnit {
    /// Set the output gain (linear).
    fn set_gain(&mut self, gain: Linear);

    /// Update the timeline placement window (start + optional duration in beats).
    fn set_placement(&mut self, start_beat: BeatPosition, duration: Option<BeatDuration>);

    /// Set the playback speed magnitude. Direction is carried separately.
    fn set_speed(&mut self, speed: Ratio);

    /// Enable/replace (`On`) or disable (`Off`) looping.
    fn set_loop(&mut self, setting: LoopSetting);

    /// Seek to a clip-relative sample position.
    fn seek(&mut self, to: SamplePosition);

    /// Replace the backing wave (in-RAM backends only; a streaming source swap is
    /// a butler re-registration, not an in-unit op — see the streaming impl).
    fn set_wave(&mut self, wave: Arc<Wave>);

    fn play(&self);
    fn stop(&self);
    fn is_playing(&self) -> bool;
}
