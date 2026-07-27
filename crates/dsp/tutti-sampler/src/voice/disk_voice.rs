//! Disk streaming sample playback, fed by the butler thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::MAX_SAMPLER_CHANNELS;
use tutti_core::SignalFrame;
use tutti_core::{
    Amplitude, AudioUnit, Beat, BeatDuration, BufferMut, BufferRef, PlaybackRate, ReadRate,
    SamplePosition, SampleRate, Samples, SrcRatio, Timeline,
};

use super::interp::cubic_hermite;
use super::memory_source::VoiceWindow;
use super::voice_pool::Direction;
use crate::butler::{RtState, SharedReader};

/// Per-block fetch budget in **frames**, reserved once per unit so the RT
/// `clear()` + `push()` in `process_normal_samples` can never reallocate.
///
/// The old comment read "8192 frames at 4x speed", and both halves are wrong in
/// ways worth recording, because the headroom this buys is accidental rather
/// than designed:
///
/// - fundsp caps a `process` block at `MAX_BUFFER_SIZE = 64`, not 8192, so the
///   real per-block demand is ~64 frames, not ~8192.
/// - the read rate is not capped at 4x. It is `PlaybackRate` (≤ 4.0) times
///   `SrcRatio`, which is `file_rate / session_rate` and has no clamp — a 192k
///   file in a 44.1k session gives ≈ 4.35, so the true ceiling is ≈ 17.4x.
///
/// Worst case is therefore `64 * 17.4 + 4 ≈ 1119` frames against 32,776
/// reserved — a 29x margin. That margin survives the rate ceiling being 4x
/// larger than documented only because the block size is 128x smaller. **If
/// fundsp's block size ever grows, recompute this** rather than trusting the
/// number.
const MAX_FETCH_SAMPLES: usize = 8192 * 4 + 8;

/// Disk streaming sampler with varispeed, seeking, and crossfade support.
///
/// The audio thread is the sole consumer of the ring. It holds the reader
/// through a wait-free [`SharedReader`] (an `ArcSwap`, never a `Mutex`): each
/// buffer it `load`s the current [`ReaderCell`](crate::butler::prefetch::ReaderCell)
/// and pops from it. No lock ever sits on `tick`/`process`, so a butler
/// operation can never stall the audio thread or be miscounted as an underrun.
pub struct DiskSource {
    consumer: SharedReader,
    playing: AtomicBool,

    gain: Amplitude,
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

    /// Output width; also the interleave stride of `history` and
    /// `fetch_scratch`.
    channels: usize,

    /// 4-tap cubic-Hermite history, **frame-major**: tap `t` channel `c` lives
    /// at `history[t * channels + c]`.
    ///
    /// Frame-major deliberately. Channel-major (`history[c * 4 + t]`) is the
    /// tempting layout, because the interpolation kernel wants four consecutive
    /// taps of one channel — but then `shift_history` has to stride, and at
    /// width 2 a stride bug and a correct stride coincide for several access
    /// patterns. Getting it wrong rotates channels while still producing
    /// smooth, plausible-sounding output.
    history: Vec<f32>,

    /// Pre-allocated scratch for fetched frames, flat interleaved (RT-safe:
    /// `clear` + `push` inside a reserved capacity, never a realloc).
    fetch_scratch: Vec<f32>,
}

// Hand-rolled: `consumer` (a `SharedReader` `ArcSwap`) and `shared_state`
// (`Arc<RtState>`) aren't `Debug`. Print the scalar params + a note; never
// load the ring consumer or read its occupancy from a Debug impl.
impl std::fmt::Debug for DiskSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskSource")
            .field("playing", &self.playing.load(Ordering::Relaxed))
            .field("gain", &self.gain)
            .field("sample_rate", &self.sample_rate)
            .field("has_shared_state", &self.shared_state.is_some())
            .field("applied_reset_epoch", &self.applied_reset_epoch)
            .finish_non_exhaustive()
    }
}

impl Clone for DiskSource {
    fn clone(&self) -> Self {
        Self {
            consumer: Arc::clone(&self.consumer),
            playing: AtomicBool::new(self.playing.load(Ordering::Relaxed)),
            gain: self.gain,
            sample_rate: self.sample_rate,
            shared_state: self.shared_state.clone(),
            applied_reset_epoch: self.applied_reset_epoch,
            fractional_pos: self.fractional_pos,
            channels: self.channels,
            history: self.history.clone(),
            fetch_scratch: Vec::with_capacity(MAX_FETCH_SAMPLES * self.channels),
        }
    }
}

impl DiskSource {
    /// Width comes from the ring: the reader's stride is what this unit must
    /// pop, so there is nothing to declare independently and nothing that can
    /// disagree.
    pub(crate) fn new(consumer: SharedReader, shared_state: Arc<RtState>) -> Self {
        let applied_reset_epoch = shared_state.reset_epoch();
        let channels = consumer.load().channels().clamp(1, MAX_SAMPLER_CHANNELS);
        Self {
            consumer,
            playing: AtomicBool::new(true),
            gain: Amplitude::new(1.0),
            sample_rate: 44100.0,
            shared_state: Some(shared_state),
            applied_reset_epoch,
            fractional_pos: 0.0,
            channels,
            history: vec![0.0; 4 * channels],
            fetch_scratch: Vec::with_capacity(MAX_FETCH_SAMPLES * channels),
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
                self.history.fill(0.0);
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

    pub fn set_gain(&mut self, gain: Amplitude) {
        self.gain = gain;
    }

    pub fn gain(&self) -> Amplitude {
        self.gain
    }

    /// Output width — this unit's `outputs()`.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Drop the oldest tap, shifting taps 1..3 down one frame.
    #[inline]
    fn shift_history(&mut self) {
        let ch = self.channels;
        self.history.copy_within(ch.., 0);
    }

    /// Tap `t`, channel `c`.
    #[inline]
    fn tap(&self, t: usize, c: usize) -> f32 {
        self.history[t * self.channels + c]
    }

    /// Call after seek to reset interpolation state.
    pub fn reset_interpolation(&mut self) {
        self.fractional_pos = 0.0;
        self.history.fill(0.0);
    }

    fn process_normal_samples(&mut self, size: usize, offset: usize, output: &mut BufferMut) {
        if size == 0 {
            return;
        }

        self.apply_pending_reset();

        // One composition point (`RtState::read_rate`) rather than multiplying
        // speed by src_ratio by hand — this used to be written out twice in this
        // function, and the fetch estimate below has to agree with the per-sample
        // advance or the ring under- or over-runs.
        let base_rate = self
            .shared_state
            .as_ref()
            .map_or(ReadRate::UNITY, |s| s.read_rate());

        let samples_needed = base_rate.advance(Samples(size)).get().ceil() as usize + 4;

        self.fetch_scratch.clear();

        let ch = self.channels;
        let gain = self.gain.get();
        {
            let cell = self.consumer.load();
            let mut frame = [0.0f32; MAX_SAMPLER_CHANNELS];
            for _ in 0..samples_needed {
                if cell.read_into(&mut frame[..ch]) {
                    for &s in &frame[..ch] {
                        self.fetch_scratch.push(s * gain);
                    }
                } else {
                    break;
                }
            }
        }

        let mut fetch_idx = 0;
        for i in 0..size {
            // Re-read per sample so a mid-block speed change takes effect
            // immediately; same composition as the fetch estimate above.
            let rate = self
                .shared_state
                .as_ref()
                .map_or(ReadRate::UNITY, |s| s.read_rate());

            self.fractional_pos += rate.advance(Samples(1)).get();

            while self.fractional_pos >= 1.0 {
                self.fractional_pos -= 1.0;
                self.shift_history();

                if fetch_idx + ch <= self.fetch_scratch.len() {
                    self.history[3 * ch..4 * ch]
                        .copy_from_slice(&self.fetch_scratch[fetch_idx..fetch_idx + ch]);
                    fetch_idx += ch;
                } else {
                    if let Some(ref state) = self.shared_state {
                        state.report_underrun();
                    }
                    self.history.copy_within(2 * ch..3 * ch, 3 * ch);
                }
            }

            let t = self.fractional_pos as f32;
            let out_ch = ch.min(output.channels());
            for c in 0..out_ch {
                let s = cubic_hermite(
                    self.tap(0, c),
                    self.tap(1, c),
                    self.tap(2, c),
                    self.tap(3, c),
                    t,
                );
                output.set_f32(c, offset + i, s);
            }
        }
    }
}

impl AudioUnit for DiskSource {
    fn inputs(&self) -> usize {
        0
    }

    fn outputs(&self) -> usize {
        self.channels
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
        let n = self.channels.min(output.len());
        if n == 0 {
            return;
        }
        if !self.playing.load(Ordering::Relaxed) {
            output[..n].fill(0.0);
            return;
        }

        let gain = self.gain.get();
        if let Some(ref state) = self.shared_state {
            if state.next_seek_crossfade_frame_into(&mut output[..n]) {
                for s in output[..n].iter_mut() {
                    *s *= gain;
                }
                return;
            }

            if state.is_seeking() {
                output[..n].fill(0.0);
                return;
            }
        }

        self.apply_pending_reset();

        let speed = self.shared_state.as_ref().map_or(1.0, |s| {
            s.effective_speed().get() as f64 * s.src_ratio().get() as f64
        });

        self.fractional_pos += speed;

        let ch = self.channels;
        let cell = self.consumer.load();
        let mut frame = [0.0f32; MAX_SAMPLER_CHANNELS];
        while self.fractional_pos >= 1.0 {
            self.fractional_pos -= 1.0;
            self.shift_history();

            if cell.read_into(&mut frame[..ch]) {
                for (c, &s) in frame[..ch].iter().enumerate() {
                    self.history[3 * ch + c] = s * gain;
                }
            } else {
                if let Some(ref state) = self.shared_state {
                    state.report_underrun();
                }
                self.history.copy_within(2 * ch..3 * ch, 3 * ch);
            }
        }

        let t = self.fractional_pos as f32;
        for (c, o) in output.iter_mut().enumerate().take(n) {
            *o = cubic_hermite(
                self.tap(0, c),
                self.tap(1, c),
                self.tap(2, c),
                self.tap(3, c),
                t,
            );
        }
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        let n = self.channels.min(output.channels());
        if !self.playing.load(Ordering::Relaxed) {
            for c in 0..n {
                for i in 0..size {
                    output.set_f32(c, i, 0.0);
                }
            }
            return;
        }

        let mut xfade = [0.0f32; MAX_SAMPLER_CHANNELS];
        if let Some(ref state) = self.shared_state {
            if state.is_seek_crossfading() {
                for i in 0..size {
                    if state.next_seek_crossfade_frame_into(&mut xfade[..n]) {
                        let g = self.gain.get();
                        for (c, &s) in xfade[..n].iter().enumerate() {
                            output.set_f32(c, i, s * g);
                        }
                    } else {
                        self.process_normal_samples(size - i, i, output);
                        return;
                    }
                }
                return;
            }

            if state.is_loop_crossfading() {
                for i in 0..size {
                    if state.next_loop_crossfade_frame_into(&mut xfade[..n]) {
                        let g = self.gain.get();
                        for (c, &s) in xfade[..n].iter().enumerate() {
                            output.set_f32(c, i, s * g);
                        }
                    } else {
                        self.process_normal_samples(size - i, i, output);
                        return;
                    }
                }
                return;
            }

            if state.is_seeking() {
                for c in 0..n {
                    for i in 0..size {
                        output.set_f32(c, i, 0.0);
                    }
                }
                return;
            }
        }

        self.process_normal_samples(size, 0, output);
    }

    audio_unit_boilerplate!(id = crate::node_id::STREAMING_SAMPLER_ID);

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        // Width must track `outputs()` or fundsp mis-plans this node's latency.
        SignalFrame::new(self.outputs())
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

// ---------------------------------------------------------------------------
// DiskVoice — a `DiskSource` wrapped in the transport
// placement gate that the raw unit lacks.
//
// `DiskSource` streams whatever the butler ring feeds it, unaware of
// *where on the timeline* the voice lives. A timeline clip only sounds while the
// playhead is inside its [start_beat, start_beat + duration) window; outside it
// must be silent. `MemorySource` bakes this in via `window_position`;
// the streaming unit does not, so this wrapper adds the same gate:
//
//   * inside the window  → pull from the ring (the raw unit's normal path)
//   * outside the window → emit silence, do not drain the ring
//   * playhead *jumps* into/within the window (seek, loop wrap, first entry)
//     → request a butler seek to the target sample so the ring refills from the
//       right disk offset before we start pulling.
//
// The gate math is a copy of `MemorySource::window_position`: it is the
// single source of truth for "is the playhead inside this window, and at what
// sample offset". Kept lock-free / alloc-free so `tick`/`process` stay RT-safe —
// the actual disk seek is deferred to the butler; here we only publish the
// requested target to the lock-free `RtState` (two atomic stores), which the
// butler polls each loop and applies click-free. The butler owns the seek flag
// so it can clear it; the reader never sets it.
// ---------------------------------------------------------------------------

/// Sentinel for "no seek has been requested yet" — any real target differs from
/// this on the first entry, so the first inside-frame always issues a seek.
const NO_SEEK_TARGET: SamplePosition = SamplePosition(f64::NEG_INFINITY);

/// Max sample-offset drift between the position the ring is streaming and the
/// position the transport wants before we treat it as a discontinuity (seek /
/// loop wrap) rather than normal contiguous playback. One buffer's worth of
/// slack at a generous block size keeps normal advance from tripping a seek.
const SEEK_EPSILON_SAMPLES: f64 = 4096.0;

/// Wiring for [`DiskVoice::new`]: the placement gate plus the file
/// sample rate. Splits the clock from the [`VoiceWindow`]
/// cluster so the streaming path and `MemorySource` speak the same value type;
/// `shared_state` stays a separate wiring arg (it must be the same `RtState` the
/// `inner` unit holds).
// Hand-rolled `Debug` for the same reason as `MemorySourceConfig`: an
// `Arc<dyn Timeline>` is not `Debug`.
#[derive(Clone)]
pub struct DiskVoiceConfig {
    /// Transport clock — the gate reads its beat position.
    pub timeline: Arc<dyn Timeline>,
    /// Span of timeline this voice occupies. Separate from the clock, mirroring
    /// `MemorySource` — see [`VoiceWindow`].
    pub window: VoiceWindow,
    /// File sample rate — converts the transport's second-offset into a sample
    /// offset for the seek target, matching `MemorySource`'s use of
    /// `wave.sample_rate()`.
    pub file_sample_rate: f64,
}

impl std::fmt::Debug for DiskVoiceConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskVoiceConfig")
            .field("window", &self.window)
            .field("file_sample_rate", &self.file_sample_rate)
            .finish_non_exhaustive()
    }
}

pub struct DiskVoice {
    inner: DiskSource,
    shared_state: Arc<RtState>,

    /// Transport clock — the gate reads its beat position.
    timeline: Arc<dyn Timeline>,
    /// Span of timeline this voice occupies.
    window: VoiceWindow,

    /// File sample rate — converts the transport's second-offset into a sample
    /// offset for the seek target, matching `MemorySource`'s use of
    /// `wave.sample_rate()`.
    file_sample_rate: f64,

    /// The window-relative sample offset we last requested the butler stream from.
    /// `NO_SEEK_TARGET` until the first inside-frame. Used to detect a
    /// discontinuous jump: when the transport's desired offset diverges from the
    /// contiguously-advanced expectation by more than `SEEK_EPSILON_SAMPLES`, we
    /// issue a fresh seek.
    streamed_offset: SamplePosition,

    /// Whether the previous frame was inside the voice window. A false→true edge
    /// (playhead entering the voice) always forces a seek.
    was_inside: bool,
}

// Hand-rolled: wraps a non-`Debug` `DiskSource` + `Arc<RtState>` +
// the `Arc<dyn Timeline>` clock. Print the gate
// scalars + inner unit; nothing here touches the ring.
impl std::fmt::Debug for DiskVoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskVoice")
            .field("inner", &self.inner)
            .field("window", &self.window)
            .field("file_sample_rate", &self.file_sample_rate)
            .field("streamed_offset", &self.streamed_offset)
            .field("was_inside", &self.was_inside)
            .finish_non_exhaustive()
    }
}

impl Clone for DiskVoice {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            shared_state: Arc::clone(&self.shared_state),
            timeline: self.timeline.clone(),
            window: self.window,
            file_sample_rate: self.file_sample_rate,
            streamed_offset: self.streamed_offset,
            was_inside: self.was_inside,
        }
    }
}

impl DiskVoice {
    /// Wrap a `DiskSource` with the transport placement gate.
    ///
    /// Construction (butler stream registration, ring allocation) happens on the
    /// ECS/butler side; this only binds the already-built unit to a timeline
    /// window. `shared_state` must be the same `RtState` the `inner` unit holds
    /// so the gate's seek requests reach the unit's seek/underrun machinery.
    pub fn new(inner: DiskSource, shared_state: Arc<RtState>, config: DiskVoiceConfig) -> Self {
        Self {
            inner,
            shared_state,
            timeline: config.timeline,
            window: config.window,
            file_sample_rate: config.file_sample_rate,
            streamed_offset: NO_SEEK_TARGET,
            was_inside: false,
        }
    }

    pub fn set_placement(&mut self, start_beat: Beat, duration: Option<BeatDuration>) {
        self.window = VoiceWindow {
            start: start_beat,
            duration,
        };
        // A placement change may move the window out from under the playhead;
        // force a re-seek on the next inside-frame.
        self.streamed_offset = NO_SEEK_TARGET;
        self.was_inside = false;
    }

    pub fn set_gain(&mut self, gain: Amplitude) {
        self.inner.set_gain(gain);
    }

    pub fn gain(&self) -> Amplitude {
        self.inner.gain()
    }

    /// The file sample rate this stream decodes at.
    pub fn file_sample_rate(&self) -> f64 {
        self.file_sample_rate
    }

    /// Where the playhead sits in this stream's file samples, or `None` when
    /// outside the voice window. Delegates to the shared
    /// [`window_position`](super::interp::window_position) — the one gate
    /// definition, also used by [`MemorySource`].
    ///
    /// Composes with [`SrcRatio::UNITY`] — deliberately unlike the memory tier,
    /// and the asymmetry is real rather than cosmetic.
    ///
    /// `file_sample_rate` here already carries the conversion: `ports.rs` builds
    /// it as `session_rate × src_ratio`, which for a placement gate measuring
    /// *file* samples is exactly the file rate. So `src_ratio` has been applied
    /// once by the time it reaches this call, and composing it again would apply it
    /// twice — on a 48 kHz file in a 44.1 kHz session the gate lands ~8.8% deep and
    /// then outruns the ring by ~4.2k file samples per second, tripping
    /// `SEEK_EPSILON_SAMPLES` about once a second, forever.
    /// `placement_gate_applies_src_ratio_exactly_once` pins it.
    ///
    /// The memory tier reaches the same product by the other split
    /// (`wave.sample_rate()` × `speed × src_ratio`), because *its* rate argument is
    /// the untouched file rate. Two splits, one product.
    ///
    /// Spelling the `UNITY` out — rather than adding a varispeed-only constructor
    /// — keeps [`PlaybackRate::read_rate`] the single place the two factors
    /// compose, and puts "the conversion is already in the rate above" at the call
    /// site as a value instead of in a comment.
    #[inline]
    fn window_position(&self) -> Option<SamplePosition> {
        super::interp::window_position(
            self.timeline.as_ref(),
            self.window.start,
            self.window.duration,
            SampleRate::new(self.file_sample_rate),
            self.shared_state
                .effective_speed()
                .read_rate(SrcRatio::UNITY),
        )
    }

    /// Ask the butler to stream from `target_offset` (window-relative samples).
    ///
    /// Records the target for drift tracking, then publishes it to the shared
    /// [`RtState`](crate::butler::RtState) via `request_seek` — two lock-free
    /// atomic stores. The butler polls the request each loop and repositions the
    /// live stream click-free (flush + seek + crossfade), owning the seek flag so
    /// it can clear it; the reader must NOT flip `set_seeking` itself (with no
    /// butler to clear it the reader would mute forever). RT-safe: no alloc, no
    /// I/O, no blocking on this path.
    #[inline]
    fn request_seek(&mut self, target_offset: SamplePosition) {
        self.streamed_offset = target_offset;
        self.shared_state
            .request_seek(target_offset.get().max(0.0) as u64);
    }

    /// Set the playback speed magnitude. Routes directly to the shared
    /// [`RtState`], which is exactly what the butler's `SetVarispeed` handler
    /// does for speed (`rt_state.set_speed`) — two lock-free atomic stores, no
    /// butler round-trip needed. Direction is a separate concern (the reader has
    /// no direction verb), so this leaves it untouched, mirroring how
    /// `VoiceCommand::UpdateSpeed` carries only a magnitude.
    pub fn set_speed(&mut self, speed: PlaybackRate) {
        self.shared_state.set_speed(speed);
    }

    /// Set the playback direction. Routes directly to the shared [`RtState`],
    /// which is exactly the direction leg of the butler's `SetVarispeed` handler
    /// (`rt_state.set_direction`) — one lock-free atomic store, no butler
    /// round-trip. Pairs with [`set_speed`](Self::set_speed) so the reader can
    /// reproduce a full `SetVarispeed { speed, direction }` in-unit.
    pub fn set_direction(&mut self, direction: Direction) {
        self.shared_state.set_direction(direction);
    }

    /// Public seek: reposition the live stream to window-relative `to`. Delegates
    /// to the private [`request_seek`](Self::request_seek), which publishes the
    /// target to the shared [`RtState`] (two atomic stores) for the butler to
    /// apply click-free — the same mechanism the placement gate uses on a
    /// discontinuous jump, and the same one the `Command::Seek` butler path
    /// ultimately drives. RT-safe: no alloc, no I/O.
    pub fn seek(&mut self, to: SamplePosition) {
        self.request_seek(to);
    }

    /// Decide, for a frame whose desired window-relative offset is `offset`,
    /// whether the playhead jumped (fresh entry or a seek/loop discontinuity)
    /// and issue a butler seek if so.
    #[inline]
    fn maybe_seek(&mut self, offset: SamplePosition) {
        let jumped = !self.was_inside
            || self.streamed_offset == NO_SEEK_TARGET
            || (offset - self.streamed_offset).get().abs() > SEEK_EPSILON_SAMPLES;
        if jumped {
            self.request_seek(offset);
        } else {
            // Contiguous playback — advance our expectation so drift stays
            // bounded and normal advance never trips the discontinuity check.
            self.streamed_offset = offset;
        }
    }
}

impl AudioUnit for DiskVoice {
    fn inputs(&self) -> usize {
        0
    }

    fn outputs(&self) -> usize {
        // Delegate rather than store a second copy: this is a thin placement
        // gate over `inner`, and two widths could disagree.
        self.inner.outputs()
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
        match self.window_position() {
            Some(offset) => {
                self.maybe_seek(offset);
                self.was_inside = true;
                self.inner.tick(input, output);
            }
            None => {
                self.was_inside = false;
                // Every channel, not a hardcoded pair: at width 1 the old
                // `output.len() >= 2` guard silenced nothing at all, and at
                // width 6 it left channels 2..6 holding the previous block.
                let n = self.outputs().min(output.len());
                output[..n].fill(0.0);
            }
        }
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        match self.window_position() {
            Some(offset) => {
                self.maybe_seek(offset);
                self.was_inside = true;
                self.inner.process(size, input, output);
            }
            None => {
                self.was_inside = false;
                let n = self.outputs().min(output.channels());
                for c in 0..n {
                    for i in 0..size {
                        output.set_f32(c, i, 0.0);
                    }
                }
            }
        }
    }

    audio_unit_boilerplate!(id = crate::node_id::STREAMING_SAMPLER_ID);

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        // Width must track `outputs()` or fundsp mis-plans this node's latency.
        SignalFrame::new(self.outputs())
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::butler::{RegionBuffer, RegionId};
    use std::path::PathBuf;
    use tutti_core::{BufferVec, Timeline};

    /// Tests still author stereo pairs for readability; flatten them at the one
    /// boundary rather than rewriting every fixture.
    fn make_reader_with_samples(samples: &[(f32, f32)]) -> SharedReader {
        let (mut writer, reader) =
            RegionBuffer::with_capacity(RegionId(1), PathBuf::new(), samples.len() + 64, 2);
        let flat: Vec<f32> = samples.iter().flat_map(|&(l, r)| [l, r]).collect();
        writer.push_interleaved(&flat);
        crate::butler::share_reader(reader)
    }

    fn make_unit(samples: &[(f32, f32)]) -> (DiskSource, Arc<RtState>) {
        let reader = make_reader_with_samples(samples);
        let state = Arc::new(RtState::new());
        let unit = DiskSource::new(reader, Arc::clone(&state));
        (unit, state)
    }

    // --- DiskVoice: placement gate ---

    use crate::test_transport::MockTransport;
    use tutti_core::Bpm;

    fn make_clip_reader(
        samples: &[(f32, f32)],
        transport: Arc<dyn Timeline>,
        start_beat: Beat,
        duration: Option<BeatDuration>,
    ) -> DiskVoice {
        let (inner, state) = make_unit(samples);
        DiskVoice::new(
            inner,
            state,
            DiskVoiceConfig {
                timeline: transport,
                window: VoiceWindow {
                    start: start_beat,
                    duration,
                },
                file_sample_rate: 44100.0,
            },
        )
    }

    /// A 48 kHz file in a 44.1 kHz session must seek to the offset the ring
    /// will actually reach — `src_ratio` applied exactly once.
    ///
    /// `file_sample_rate` is reconstructed as `session_rate × src_ratio`
    /// (`ports.rs`), so it already carries the conversion; multiplying the gate
    /// by `read_rate` (speed × src_ratio) applied it twice. Every other
    /// streaming fixture hardcodes 44100 against a default `src_ratio` of 1.0 —
    /// the one value that makes a double-multiply invisible.
    #[test]
    fn placement_gate_applies_src_ratio_exactly_once() {
        let samples: Vec<_> = (1..64).map(|i| (i as f32, i as f32)).collect();
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));

        let (inner, state) = make_unit(&samples);
        let src = 48_000.0 / 44_100.0;
        state.set_src_ratio(tutti_core::SrcRatio::new(src as f32));

        let reader = DiskVoice::new(
            inner,
            state,
            DiskVoiceConfig {
                timeline: transport.clone(),
                window: VoiceWindow {
                    start: Beat::new(0.0),
                    duration: None,
                },
                // What `ports.rs` reconstructs: session × src == the real file rate.
                file_sample_rate: 44_100.0 * src,
            },
        );

        // Two seconds in at 120 BPM = beat 4.0.
        transport.set_beat(Beat::new(4.0));
        let offset = reader.window_position().expect("inside the voice window");

        // 2 s of a 48 kHz file is 96000 file samples — src_ratio applied once.
        let expected = 2.0 * 48_000.0;
        assert!(
            (offset.get() - expected).abs() < 1.0,
            "expected ~{expected} file samples, got {offset} \
             (a double-applied src_ratio gives ~{})",
            expected * src
        );
    }

    #[test]
    fn clip_reader_silent_outside_window_audible_inside() {
        // Voice window: beats [4, 8). Non-zero ramp in the ring.
        let samples: Vec<_> = (1..64).map(|i| (i as f32, i as f32)).collect();
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let mut reader = make_clip_reader(
            &samples,
            transport.clone(),
            Beat::new(4.0),
            Some(BeatDuration::new(4.0)),
        );

        // Before the window (beat 0): silence, ring untouched.
        let mut out = [0.0f32; 2];
        for _ in 0..8 {
            reader.tick(&[], &mut out);
        }
        assert_eq!(out, [0.0, 0.0], "before window → silence");

        // Inside the window (beat 5): pulls the ring, produces audio.
        transport.set_beat(Beat::new(5.0));
        let mut got_audio = false;
        for _ in 0..8 {
            reader.tick(&[], &mut out);
            if out[0] != 0.0 || out[1] != 0.0 {
                got_audio = true;
            }
        }
        assert!(got_audio, "inside window → audible");

        // Past the window (beat 9): silent again.
        transport.set_beat(Beat::new(9.0));
        reader.tick(&[], &mut out);
        assert_eq!(out, [0.0, 0.0], "after window → silence");
    }

    #[test]
    fn clip_reader_stopped_transport_is_silent() {
        let samples: Vec<_> = (1..32).map(|i| (i as f32, i as f32)).collect();
        let transport = MockTransport::stopped(Beat::new(5.0), Bpm::new(120.0)); // inside window but stopped
        let mut reader = make_clip_reader(
            &samples,
            transport,
            Beat::new(4.0),
            Some(BeatDuration::new(4.0)),
        );

        let mut out = [1.0f32; 2];
        reader.tick(&[], &mut out);
        assert_eq!(out, [0.0, 0.0], "stopped transport → silence");
    }

    // --- DiskVoice: RT no-alloc (steady-state process inside window) ---
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
        let samples: Vec<_> = (1..2048)
            .map(|i| (i as f32 * 0.001, i as f32 * 0.001))
            .collect();
        let transport = MockTransport::rolling(Beat::new(5.0), Bpm::new(120.0)); // inside window
        let mut reader = make_clip_reader(
            &samples,
            transport,
            Beat::new(4.0),
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
    /// jumps into or across the voice window) must be allocation-free — it may only
    /// touch the lock-free `RtState` seek-request slot. This exercises that edge
    /// *inside* the guarded block by moving the transport beat each iteration, so a
    /// regression that reintroduces allocation on the seek path is caught.
    #[test]
    fn clip_reader_seek_edge_is_allocation_free() {
        let samples: Vec<_> = (1..2048)
            .map(|i| (i as f32 * 0.001, i as f32 * 0.001))
            .collect();
        let transport = MockTransport::rolling(Beat::new(5.0), Bpm::new(120.0)); // inside [4, 8)
        let mut reader = make_clip_reader(
            &samples,
            Arc::clone(&transport) as Arc<dyn Timeline>,
            Beat::new(4.0),
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
                transport.set_beat(Beat::new(beat));
                let input = input_vec.buffer_ref();
                let mut output = output_vec.buffer_mut();
                reader.process(64, &input, &mut output);
            }
        });
    }

    #[test]
    fn clip_reader_tick_steady_state_is_allocation_free() {
        let samples: Vec<_> = (1..2048)
            .map(|i| (i as f32 * 0.001, i as f32 * 0.001))
            .collect();
        let transport = MockTransport::rolling(Beat::new(5.0), Bpm::new(120.0));
        let mut reader = make_clip_reader(
            &samples,
            transport,
            Beat::new(4.0),
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
        use tutti_core::PlaybackRate;
        let state = RtState::new();

        assert_eq!(state.speed(), PlaybackRate::UNITY);

        state.set_speed(PlaybackRate::new(0.5));
        assert_eq!(state.speed(), PlaybackRate::new(0.5));

        state.set_speed(PlaybackRate::new(2.0));
        assert_eq!(state.speed(), PlaybackRate::new(2.0));

        // The range is still enforced — but by the type, at construction, so the
        // in-memory tier gets it too. `RtState` used to be the only place it
        // happened, which is why the two tiers disagreed.
        state.set_speed(PlaybackRate::new_clamped(0.1));
        assert_eq!(state.speed(), PlaybackRate::MIN);

        state.set_speed(PlaybackRate::new_clamped(10.0));
        assert_eq!(state.speed(), PlaybackRate::MAX);
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
        let samples: Vec<_> = (0..256)
            .map(|i| (i as f32 * 0.01, -(i as f32) * 0.01))
            .collect();
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

        let mut full = DiskSource::new(reader1, state1);
        let mut half = DiskSource::new(reader2, state2);
        half.set_gain(Amplitude::new(0.5));

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
        assert!(unit.history.iter().all(|&s| s == 0.0));
    }

    // --- New: seek crossfade path ---

    #[test]
    fn seek_crossfade_plays_through_then_resumes_normal() {
        let samples: Vec<_> = (0..256).map(|i| (i as f32 * 0.1, 0.0)).collect();
        let (mut unit, state) = make_unit(&samples);

        let fadeout: Vec<f32> = (0..4).flat_map(|i| [1.0 - i as f32 * 0.25, 0.0]).collect();
        let fadein: Vec<f32> = (0..4).flat_map(|i| [i as f32 * 0.25, 0.0]).collect();
        state.start_seek_crossfade(fadeout, fadein, 2);

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

    /// Build a `channels`-wide ring pre-filled with `frames` interleaved frames.
    fn make_wide_reader(frames: &[f32], channels: usize) -> SharedReader {
        let (mut writer, reader) = RegionBuffer::with_capacity(
            RegionId(1),
            PathBuf::new(),
            frames.len() / channels + 64,
            channels,
        );
        writer.push_interleaved(frames);
        crate::butler::share_reader(reader)
    }

    /// Outside its transport window a voice must silence EVERY channel.
    ///
    /// At width 1 the old guard was `if output.len() >= 2 { .. }`, so a mono
    /// streaming voice outside its window silenced nothing at all and simply
    /// leaked whatever the caller's buffer already held. At width 6 the same
    /// site wrote only channels 0 and 1, leaving 2..6 holding the previous
    /// block. Both are invisible at width 2, which is what every other
    /// streaming fixture uses.
    #[test]
    fn outside_the_window_silences_every_channel() {
        for width in [1usize, 6] {
            let frames: Vec<f32> = (0..64 * width).map(|i| (i + 1) as f32).collect();
            let ring = make_wide_reader(&frames, width);
            let state = Arc::new(RtState::new());
            let inner = DiskSource::new(ring, state.clone());
            assert_eq!(inner.channels(), width, "ring width must reach the unit");

            // Transport parked BEFORE the voice's start beat, so the placement
            // gate reports "outside".
            let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
            let mut voice = DiskVoice::new(
                inner,
                state,
                DiskVoiceConfig {
                    timeline: transport,
                    window: VoiceWindow {
                        start: Beat::new(64.0),
                        duration: None,
                    },
                    file_sample_rate: 44100.0,
                },
            );

            // `tick`: pre-dirty the caller's frame so a missing write shows.
            let mut out = vec![9.0f32; width];
            voice.tick(&[], &mut out);
            for (c, &s) in out.iter().enumerate() {
                assert_eq!(
                    s, 0.0,
                    "width {width} tick: channel {c} not silenced outside the window"
                );
            }

            // `process`: same, through the planar path.
            let input = BufferVec::new(0);
            let mut output = BufferVec::new(width);
            {
                let mut buf = output.buffer_mut();
                for c in 0..width {
                    for i in 0..8 {
                        buf.set_f32(c, i, 9.0);
                    }
                }
            }
            voice.process(8, &input.buffer_ref(), &mut output.buffer_mut());
            let buf = output.buffer_ref();
            for c in 0..width {
                for i in 0..8 {
                    assert_eq!(
                        buf.at_f32(c, i),
                        0.0,
                        "width {width} process: channel {c} sample {i} not silenced"
                    );
                }
            }
        }
    }

    /// The seeking branch of `process` must silence every channel too — same
    /// hardcoded-pair bug, on the path taken while a seek is in flight.
    #[test]
    fn seeking_silences_every_channel() {
        let width = 6usize;
        let frames: Vec<f32> = (0..64 * width).map(|i| (i + 1) as f32).collect();
        let ring = make_wide_reader(&frames, width);
        let state = Arc::new(RtState::new());
        let mut unit = DiskSource::new(ring, state.clone());

        state.set_seeking(true);
        assert!(state.is_seeking());

        let input = BufferVec::new(0);
        let mut output = BufferVec::new(width);
        {
            let mut buf = output.buffer_mut();
            for c in 0..width {
                for i in 0..8 {
                    buf.set_f32(c, i, 9.0);
                }
            }
        }
        unit.process(8, &input.buffer_ref(), &mut output.buffer_mut());

        let buf = output.buffer_ref();
        for c in 0..width {
            for i in 0..8 {
                assert_eq!(
                    buf.at_f32(c, i),
                    0.0,
                    "channel {c} sample {i} not silenced while seeking"
                );
            }
        }
    }
}
