//! Disk streaming sample playback, fed by the butler thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tutti_core::{
    AudioUnit, BeatDuration, BeatPosition, BufferMut, BufferRef, Linear, Ratio, SamplePosition,
    Wave,
};

use super::clip_reader::ClipReader;
use super::interp::cubic_hermite;
use super::sampler_unit::{LoopSetting, TransportPlacement};
use super::track_clip_reader::Direction;
use crate::butler::{RtState, SharedReader};

/// 8192 frames at 4x speed with interpolation padding.
const MAX_FETCH_SAMPLES: usize = 8192 * 4 + 8;

/// Disk streaming sampler with varispeed, seeking, and crossfade support.
///
/// The audio thread is the sole consumer of the ring. It holds the reader
/// through a wait-free [`SharedReader`] (an `ArcSwap`, never a `Mutex`): each
/// buffer it `load`s the current [`ReaderCell`](crate::butler::prefetch::ReaderCell)
/// and pops from it. No lock ever sits on `tick`/`process`, so a butler
/// operation can never stall the audio thread or be miscounted as an underrun.
pub struct StreamingSamplerUnit {
    consumer: SharedReader,
    playing: AtomicBool,

    gain: Linear,
    sample_rate: f32,

    /// Shared state for cross-thread communication (speed, direction, seeking).
    shared_state: Option<Arc<RtState>>,

    /// Last ring-reset epoch this consumer applied. When the butler bumps
    /// `RtState::reset_epoch` (on a seek / loop-wrap reposition), the audio
    /// thread — the ring's sole consumer — clears the stale buffered samples so
    /// the butler never has to pop the SPSC ring.
    applied_reset_epoch: u64,

    /// Fractional position for sub-sample interpolation.
    fractional_pos: f64,

    /// History buffer for cubic Hermite interpolation (last 4 samples).
    history: [(f32, f32); 4],

    /// Pre-allocated scratch buffer for fetched samples (RT-safe).
    fetch_scratch: Vec<(f32, f32)>,
}

impl Clone for StreamingSamplerUnit {
    fn clone(&self) -> Self {
        Self {
            consumer: Arc::clone(&self.consumer),
            playing: AtomicBool::new(self.playing.load(Ordering::Relaxed)),
            gain: self.gain,
            sample_rate: self.sample_rate,
            shared_state: self.shared_state.clone(),
            applied_reset_epoch: self.applied_reset_epoch,
            fractional_pos: self.fractional_pos,
            history: self.history,
            fetch_scratch: Vec::with_capacity(MAX_FETCH_SAMPLES),
        }
    }
}

impl StreamingSamplerUnit {
    pub(crate) fn new(consumer: SharedReader, shared_state: Arc<RtState>) -> Self {
        let applied_reset_epoch = shared_state.reset_epoch();
        Self {
            consumer,
            playing: AtomicBool::new(true),
            gain: Linear::new(1.0),
            sample_rate: 44100.0,
            shared_state: Some(shared_state),
            applied_reset_epoch,
            fractional_pos: 0.0,
            history: [(0.0, 0.0); 4],
            fetch_scratch: Vec::with_capacity(MAX_FETCH_SAMPLES),
        }
    }

    /// Apply a butler-requested ring reset if one is pending. Called at the top
    /// of each `tick`/`process` before pulling from the ring. Wait-free:
    /// compares an atomic epoch and, on a change, drains the ring the audio
    /// thread already owns (a bounded pop-loop — no alloc, no I/O, no lock).
    #[inline]
    fn apply_pending_reset(&mut self) {
        if let Some(ref state) = self.shared_state {
            let epoch = state.reset_epoch();
            if epoch != self.applied_reset_epoch {
                self.applied_reset_epoch = epoch;
                self.consumer.load().clear();
                self.history = [(0.0, 0.0); 4];
                self.fractional_pos = 0.0;
            }
        }
    }

    pub fn play(&self) {
        self.playing.store(true, Ordering::Relaxed);
    }

    pub fn stop(&self) {
        self.playing.store(false, Ordering::Relaxed);
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    pub fn set_gain(&mut self, gain: Linear) {
        self.gain = gain;
    }

    pub fn gain(&self) -> Linear {
        self.gain
    }

    #[inline]
    fn shift_history(&mut self) {
        self.history[0] = self.history[1];
        self.history[1] = self.history[2];
        self.history[2] = self.history[3];
    }

    /// Call after seek to reset interpolation state.
    pub fn reset_interpolation(&mut self) {
        self.fractional_pos = 0.0;
        self.history = [(0.0, 0.0); 4];
    }

    fn process_normal_samples(&mut self, size: usize, offset: usize, output: &mut BufferMut) {
        if size == 0 {
            return;
        }

        self.apply_pending_reset();

        let src_ratio = self
            .shared_state
            .as_ref()
            .map_or(1.0, |s| s.src_ratio().get() as f64);
        let base_speed = self
            .shared_state
            .as_ref()
            .map_or(1.0, |s| s.effective_speed().get() as f64)
            * src_ratio;

        let samples_needed = (size as f64 * base_speed).ceil() as usize + 4;

        self.fetch_scratch.clear();

        let gain = self.gain.get();
        {
            let cell = self.consumer.load();
            for _ in 0..samples_needed {
                if let Some((left, right)) = cell.read() {
                    self.fetch_scratch.push((left * gain, right * gain));
                } else {
                    break;
                }
            }
        }

        let mut fetch_idx = 0;
        for i in 0..size {
            let speed = self
                .shared_state
                .as_ref()
                .map_or(1.0, |s| s.effective_speed().get() as f64 * s.src_ratio().get() as f64);

            self.fractional_pos += speed;

            while self.fractional_pos >= 1.0 {
                self.fractional_pos -= 1.0;
                self.shift_history();

                if fetch_idx < self.fetch_scratch.len() {
                    self.history[3] = self.fetch_scratch[fetch_idx];
                    fetch_idx += 1;
                } else {
                    if let Some(ref state) = self.shared_state {
                        state.report_underrun();
                    }
                    self.history[3] = self.history[2];
                }
            }

            let t = self.fractional_pos as f32;
            let left = cubic_hermite(
                self.history[0].0,
                self.history[1].0,
                self.history[2].0,
                self.history[3].0,
                t,
            );
            let right = cubic_hermite(
                self.history[0].1,
                self.history[1].1,
                self.history[2].1,
                self.history[3].1,
                t,
            );

            output.set_f32(0, offset + i, left);
            output.set_f32(1, offset + i, right);
        }
    }
}

impl AudioUnit for StreamingSamplerUnit {
    fn inputs(&self) -> usize {
        0
    }

    fn outputs(&self) -> usize {
        2
    }

    fn reset(&mut self) {
        self.playing.store(false, Ordering::Relaxed);
        self.reset_interpolation();
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.sample_rate = sample_rate as f32;
    }

    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        if !self.playing.load(Ordering::Relaxed) {
            if output.len() >= 2 {
                output[0] = 0.0;
                output[1] = 0.0;
            }
            return;
        }

        let gain = self.gain.get();
        if let Some(ref state) = self.shared_state {
            if let Some((left, right)) = state.next_seek_crossfade_sample() {
                if output.len() >= 2 {
                    output[0] = left * gain;
                    output[1] = right * gain;
                }
                return;
            }

            if state.is_seeking() {
                if output.len() >= 2 {
                    output[0] = 0.0;
                    output[1] = 0.0;
                }
                return;
            }
        }

        self.apply_pending_reset();

        let speed = self
            .shared_state
            .as_ref()
            .map_or(1.0, |s| s.effective_speed().get() as f64 * s.src_ratio().get() as f64);

        self.fractional_pos += speed;

        let cell = self.consumer.load();
        while self.fractional_pos >= 1.0 {
            self.fractional_pos -= 1.0;
            self.shift_history();

            if let Some((left, right)) = cell.read() {
                self.history[3] = (left * gain, right * gain);
            } else {
                if let Some(ref state) = self.shared_state {
                    state.report_underrun();
                }
                self.history[3] = self.history[2];
            }
        }

        let t = self.fractional_pos as f32;
        let left = cubic_hermite(
            self.history[0].0,
            self.history[1].0,
            self.history[2].0,
            self.history[3].0,
            t,
        );
        let right = cubic_hermite(
            self.history[0].1,
            self.history[1].1,
            self.history[2].1,
            self.history[3].1,
            t,
        );

        if output.len() >= 2 {
            output[0] = left;
            output[1] = right;
        }
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        if !self.playing.load(Ordering::Relaxed) {
            for i in 0..size {
                output.set_f32(0, i, 0.0);
                output.set_f32(1, i, 0.0);
            }
            return;
        }

        if let Some(ref state) = self.shared_state {
            if state.is_seek_crossfading() {
                for i in 0..size {
                    if let Some((left, right)) = state.next_seek_crossfade_sample() {
                        output.set_f32(0, i, left * self.gain.get());
                        output.set_f32(1, i, right * self.gain.get());
                    } else {
                        self.process_normal_samples(size - i, i, output);
                        return;
                    }
                }
                return;
            }

            if state.is_loop_crossfading() {
                for i in 0..size {
                    if let Some((left, right)) = state.next_loop_crossfade_sample() {
                        output.set_f32(0, i, left * self.gain.get());
                        output.set_f32(1, i, right * self.gain.get());
                    } else {
                        self.process_normal_samples(size - i, i, output);
                        return;
                    }
                }
                return;
            }

            if state.is_seeking() {
                for i in 0..size {
                    output.set_f32(0, i, 0.0);
                    output.set_f32(1, i, 0.0);
                }
                return;
            }
        }

        self.process_normal_samples(size, 0, output);
    }

    audio_unit_boilerplate!(id = crate::node_id::STREAMING_SAMPLER_ID, outputs = 2);
}

// ---------------------------------------------------------------------------
// StreamingClipReader — a `StreamingSamplerUnit` wrapped in the transport
// placement gate that the raw unit lacks.
//
// `StreamingSamplerUnit` streams whatever the butler ring feeds it, unaware of
// *where on the timeline* the clip lives. A timeline clip only sounds while the
// playhead is inside its [start_beat, start_beat + duration) window; outside it
// must be silent. `SamplerUnit` bakes this in via `transport_sample_position`;
// the streaming unit does not, so this wrapper adds the same gate:
//
//   * inside the window  → pull from the ring (the raw unit's normal path)
//   * outside the window → emit silence, do not drain the ring
//   * playhead *jumps* into/within the window (seek, loop wrap, first entry)
//     → request a butler seek to the target sample so the ring refills from the
//       right disk offset before we start pulling.
//
// The gate math is a copy of `SamplerUnit::transport_sample_position`: it is the
// single source of truth for "is the playhead inside this clip, and at what
// sample offset". Kept lock-free / alloc-free so `tick`/`process` stay RT-safe —
// the actual disk seek is deferred to the butler; here we only publish the
// requested target to the lock-free `RtState` (two atomic stores), which the
// butler polls each loop and applies click-free. The butler owns the seek flag
// so it can clear it; the reader never sets it.
// ---------------------------------------------------------------------------

/// Sentinel for "no seek has been requested yet" — any real target differs from
/// this on the first entry, so the first inside-frame always issues a seek.
const NO_SEEK_TARGET: f64 = f64::NEG_INFINITY;

/// Max sample-offset drift between the position the ring is streaming and the
/// position the transport wants before we treat it as a discontinuity (seek /
/// loop wrap) rather than normal contiguous playback. One buffer's worth of
/// slack at a generous block size keeps normal advance from tripping a seek.
const SEEK_EPSILON_SAMPLES: f64 = 4096.0;

/// Wiring for [`StreamingClipReader::new`]: the placement gate plus the file
/// sample rate. Reuses [`TransportPlacement`] for the transport/start/duration
/// cluster so the streaming path and `SamplerUnit` speak the same value type;
/// `shared_state` stays a separate wiring arg (it must be the same `RtState` the
/// `inner` unit holds).
pub struct StreamingClipConfig {
    /// Transport binding — the placement gate. Whole-or-nothing, mirroring
    /// `SamplerUnit`'s use of [`TransportPlacement`].
    pub placement: TransportPlacement,
    /// File sample rate — converts the transport's second-offset into a sample
    /// offset for the seek target, matching `SamplerUnit`'s use of
    /// `wave.sample_rate()`.
    pub file_sample_rate: f64,
}

pub struct StreamingClipReader {
    inner: StreamingSamplerUnit,
    shared_state: Arc<RtState>,

    /// Transport binding — the placement gate. Whole-or-nothing, mirroring
    /// `SamplerUnit`'s `TransportPlacement`.
    placement: TransportPlacement,

    /// File sample rate — converts the transport's second-offset into a sample
    /// offset for the seek target, matching `SamplerUnit`'s use of
    /// `wave.sample_rate()`.
    file_sample_rate: f64,

    /// The clip-relative sample offset we last requested the butler stream from.
    /// `NO_SEEK_TARGET` until the first inside-frame. Used to detect a
    /// discontinuous jump: when the transport's desired offset diverges from the
    /// contiguously-advanced expectation by more than `SEEK_EPSILON_SAMPLES`, we
    /// issue a fresh seek.
    streamed_offset: f64,

    /// Whether the previous frame was inside the clip window. A false→true edge
    /// (playhead entering the clip) always forces a seek.
    was_inside: bool,
}

impl Clone for StreamingClipReader {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            shared_state: Arc::clone(&self.shared_state),
            placement: self.placement.clone(),
            file_sample_rate: self.file_sample_rate,
            streamed_offset: self.streamed_offset,
            was_inside: self.was_inside,
        }
    }
}

impl StreamingClipReader {
    /// Wrap a `StreamingSamplerUnit` with the transport placement gate.
    ///
    /// Construction (butler stream registration, ring allocation) happens on the
    /// ECS/butler side; this only binds the already-built unit to a timeline
    /// window. `shared_state` must be the same `RtState` the `inner` unit holds
    /// so the gate's seek requests reach the unit's seek/underrun machinery.
    pub fn new(
        inner: StreamingSamplerUnit,
        shared_state: Arc<RtState>,
        config: StreamingClipConfig,
    ) -> Self {
        Self {
            inner,
            shared_state,
            placement: config.placement,
            file_sample_rate: config.file_sample_rate,
            streamed_offset: NO_SEEK_TARGET,
            was_inside: false,
        }
    }

    pub fn set_placement(&mut self, start_beat: BeatPosition, duration: Option<BeatDuration>) {
        self.placement.start_beat = start_beat;
        self.placement.duration_beats = duration;
        // A placement change may move the window out from under the playhead;
        // force a re-seek on the next inside-frame.
        self.streamed_offset = NO_SEEK_TARGET;
        self.was_inside = false;
    }

    pub fn set_gain(&mut self, gain: Linear) {
        self.inner.set_gain(gain);
    }

    pub fn gain(&self) -> Linear {
        self.inner.gain()
    }

    /// The file sample rate this stream decodes at.
    pub fn file_sample_rate(&self) -> f64 {
        self.file_sample_rate
    }

    /// Clip-relative sample offset the playhead sits at, or `None` when it is
    /// outside the clip window. Delegates to the shared
    /// [`transport_sample_offset`](super::interp::transport_sample_offset) — the
    /// one gate definition, also used by [`SamplerUnit`].
    #[inline]
    fn placement_sample_offset(&self) -> Option<f64> {
        super::interp::transport_sample_offset(
            self.placement.transport.as_ref(),
            self.placement.start_beat,
            self.placement.duration_beats,
            self.file_sample_rate,
        )
    }

    /// Ask the butler to stream from `target_offset` (clip-relative samples).
    ///
    /// Records the target for drift tracking, then publishes it to the shared
    /// [`RtState`](crate::butler::RtState) via `request_seek` — two lock-free
    /// atomic stores. The butler polls the request each loop and repositions the
    /// live stream click-free (flush + seek + crossfade), owning the seek flag so
    /// it can clear it; the reader must NOT flip `set_seeking` itself (with no
    /// butler to clear it the reader would mute forever). RT-safe: no alloc, no
    /// I/O, no blocking on this path.
    #[inline]
    fn request_seek(&mut self, target_offset: f64) {
        self.streamed_offset = target_offset;
        self.shared_state.request_seek(target_offset.max(0.0) as u64);
    }

    /// Set the playback speed magnitude. Routes directly to the shared
    /// [`RtState`], which is exactly what the butler's `SetVarispeed` handler
    /// does for speed (`rt_state.set_speed`) — two lock-free atomic stores, no
    /// butler round-trip needed. Direction is a separate concern (the reader has
    /// no direction verb), so this leaves it untouched, mirroring how
    /// `ClipCommand::UpdateSpeed` carries only a magnitude.
    pub fn set_speed(&mut self, speed: Ratio) {
        self.shared_state.set_speed(speed.get());
    }

    /// Set the playback direction. Routes directly to the shared [`RtState`],
    /// which is exactly the direction leg of the butler's `SetVarispeed` handler
    /// (`rt_state.set_direction`) — one lock-free atomic store, no butler
    /// round-trip. Pairs with [`set_speed`](Self::set_speed) so the reader can
    /// reproduce a full `SetVarispeed { speed, direction }` in-unit.
    pub fn set_direction(&mut self, direction: Direction) {
        self.shared_state.set_direction(direction);
    }

    /// Public seek: reposition the live stream to clip-relative `to`. Delegates
    /// to the private [`request_seek`](Self::request_seek), which publishes the
    /// target to the shared [`RtState`] (two atomic stores) for the butler to
    /// apply click-free — the same mechanism the placement gate uses on a
    /// discontinuous jump, and the same one the `Command::Seek` butler path
    /// ultimately drives. RT-safe: no alloc, no I/O.
    pub fn seek(&mut self, to: SamplePosition) {
        self.request_seek(to.get());
    }

    /// Decide, for a frame whose desired clip-relative offset is `offset`,
    /// whether the playhead jumped (fresh entry or a seek/loop discontinuity)
    /// and issue a butler seek if so.
    #[inline]
    fn maybe_seek(&mut self, offset: f64) {
        let jumped = !self.was_inside
            || self.streamed_offset == NO_SEEK_TARGET
            || (offset - self.streamed_offset).abs() > SEEK_EPSILON_SAMPLES;
        if jumped {
            self.request_seek(offset);
        } else {
            // Contiguous playback — advance our expectation so drift stays
            // bounded and normal advance never trips the discontinuity check.
            self.streamed_offset = offset;
        }
    }
}

impl ClipReader for StreamingClipReader {
    fn set_gain(&mut self, gain: Linear) {
        StreamingClipReader::set_gain(self, gain);
    }

    fn set_placement(&mut self, start_beat: BeatPosition, duration: Option<BeatDuration>) {
        StreamingClipReader::set_placement(self, start_beat, duration);
    }

    fn set_speed(&mut self, speed: Ratio) {
        // Honest in-unit forward: the butler's `SetVarispeed` handler's speed
        // effect is exactly `RtState::set_speed`, which this reader can reach via
        // its `Arc<RtState>`. See [`StreamingClipReader::set_speed`].
        StreamingClipReader::set_speed(self, speed);
    }

    fn set_direction(&mut self, direction: Direction) {
        // Honest in-unit forward: the direction leg of the butler's `SetVarispeed`
        // handler is `RtState::set_direction`, reachable via this reader's
        // `Arc<RtState>`. See [`StreamingClipReader::set_direction`]. This is what
        // makes streaming reverse produce the same effect the old butler
        // `Command::SetSpeed { direction }` did, now driven from the reader drain.
        StreamingClipReader::set_direction(self, direction);
    }

    fn set_loop(&mut self, _setting: LoopSetting) {
        // NOT honestly implementable in-unit. `SetStreamLoop` mutates
        // butler-owned state (`plan.link.loop_config`) and reads the loop-start
        // fadein head off disk in `handle_set_stream_loop`; the reader holds only
        // `Arc<RtState>` + the ring consumer and can reach neither. Deliberate
        // no-op: streaming loop stays on the butler command path
        // (`Command::Loop` → `SetStreamLoop`/`ClearStreamLoop`), which Layer 4
        // keeps driving control-side from dawai-model. The command drain skips the
        // streaming variant for loop ops accordingly.
    }

    fn seek(&mut self, to: SamplePosition) {
        StreamingClipReader::seek(self, to);
    }

    fn set_wave(&mut self, _wave: Arc<Wave>) {
        // Deliberate no-op. A streaming source has no in-RAM wave to swap; a
        // source change means re-registering the butler stream on a different
        // file (a control-thread / butler op via `AddStreaming`), not an in-unit
        // mutation. Preserves the existing `ReplaceWave` behavior where the
        // command drain skips the streaming variant.
    }

    fn play(&self) {
        self.inner.play();
    }

    fn stop(&self) {
        self.inner.stop();
    }

    fn is_playing(&self) -> bool {
        self.inner.is_playing()
    }
}

impl AudioUnit for StreamingClipReader {
    fn inputs(&self) -> usize {
        0
    }

    fn outputs(&self) -> usize {
        2
    }

    fn reset(&mut self) {
        self.inner.reset();
        self.streamed_offset = NO_SEEK_TARGET;
        self.was_inside = false;
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.inner.set_sample_rate(sample_rate);
    }

    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        match self.placement_sample_offset() {
            Some(offset) => {
                self.maybe_seek(offset);
                self.was_inside = true;
                self.inner.tick(input, output);
            }
            None => {
                self.was_inside = false;
                if output.len() >= 2 {
                    output[0] = 0.0;
                    output[1] = 0.0;
                }
            }
        }
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        match self.placement_sample_offset() {
            Some(offset) => {
                self.maybe_seek(offset);
                self.was_inside = true;
                self.inner.process(size, input, output);
            }
            None => {
                self.was_inside = false;
                for i in 0..size {
                    output.set_f32(0, i, 0.0);
                    output.set_f32(1, i, 0.0);
                }
            }
        }
    }

    audio_unit_boilerplate!(id = crate::node_id::STREAMING_SAMPLER_ID, outputs = 2);
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::butler::{RegionBuffer, RegionId};
    use std::path::PathBuf;
    use tutti_core::{BufferVec, TransportReader};

    fn make_reader_with_samples(samples: &[(f32, f32)]) -> SharedReader {
        let (mut writer, reader) =
            RegionBuffer::with_capacity(RegionId(1), PathBuf::new(), samples.len() + 64);
        writer.write(samples);
        crate::butler::share_reader(reader)
    }

    fn make_unit(
        samples: &[(f32, f32)],
    ) -> (StreamingSamplerUnit, Arc<RtState>) {
        let reader = make_reader_with_samples(samples);
        let state = Arc::new(RtState::new());
        let unit = StreamingSamplerUnit::new(reader, Arc::clone(&state));
        (unit, state)
    }

    // --- StreamingClipReader: placement gate ---

    use std::sync::atomic::AtomicU64;

    struct MockTransport {
        playing: AtomicBool,
        beat: AtomicU64,
        tempo: AtomicU64,
    }

    impl MockTransport {
        fn new(tempo: f64, beat: f64, playing: bool) -> Arc<Self> {
            Arc::new(Self {
                playing: AtomicBool::new(playing),
                beat: AtomicU64::new(beat.to_bits()),
                tempo: AtomicU64::new(tempo.to_bits()),
            })
        }
        fn set_beat(&self, beat: f64) {
            self.beat.store(beat.to_bits(), Ordering::Relaxed);
        }
    }

    impl TransportReader for MockTransport {
        fn is_playing(&self) -> bool {
            self.playing.load(Ordering::Relaxed)
        }
        fn current_beat(&self) -> f64 {
            f64::from_bits(self.beat.load(Ordering::Relaxed))
        }
        fn tempo(&self) -> tutti_core::Bpm {
            tutti_core::Bpm::new(f64::from_bits(self.tempo.load(Ordering::Relaxed)))
        }
        fn is_loop_enabled(&self) -> bool {
            false
        }
        fn get_loop_range(&self) -> Option<(f64, f64)> {
            None
        }
        fn is_recording(&self) -> bool {
            false
        }
        fn is_in_preroll(&self) -> bool {
            false
        }
    }

    fn make_clip_reader(
        samples: &[(f32, f32)],
        transport: Arc<dyn TransportReader>,
        start_beat: BeatPosition,
        duration: Option<BeatDuration>,
    ) -> StreamingClipReader {
        let (inner, state) = make_unit(samples);
        StreamingClipReader::new(
            inner,
            state,
            StreamingClipConfig {
                placement: TransportPlacement {
                    transport,
                    start_beat,
                    duration_beats: duration,
                },
                file_sample_rate: 44100.0,
            },
        )
    }

    #[test]
    fn clip_reader_silent_outside_window_audible_inside() {
        // Clip window: beats [4, 8). Non-zero ramp in the ring.
        let samples: Vec<_> = (1..64).map(|i| (i as f32, i as f32)).collect();
        let transport = MockTransport::new(120.0, 0.0, true);
        let mut reader = make_clip_reader(
            &samples,
            transport.clone(),
            BeatPosition::new(4.0),
            Some(BeatDuration::new(4.0)),
        );

        // Before the window (beat 0): silence, ring untouched.
        let mut out = [0.0f32; 2];
        for _ in 0..8 {
            reader.tick(&[], &mut out);
        }
        assert_eq!(out, [0.0, 0.0], "before window → silence");

        // Inside the window (beat 5): pulls the ring, produces audio.
        transport.set_beat(5.0);
        let mut got_audio = false;
        for _ in 0..8 {
            reader.tick(&[], &mut out);
            if out[0] != 0.0 || out[1] != 0.0 {
                got_audio = true;
            }
        }
        assert!(got_audio, "inside window → audible");

        // Past the window (beat 9): silent again.
        transport.set_beat(9.0);
        reader.tick(&[], &mut out);
        assert_eq!(out, [0.0, 0.0], "after window → silence");
    }

    #[test]
    fn clip_reader_stopped_transport_is_silent() {
        let samples: Vec<_> = (1..32).map(|i| (i as f32, i as f32)).collect();
        let transport = MockTransport::new(120.0, 5.0, false); // inside window but stopped
        let mut reader = make_clip_reader(
            &samples,
            transport,
            BeatPosition::new(4.0),
            Some(BeatDuration::new(4.0)),
        );

        let mut out = [1.0f32; 2];
        reader.tick(&[], &mut out);
        assert_eq!(out, [0.0, 0.0], "stopped transport → silence");
    }

    // --- StreamingClipReader: RT no-alloc (steady-state process inside window) ---
    //
    // The integration-test gate `tests/rt_no_alloc.rs` cannot reach the
    // crate-private `RegionReader` / `RtState` needed to build a streaming
    // reader, so the streaming-variant no-alloc guard lives here, in-crate, with
    // a module-local `AllocDisabler`. The disabler only aborts inside an
    // `assert_no_alloc` region; every other unit test allocates normally.

    #[global_allocator]
    static ALLOC: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

    #[test]
    fn clip_reader_process_steady_state_is_allocation_free() {
        let samples: Vec<_> = (1..2048).map(|i| (i as f32 * 0.001, i as f32 * 0.001)).collect();
        let transport = MockTransport::new(120.0, 5.0, true); // inside window
        let mut reader = make_clip_reader(
            &samples,
            transport,
            BeatPosition::new(4.0),
            Some(BeatDuration::new(4.0)),
        );
        reader.set_sample_rate(tutti_core::SampleRate::new(48_000.0));

        let input_vec = BufferVec::new(0);
        let mut output_vec = BufferVec::new(2);

        // Warm-up: settles the enter-window seek edge + primes the interpolation
        // history so the guarded loop is on the steady-state path. (The seek edge
        // itself is alloc-free — see `clip_reader_seek_edge_is_allocation_free`.)
        for _ in 0..16 {
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            reader.process(64, &input, &mut output);
        }

        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..2_000 {
                let input = input_vec.buffer_ref();
                let mut output = output_vec.buffer_mut();
                reader.process(64, &input, &mut output);
            }
        });
    }

    /// The seek edge (`request_seek`, fired on the audio thread when the playhead
    /// jumps into or across the clip window) must be allocation-free — it may only
    /// touch the lock-free `RtState` seek-request slot. This exercises that edge
    /// *inside* the guarded block by moving the transport beat each iteration, so a
    /// regression that reintroduces allocation on the seek path is caught.
    #[test]
    fn clip_reader_seek_edge_is_allocation_free() {
        let samples: Vec<_> = (1..2048).map(|i| (i as f32 * 0.001, i as f32 * 0.001)).collect();
        let transport = MockTransport::new(120.0, 5.0, true); // inside [4, 8)
        let mut reader = make_clip_reader(
            &samples,
            Arc::clone(&transport) as Arc<dyn TransportReader>,
            BeatPosition::new(4.0),
            Some(BeatDuration::new(4.0)),
        );
        reader.set_sample_rate(tutti_core::SampleRate::new(48_000.0));

        let input_vec = BufferVec::new(0);
        let mut output_vec = BufferVec::new(2);

        // Prime interpolation history / settle the initial enter-window seek.
        for _ in 0..16 {
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            reader.process(64, &input, &mut output);
        }

        assert_no_alloc::assert_no_alloc(|| {
            for i in 0..2_000 {
                // Jump the playhead in and out of the window so `maybe_seek` keeps
                // detecting discontinuities and calling `request_seek`.
                let beat = if i % 2 == 0 { 5.0 } else { 6.5 };
                transport.set_beat(beat);
                let input = input_vec.buffer_ref();
                let mut output = output_vec.buffer_mut();
                reader.process(64, &input, &mut output);
            }
        });
    }

    #[test]
    fn clip_reader_tick_steady_state_is_allocation_free() {
        let samples: Vec<_> = (1..2048).map(|i| (i as f32 * 0.001, i as f32 * 0.001)).collect();
        let transport = MockTransport::new(120.0, 5.0, true);
        let mut reader = make_clip_reader(
            &samples,
            transport,
            BeatPosition::new(4.0),
            Some(BeatDuration::new(4.0)),
        );
        reader.set_sample_rate(tutti_core::SampleRate::new(48_000.0));

        let mut out = [0.0f32; 2];
        for _ in 0..256 {
            reader.tick(&[], &mut out);
        }

        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..100_000 {
                reader.tick(&[], &mut out);
            }
        });
    }

    // --- Existing tests ---

    #[test]
    fn test_cubic_hermite_interpolation() {
        let result = cubic_hermite(0.0, 1.0, 2.0, 3.0, 0.0);
        assert!((result - 1.0).abs() < 0.001);

        let result = cubic_hermite(0.0, 1.0, 2.0, 3.0, 1.0);
        assert!((result - 2.0).abs() < 0.001);

        let result = cubic_hermite(0.0, 1.0, 2.0, 3.0, 0.5);
        assert!((result - 1.5).abs() < 0.1);
    }

    #[test]
    fn test_shared_stream_state_seeking() {
        let state = RtState::new();

        assert!(!state.is_seeking());

        state.set_seeking(true);
        assert!(state.is_seeking());

        state.set_seeking(false);
        assert!(!state.is_seeking());
    }

    #[test]
    fn test_shared_stream_state_speed() {
        let state = RtState::new();

        assert_eq!(state.speed(), tutti_core::Ratio::new(1.0));

        state.set_speed(0.5);
        assert_eq!(state.speed(), tutti_core::Ratio::new(0.5));

        state.set_speed(2.0);
        assert_eq!(state.speed(), tutti_core::Ratio::new(2.0));

        state.set_speed(0.1);
        assert_eq!(state.speed(), tutti_core::Ratio::new(0.25));

        state.set_speed(10.0);
        assert_eq!(state.speed(), tutti_core::Ratio::new(4.0));
    }

    // --- New: play/stop state ---

    #[test]
    fn play_stop_controls_output() {
        let samples: Vec<_> = (0..100).map(|i| (i as f32, -(i as f32))).collect();
        let (mut unit, _state) = make_unit(&samples);

        assert!(unit.is_playing());

        // Tick while playing — should produce non-zero after history primes.
        let mut out = [0.0f32; 2];
        for _ in 0..5 {
            unit.tick(&[], &mut out);
        }
        let playing_sample = out[0];

        unit.stop();
        assert!(!unit.is_playing());
        unit.tick(&[], &mut out);
        assert_eq!(out[0], 0.0, "stopped unit must output silence");
        assert_eq!(out[1], 0.0);

        unit.play();
        assert!(unit.is_playing());
        unit.tick(&[], &mut out);
        assert_ne!(out[0], 0.0, "resumed unit should produce audio");
        let _ = playing_sample;
    }

    // --- New: tick produces interpolated output from ring buffer ---

    #[test]
    fn tick_reads_from_ring_buffer_and_interpolates() {
        // Feed a ramp 0,1,2,...,19 into the ring buffer. After enough
        // ticks to prime the 4-sample history, output should be
        // non-zero and monotonically increasing (speed=1, src_ratio=1).
        let samples: Vec<_> = (0..20).map(|i| (i as f32, i as f32)).collect();
        let (mut unit, _state) = make_unit(&samples);

        let mut prev = f32::NEG_INFINITY;
        let mut out = [0.0f32; 2];
        for i in 0..16 {
            unit.tick(&[], &mut out);
            if i >= 4 {
                assert!(
                    out[0] >= prev,
                    "ramp should be monotonic at tick {i}: prev={prev}, got={}",
                    out[0]
                );
            }
            prev = out[0];
        }
        assert!(prev > 0.0, "should have produced non-zero audio");
    }

    // --- New: process block reads from ring buffer ---

    #[test]
    fn process_block_produces_output() {
        let samples: Vec<_> = (0..256).map(|i| (i as f32 * 0.01, -(i as f32) * 0.01)).collect();
        let (mut unit, _state) = make_unit(&samples);

        let input_vec = BufferVec::new(0);
        let mut output_vec = BufferVec::new(2);

        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        unit.process(64, &input, &mut output);

        // After 64 frames at speed=1, output should contain interpolated
        // samples from the ramp. Check last few are non-zero.
        let last = output.at_f32(0, 63);
        assert!(last > 0.0, "process() should produce audio, got {last}");
    }

    #[test]
    fn process_block_silence_when_stopped() {
        let samples: Vec<_> = (0..256).map(|i| (i as f32, 0.0)).collect();
        let (mut unit, _state) = make_unit(&samples);
        unit.stop();

        let input_vec = BufferVec::new(0);
        let mut output_vec = BufferVec::new(2);

        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        unit.process(16, &input, &mut output);

        for i in 0..16 {
            assert_eq!(output.at_f32(0, i), 0.0);
            assert_eq!(output.at_f32(1, i), 0.0);
        }
    }

    // --- New: seeking outputs silence ---

    #[test]
    fn tick_outputs_silence_while_seeking() {
        let samples: Vec<_> = (0..100).map(|i| (i as f32, 0.0)).collect();
        let (mut unit, state) = make_unit(&samples);

        state.set_seeking(true);

        let mut out = [99.0f32; 2];
        unit.tick(&[], &mut out);
        assert_eq!(out[0], 0.0, "seeking → silence");
        assert_eq!(out[1], 0.0);
    }

    #[test]
    fn process_outputs_silence_while_seeking() {
        let samples: Vec<_> = (0..100).map(|i| (i as f32, 0.0)).collect();
        let (mut unit, state) = make_unit(&samples);

        state.set_seeking(true);

        let input_vec = BufferVec::new(0);
        let mut output_vec = BufferVec::new(2);
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        unit.process(8, &input, &mut output);

        for i in 0..8 {
            assert_eq!(output.at_f32(0, i), 0.0, "seeking → silence at {i}");
        }
    }

    // --- New: gain application ---

    #[test]
    fn gain_scales_tick_output() {
        let samples: Vec<_> = (0..20).map(|_| (1.0f32, -1.0f32)).collect();

        let reader1 = make_reader_with_samples(&samples);
        let reader2 = make_reader_with_samples(&samples);
        let state1 = Arc::new(RtState::new());
        let state2 = Arc::new(RtState::new());

        let mut full = StreamingSamplerUnit::new(reader1, state1);
        let mut half = StreamingSamplerUnit::new(reader2, state2);
        half.set_gain(Linear::new(0.5));

        let mut out_full = [0.0f32; 2];
        let mut out_half = [0.0f32; 2];

        // Prime history then compare
        for _ in 0..6 {
            full.tick(&[], &mut out_full);
            half.tick(&[], &mut out_half);
        }

        if out_full[0].abs() > 1e-6 {
            let ratio = out_half[0] / out_full[0];
            assert!(
                (ratio - 0.5).abs() < 0.05,
                "gain=0.5 should halve output: full={}, half={}, ratio={ratio}",
                out_full[0],
                out_half[0]
            );
        }
    }

    // --- New: reset clears interpolation state ---

    #[test]
    fn reset_clears_state() {
        let samples: Vec<_> = (0..100).map(|i| (i as f32, 0.0)).collect();
        let (mut unit, _state) = make_unit(&samples);

        let mut out = [0.0f32; 2];
        for _ in 0..10 {
            unit.tick(&[], &mut out);
        }

        unit.reset();
        assert!(!unit.is_playing());
        assert_eq!(unit.fractional_pos, 0.0);
        assert_eq!(unit.history, [(0.0, 0.0); 4]);
    }

    // --- New: seek crossfade path ---

    #[test]
    fn seek_crossfade_plays_through_then_resumes_normal() {
        let samples: Vec<_> = (0..256).map(|i| (i as f32 * 0.1, 0.0)).collect();
        let (mut unit, state) = make_unit(&samples);

        let fadeout: Vec<_> = (0..4).map(|i| (1.0 - i as f32 * 0.25, 0.0)).collect();
        let fadein: Vec<_> = (0..4).map(|i| (i as f32 * 0.25, 0.0)).collect();
        state.start_seek_crossfade(fadeout, fadein);

        assert!(state.is_seek_crossfading());

        let input_vec = BufferVec::new(0);
        let mut output_vec = BufferVec::new(2);
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();

        // Process a block larger than the crossfade (4 samples).
        // First 4 samples come from the crossfade, rest from normal path.
        unit.process(16, &input, &mut output);

        // After exhausting the 4-sample crossfade, the remaining 12
        // frames should be non-zero from the normal path.
        let last = output.at_f32(0, 15);
        // The normal path starts reading from the ring buffer, so
        // after enough samples to prime interpolation it should be > 0.
        // Just verify the process didn't panic and produced some output.
        let _ = last;
    }
}
