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
use super::track_clip_reader::Direction;

/// One cold-path control surface over a clip-playback backend.
///
/// See the [module docs](self) for why the per-sample read is deliberately
/// *not* part of this trait.
pub trait ClipReader: AudioUnit {
    /// Set the output gain (linear).
    fn set_gain(&mut self, gain: Linear);

    /// Update the timeline placement window (start + optional duration in beats).
    fn set_placement(&mut self, start_beat: BeatPosition, duration: Option<BeatDuration>);

    /// Set the playback speed magnitude. Direction is carried separately (see
    /// [`set_direction`](Self::set_direction)).
    fn set_speed(&mut self, speed: Ratio);

    /// Set the playback direction. In-RAM playback carries direction *outside*
    /// the source (on the reader's per-slot `direction`, consumed by the reversed
    /// index in the hot read), so the in-RAM impl is a no-op — the drain sets the
    /// slot field directly. The streaming source has no slot-side read to reverse;
    /// its direction lives in the shared `RtState`, so the streaming impl forwards
    /// here (`RtState::set_direction`) — the same effect the butler's
    /// `SetVarispeed` direction leg produced.
    fn set_direction(&mut self, direction: Direction);

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

/// The raw sample-producer vocabulary a clip source speaks: report its rate and
/// (known) length, read the next block of stereo frames, and seek.
///
/// # Vocabulary, not dispatch
///
/// This trait NAMES the shared shape of the two producers — the in-RAM
/// [`SamplerUnit`](super::sampler_unit::SamplerUnit) reading an `Arc<Wave>` and
/// the disk-streaming
/// [`StreamingClipReader`](super::streaming_sampler::StreamingClipReader) popping
/// the butler ring. It is a **compose / construction bound only**: it exists so a
/// future `tutti-io` layer and the `Voice` builders can talk about "a sample
/// source" generically.
///
/// It is **never** stored as a `Box<dyn SampleSource>` and **never** dispatched
/// per-sample. The RT hot path continues to match the concrete
/// [`VoiceSource`](super::track_clip_reader::VoiceSource) enum directly, so the
/// per-frame read inlines and touches neither a vtable nor the heap. Keeping the
/// trait around as vocabulary does not change that: the enum stays the hot-path
/// dispatch; this trait is only ever used behind a generic bound at
/// construction time.
pub trait SampleSource {
    /// The producer's native sample rate.
    fn sample_rate(&self) -> f64;

    /// Total length in samples when known (in-RAM), or `None` when the length is
    /// not known up front (disk stream).
    fn len(&self) -> Option<usize>;

    /// Whether the source is empty (known length of zero). `None`-length streams
    /// are treated as non-empty.
    fn is_empty(&self) -> bool {
        matches!(self.len(), Some(0))
    }

    /// Fill `out` with the next block of stereo frames, advancing the read
    /// cursor. Not for the RT hot path — see the type-level note.
    fn read(&mut self, out: &mut [(f32, f32)]);

    /// Move the read cursor to `pos` (source-relative samples).
    fn seek(&mut self, pos: SamplePosition);
}

impl SampleSource for super::sampler_unit::SamplerUnit {
    fn sample_rate(&self) -> f64 {
        self.wave().sample_rate()
    }

    fn len(&self) -> Option<usize> {
        Some(self.duration_samples())
    }

    fn read(&mut self, out: &mut [(f32, f32)]) {
        // Advance a raw (gain-applied) read from the current position, matching
        // the in-RAM per-sample read. Construction-time helper only.
        let mut pos = self.position().get();
        let advance = (self.speed().get() * self.src_ratio().get()) as f64;
        for frame in out.iter_mut() {
            *frame = self.get_sample(pos);
            pos += advance;
        }
        self.trigger_at(SamplePosition::new(pos));
    }

    fn seek(&mut self, pos: SamplePosition) {
        self.trigger_at(pos);
    }
}

impl SampleSource for super::streaming_sampler::StreamingClipReader {
    fn sample_rate(&self) -> f64 {
        self.file_sample_rate()
    }

    fn len(&self) -> Option<usize> {
        // A disk stream's length is not known up front from the reader.
        None
    }

    fn read(&mut self, out: &mut [(f32, f32)]) {
        let mut frame = [0.0f32; 2];
        for slot in out.iter_mut() {
            self.tick(&[], &mut frame);
            *slot = (frame[0], frame[1]);
        }
    }

    fn seek(&mut self, pos: SamplePosition) {
        super::streaming_sampler::StreamingClipReader::seek(self, pos);
    }
}
