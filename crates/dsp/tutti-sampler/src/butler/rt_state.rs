//! Shared state between butler and audio thread.
//!
//! All fields are atomic or lock-free. Organized into orthogonal sub-structs
//! (playback, health, seek/loop crossfade) — one `Arc`, one allocation, but
//! fields are grouped by concern to reduce false sharing and make the
//! ownership story obvious.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use tutti_core::ChannelLayout;
use tutti_core::{Amplitude, AtomicF32, AtomicReadRate, PlaybackRate, ReadRate, SrcRatio};

use super::crossfader::StreamingCrossfader;
use crate::voice::types::Direction;

/// Playback parameters read by the audio thread every sample.
#[repr(align(64))]
pub struct PlaybackParams {
    speed: AtomicF32,
    /// 0 = forward, 1 = reverse.
    direction: AtomicU8,
    /// file_sample_rate / session_sample_rate. 1.0 = no conversion.
    src_ratio: AtomicF32,
    /// Linear output gain.
    ///
    /// Here rather than on `DiskSource` for the reason `tutti_units`' crate
    /// docs give: a control stored **by value** in a unit cannot be changed on
    /// a live node, because `Net`'s frontend holds clones and `Net::migrate`
    /// discards edits to them. Gain was a plain `Amplitude` field, so a clip's
    /// fader did nothing once its voice existed — silently.
    gain: AtomicF32,
    /// Source samples consumed per output sample by a wrapping time-stretcher:
    /// `1 / stretch`. 1.0 when the voice does not stretch.
    ///
    /// Held here rather than in `DiskSource` because the stretch filter and the
    /// ring reader never meet: the filter lives in the `VoiceSlot`, the reader
    /// behind the butler's `SharedReader`. `RtState` is the cell both already
    /// share, and it is where the other two rate factors compose.
    ///
    /// `AtomicReadRate`, not `AtomicF32`, unlike the two neighbours: those hold
    /// `f32`-backed units, while `ReadRate` is `f64` precisely because a disk
    /// voice accumulates it into a read position once per sample. Storing it
    /// narrowed reinstated that drift on every load.
    stretch_rate: AtomicReadRate,
}

impl Default for PlaybackParams {
    fn default() -> Self {
        Self {
            speed: AtomicF32::new(1.0),
            direction: AtomicU8::new(0),
            src_ratio: AtomicF32::new(1.0),
            gain: AtomicF32::new(1.0),
            stretch_rate: AtomicReadRate::new(ReadRate::UNITY),
        }
    }
}

/// Buffer health / underrun reporting.
#[repr(align(64))]
pub struct BufferHealth {
    seeking: AtomicBool,
    underrun_count: AtomicU64,
    /// 0-1000 representing 0.0-1.0.
    buffer_fill_level: AtomicU32,
    /// Butler-bumped ring-reset request. The butler increments this when it
    /// repositions the stream (seek / loop-wrap) and needs the audio thread to
    /// drop the stale buffered samples. The audio thread — the sole ring
    /// consumer — clears the ring when it observes a change vs. its last-applied
    /// value, keeping the SPSC pop single-threaded (the butler never pops).
    reset_epoch: AtomicU64,
    /// Audio-bumped timeline-seek request (the mirror image of `reset_epoch`:
    /// audio requests, butler applies). The audio thread stores the absolute
    /// file offset in `seek_target` and bumps `seek_request_epoch`; the butler
    /// polls the epoch and, on a change vs. `applied_seek_epoch`, repositions the
    /// live stream to `seek_target` (flush + seek + crossfade). Both stores are
    /// lock-free and allocation-free — safe on the audio hot path.
    seek_target: AtomicU64,
    seek_request_epoch: AtomicU64,
    /// Butler-only last-applied seek epoch. Never touched by the audio thread —
    /// the symmetric counterpart to the audio-side `applied_reset_epoch`. Lets
    /// the butler coalesce rapid seeks (only the latest `seek_target` survives).
    applied_seek_epoch: AtomicU64,
}

impl Default for BufferHealth {
    fn default() -> Self {
        Self {
            seeking: AtomicBool::new(false),
            underrun_count: AtomicU64::new(0),
            buffer_fill_level: AtomicU32::new(0),
            reset_epoch: AtomicU64::new(0),
            seek_target: AtomicU64::new(0),
            seek_request_epoch: AtomicU64::new(0),
            applied_seek_epoch: AtomicU64::new(0),
        }
    }
}

/// Shared state between butler and audio thread.
pub struct RtState {
    pub playback: PlaybackParams,
    pub health: BufferHealth,
    pub seek_crossfade: StreamingCrossfader,
    pub loop_crossfade: StreamingCrossfader,
}

impl Default for RtState {
    fn default() -> Self {
        Self::new()
    }
}

impl RtState {
    pub fn new() -> Self {
        Self {
            playback: PlaybackParams::default(),
            health: BufferHealth::default(),
            seek_crossfade: StreamingCrossfader::new(),
            loop_crossfade: StreamingCrossfader::new(),
        }
    }

    #[inline]
    pub fn speed(&self) -> PlaybackRate {
        PlaybackRate::new(self.playback.speed.load(Ordering::Acquire))
    }

    /// Publish a new varispeed.
    ///
    /// Takes the already-bounded [`PlaybackRate`] rather than a raw `f32`: the
    /// range used to be enforced here and *only* here, so the in-memory tier —
    /// which never went through this function — accepted speeds this one
    /// clamped. Same command, different audio per tier. The type carries the
    /// bound now, so both tiers get it.
    pub fn set_speed(&self, speed: PlaybackRate) {
        self.playback.speed.store(speed.get(), Ordering::Release);
    }

    /// The voice's linear output gain.
    #[inline]
    pub fn gain(&self) -> Amplitude {
        Amplitude::new(self.playback.gain.load(Ordering::Acquire))
    }

    /// Publish a new output gain.
    ///
    /// `&self`, like every other publisher here: the whole point of this cell
    /// is that the control thread and the audio thread hold the *same* one, so
    /// no `&mut` is available or needed.
    pub fn set_gain(&self, gain: Amplitude) {
        self.playback.gain.store(gain.get(), Ordering::Release);
    }

    /// Current playback speed. Same as [`speed`] — kept distinct from the
    /// raw atomic load for the audio-thread call site which reads it every
    /// sample.
    #[inline]
    pub fn effective_speed(&self) -> PlaybackRate {
        self.speed()
    }

    /// Source samples consumed per output sample: varispeed × conversion ×
    /// stretch.
    ///
    /// The streaming twin of `MemorySource::read_rate`, composing through the
    /// same [`PlaybackRate::read_rate`] so neither tier can drop a factor or
    /// swap the pair.
    ///
    /// The stretch term is folded in **here**, at the one composition point, for
    /// the reason the other two are: this rate has three consumers in
    /// `DiskSource` — the per-sample advance in `tick`, the per-sample advance in
    /// `process`, and the `samples_needed` fetch estimate that has to agree with
    /// them or the ring under- or over-runs. Applying stretch at the call sites
    /// instead would need all three to remember, and the fetch estimate is the
    /// one that fails silently.
    ///
    /// Without it the factor acted as varispeed on this tier: a 2x-stretched
    /// disk voice consumed exactly as many source frames as an unstretched one
    /// (measured 512 vs 512), so the vocoder was fed at full rate and had
    /// nothing to spread.
    #[inline]
    pub fn read_rate(&self) -> ReadRate {
        self.speed()
            .read_rate(self.src_ratio())
            .then(self.stretch_rate())
    }

    /// The wrapping stretcher's read rate — `1 / stretch`, or unity when the
    /// voice does not stretch.
    #[inline]
    pub fn stretch_rate(&self) -> ReadRate {
        self.playback.stretch_rate.load(Ordering::Acquire)
    }

    /// Publish the wrapping stretcher's read rate. Control thread, or the audio
    /// thread's own parameter application — a single relaxed store either way.
    #[inline]
    pub fn set_stretch_rate(&self, rate: ReadRate) {
        self.playback.stretch_rate.store(rate, Ordering::Release);
    }

    /// Current playback direction. Backed by the `AtomicU8` (0 = forward,
    /// 1 = reverse); the [`Direction`] enum is the API surface.
    #[inline]
    pub fn direction(&self) -> Direction {
        if self.playback.direction.load(Ordering::Acquire) == 1 {
            Direction::Reverse
        } else {
            Direction::Forward
        }
    }

    pub fn set_direction(&self, direction: Direction) {
        self.playback
            .direction
            .store(u8::from(direction.is_reverse()), Ordering::Release);
    }

    #[inline]
    pub fn is_reverse(&self) -> bool {
        self.direction().is_reverse()
    }

    #[cfg(test)]
    pub fn set_reverse(&self, reverse: bool) {
        self.set_direction(Direction::from_reverse(reverse));
    }

    #[inline]
    pub fn src_ratio(&self) -> SrcRatio {
        SrcRatio::new(self.playback.src_ratio.load(Ordering::Acquire))
    }

    pub fn set_src_ratio(&self, ratio: SrcRatio) {
        self.playback
            .src_ratio
            .store(ratio.get(), Ordering::Release);
    }

    #[inline]
    pub fn is_seeking(&self) -> bool {
        self.health.seeking.load(Ordering::Acquire)
    }

    pub fn set_seeking(&self, seeking: bool) {
        self.health.seeking.store(seeking, Ordering::Release);
    }

    /// Butler side: request the audio thread drop the ring's stale contents
    /// after repositioning the stream. Lock-free; the butler never touches the
    /// SPSC consumer itself.
    pub fn request_ring_reset(&self) {
        self.health.reset_epoch.fetch_add(1, Ordering::Release);
    }

    /// Current ring-reset epoch. The audio thread compares this against its
    /// last-applied value to decide whether a butler-requested clear is pending.
    #[inline]
    pub fn reset_epoch(&self) -> u64 {
        self.health.reset_epoch.load(Ordering::Acquire)
    }

    /// Audio side: request the butler reposition the live stream to absolute file
    /// offset `file_offset` (timeline seek). Two atomic stores, zero alloc, zero
    /// I/O — safe to call from the audio hot path. Coalescing is intentional:
    /// only the latest target survives if the butler hasn't caught up.
    #[inline]
    pub fn request_seek(&self, file_offset: u64) {
        self.health
            .seek_target
            .store(file_offset, Ordering::Relaxed);
        self.health
            .seek_request_epoch
            .fetch_add(1, Ordering::Release);
    }

    /// Butler side: read the pending seek request as `(epoch, target)`. The
    /// butler compares `epoch` against its last-applied value (see
    /// [`take_seek_request`](Self::take_seek_request)) to decide whether to act.
    #[inline]
    pub fn seek_request(&self) -> (u64, u64) {
        let epoch = self.health.seek_request_epoch.load(Ordering::Acquire);
        let target = self.health.seek_target.load(Ordering::Relaxed);
        (epoch, target)
    }

    /// Butler side: if a new seek has been requested since the last poll, mark it
    /// applied and return `Some(target)`; otherwise `None`. Coalesces rapid
    /// seeks — only the latest `seek_target` is returned. Butler-only: never
    /// touched by the audio thread.
    #[inline]
    pub fn take_seek_request(&self) -> Option<u64> {
        let (epoch, target) = self.seek_request();
        if epoch == self.health.applied_seek_epoch.load(Ordering::Relaxed) {
            return None;
        }
        self.health
            .applied_seek_epoch
            .store(epoch, Ordering::Relaxed);
        Some(target)
    }

    #[inline]
    pub fn report_underrun(&self) {
        self.health.underrun_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn take_underruns(&self) -> u64 {
        self.health.underrun_count.swap(0, Ordering::Relaxed)
    }

    pub fn set_buffer_fill(&self, level: f32) {
        let scaled = (level.clamp(0.0, 1.0) * 1000.0) as u32;
        self.health
            .buffer_fill_level
            .store(scaled, Ordering::Relaxed);
    }

    /// 0.0 = empty, 1.0 = full. Near 0.0 means underrun risk.
    #[inline]
    pub fn buffer_fill(&self) -> f32 {
        self.health.buffer_fill_level.load(Ordering::Relaxed) as f32 / 1000.0
    }

    pub fn start_seek_crossfade(
        &self,
        fadeout: Vec<f32>,
        fadein: Vec<f32>,
        channels: impl Into<ChannelLayout>,
    ) {
        self.seek_crossfade.start(fadeout, fadein, channels);
    }

    #[inline]
    pub fn is_seek_crossfading(&self) -> bool {
        self.seek_crossfade.is_active()
    }

    pub fn next_seek_crossfade_frame_into(&self, out: &mut [f32]) -> bool {
        self.seek_crossfade.next_frame_into(out)
    }

    pub fn start_loop_crossfade(
        &self,
        fadeout: Vec<f32>,
        fadein: Vec<f32>,
        channels: impl Into<ChannelLayout>,
    ) {
        self.loop_crossfade.start(fadeout, fadein, channels);
    }

    #[inline]
    pub fn is_loop_crossfading(&self) -> bool {
        self.loop_crossfade.is_active()
    }

    pub fn next_loop_crossfade_frame_into(&self, out: &mut [f32]) -> bool {
        self.loop_crossfade.next_frame_into(out)
    }

    pub fn clear_loop_crossfade(&self) {
        self.loop_crossfade.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_values() {
        let state = RtState::new();
        assert_eq!(state.speed(), PlaybackRate::UNITY);
        assert!(!state.is_reverse());
        assert!(!state.is_seeking());
    }

    /// The stretch rate survives the cell bit-for-bit.
    ///
    /// It used to round-trip through an `AtomicF32`, so every load returned a
    /// value the control thread never wrote. That matters here and not for the
    /// neighbouring `speed`/`src_ratio` cells because this is the term a disk
    /// voice accumulates into its read position once per sample: the error does
    /// not average out, it integrates.
    ///
    /// The magnitude is modest — the f32 round-trip costs ~3e-8 relative, so a
    /// triplet stretch drifts about 0.03 frames per minute and 1.7 frames per
    /// hour at 48 kHz. Inaudible in a pop song, a real seek offset in a long
    /// installation piece, and free to avoid.
    #[test]
    fn the_stretch_rate_cell_does_not_narrow_what_it_is_given() {
        let state = RtState::new();
        // Not representable in f32 — 1/3 at f64 width, the ratio a triplet
        // stretch actually produces.
        let rate = ReadRate::new(1.0 / 3.0);
        state.set_stretch_rate(rate);
        assert_eq!(state.stretch_rate(), rate);

        // An hour at 48 kHz: the stored rate still advances the read position
        // exactly, where the narrowed one is over a frame out.
        let frames = tutti_core::Samples(48_000 * 3600);
        let exact = rate.advance(frames);
        assert_eq!(state.stretch_rate().advance(frames), exact);
        let narrowed = ReadRate::new(f64::from(rate.get() as f32)).advance(frames);
        assert!(
            (narrowed.get() - exact.get()).abs() > 1.0,
            "expected the f32 round-trip to cost more than a frame over an hour"
        );
    }

    #[test]
    fn test_speed_clamping() {
        // The clamp moved into `PlaybackRate` so BOTH playback tiers get it —
        // this used to be the only place it happened, so the in-memory sampler,
        // which never called this setter, accepted out-of-range speeds.
        let state = RtState::new();

        state.set_speed(PlaybackRate::new_clamped(0.1));
        assert_eq!(state.speed(), PlaybackRate::MIN);

        state.set_speed(PlaybackRate::new_clamped(10.0));
        assert_eq!(state.speed(), PlaybackRate::MAX);

        state.set_speed(PlaybackRate::new_clamped(2.0));
        assert_eq!(state.speed(), PlaybackRate::new(2.0));
    }

    #[test]
    fn test_direction() {
        let state = RtState::new();
        assert!(!state.is_reverse());
        state.set_reverse(true);
        assert!(state.is_reverse());
        state.set_reverse(false);
        assert!(!state.is_reverse());
    }

    #[test]
    fn test_seeking() {
        let state = RtState::new();
        assert!(!state.is_seeking());
        state.set_seeking(true);
        assert!(state.is_seeking());
        state.set_seeking(false);
        assert!(!state.is_seeking());
    }

    #[test]
    fn test_underrun_reporting() {
        let state = RtState::new();

        state.report_underrun();
        state.report_underrun();
        state.report_underrun();
        assert_eq!(state.take_underruns(), 3);
        assert_eq!(state.take_underruns(), 0);

        state.report_underrun();
        assert_eq!(state.take_underruns(), 1);
    }

    #[test]
    fn test_seek_crossfade() {
        let state = RtState::new();

        assert!(!state.is_seek_crossfading());
        assert!(!state.next_seek_crossfade_frame_into(&mut [0.0f32; 2]));

        let fadeout = vec![1.0; 4 * 2];
        let fadein = vec![0.0; 4 * 2];

        state.start_seek_crossfade(fadeout, fadein, 2usize);

        assert!(state.is_seek_crossfading());

        let mut sample = [0.0f32; 2];
        assert!(state.next_seek_crossfade_frame_into(&mut sample));
        assert!((sample[0] - 1.0).abs() < 0.01);

        let mut sample = [0.0f32; 2];
        assert!(state.next_seek_crossfade_frame_into(&mut sample));
        assert!((sample[0] - 0.75).abs() < 0.01);

        let mut sample = [0.0f32; 2];
        assert!(state.next_seek_crossfade_frame_into(&mut sample));
        assert!((sample[0] - 0.5).abs() < 0.01);

        let mut sample = [0.0f32; 2];
        assert!(state.next_seek_crossfade_frame_into(&mut sample));
        assert!((sample[0] - 0.25).abs() < 0.01);

        assert!(!state.is_seek_crossfading());
        assert!(!state.next_seek_crossfade_frame_into(&mut [0.0f32; 2]));
    }

    #[test]
    fn test_buffer_fill_level() {
        let state = RtState::new();

        assert_eq!(state.buffer_fill(), 0.0);

        state.set_buffer_fill(0.5);
        assert!((state.buffer_fill() - 0.5).abs() < 0.01);

        state.set_buffer_fill(1.0);
        assert!((state.buffer_fill() - 1.0).abs() < 0.01);

        state.set_buffer_fill(0.0);
        assert!((state.buffer_fill() - 0.0).abs() < 0.01);

        state.set_buffer_fill(-0.5);
        assert_eq!(state.buffer_fill(), 0.0);

        state.set_buffer_fill(1.5);
        assert!((state.buffer_fill() - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_loop_crossfade() {
        let state = RtState::new();

        assert!(!state.is_loop_crossfading());
        assert!(!state.next_loop_crossfade_frame_into(&mut [0.0f32; 2]));

        let fadeout = vec![1.0; 4 * 2];
        let fadein = vec![0.0; 4 * 2];

        state.start_loop_crossfade(fadeout, fadein, 2usize);

        assert!(state.is_loop_crossfading());

        let mut sample = [0.0f32; 2];
        assert!(state.next_loop_crossfade_frame_into(&mut sample));
        assert!((sample[0] - 1.0).abs() < 0.01);

        let mut sample = [0.0f32; 2];
        assert!(state.next_loop_crossfade_frame_into(&mut sample));
        assert!((sample[0] - 0.75).abs() < 0.01);

        let mut sample = [0.0f32; 2];
        assert!(state.next_loop_crossfade_frame_into(&mut sample));
        assert!((sample[0] - 0.5).abs() < 0.01);

        let mut sample = [0.0f32; 2];
        assert!(state.next_loop_crossfade_frame_into(&mut sample));
        assert!((sample[0] - 0.25).abs() < 0.01);

        assert!(!state.is_loop_crossfading());
        assert!(!state.next_loop_crossfade_frame_into(&mut [0.0f32; 2]));
    }

    #[test]
    fn test_loop_crossfade_clear() {
        let state = RtState::new();

        let fadeout = vec![1.0; 10 * 2];
        let fadein = vec![0.0; 10 * 2];
        state.start_loop_crossfade(fadeout, fadein, 2usize);

        assert!(state.is_loop_crossfading());

        state.next_loop_crossfade_frame_into(&mut [0.0f32; 2]);
        state.next_loop_crossfade_frame_into(&mut [0.0f32; 2]);

        state.clear_loop_crossfade();
        assert!(!state.is_loop_crossfading());
        assert!(!state.next_loop_crossfade_frame_into(&mut [0.0f32; 2]));
    }

    #[test]
    fn test_loop_crossfade_empty_buffers() {
        let state = RtState::new();

        state.start_loop_crossfade(Vec::new(), Vec::new(), 2usize);
        assert!(!state.is_loop_crossfading());

        state.start_loop_crossfade(vec![1.0, 1.0], Vec::new(), 2usize);
        assert!(!state.is_loop_crossfading());
    }
}
