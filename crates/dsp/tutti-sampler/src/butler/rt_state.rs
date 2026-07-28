//! Shared state between butler and audio thread.
//!
//! All fields are atomic or lock-free. Organized into orthogonal sub-structs
//! (playback, health, seek/loop crossfade) — one `Arc`, one allocation, but
//! fields are grouped by concern to reduce false sharing and make the
//! ownership story obvious.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use tutti_core::{AtomicF32, PlaybackRate, ReadRate, SrcRatio};

use super::crossfader::StreamingCrossfader;
use crate::voice::voice_pool::Direction;

/// Playback parameters read by the audio thread every sample.
#[repr(align(64))]
pub struct PlaybackParams {
    speed: AtomicF32,
    /// 0 = forward, 1 = reverse.
    direction: AtomicU8,
    /// file_sample_rate / session_sample_rate. 1.0 = no conversion.
    src_ratio: AtomicF32,
}

impl Default for PlaybackParams {
    fn default() -> Self {
        Self {
            speed: AtomicF32::new(1.0),
            direction: AtomicU8::new(0),
            src_ratio: AtomicF32::new(1.0),
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

    /// Current playback speed. Same as [`speed`] — kept distinct from the
    /// raw atomic load for the audio-thread call site which reads it every
    /// sample.
    #[inline]
    pub fn effective_speed(&self) -> PlaybackRate {
        self.speed()
    }

    /// Source samples consumed per output sample: varispeed × conversion.
    ///
    /// The streaming twin of `MemorySource::read_rate`, composing through the
    /// same [`PlaybackRate::read_rate`] so neither tier can drop a factor or
    /// swap the pair.
    #[inline]
    pub fn read_rate(&self) -> ReadRate {
        self.speed().read_rate(self.src_ratio())
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

    pub fn start_seek_crossfade(&self, fadeout: Vec<f32>, fadein: Vec<f32>, channels: usize) {
        self.seek_crossfade.start(fadeout, fadein, channels);
    }

    #[inline]
    pub fn is_seek_crossfading(&self) -> bool {
        self.seek_crossfade.is_active()
    }

    pub fn next_seek_crossfade_frame_into(&self, out: &mut [f32]) -> bool {
        self.seek_crossfade.next_frame_into(out)
    }

    pub fn start_loop_crossfade(&self, fadeout: Vec<f32>, fadein: Vec<f32>, channels: usize) {
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

        state.start_seek_crossfade(fadeout, fadein, 2);

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

        state.start_loop_crossfade(fadeout, fadein, 2);

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
        state.start_loop_crossfade(fadeout, fadein, 2);

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

        state.start_loop_crossfade(Vec::new(), Vec::new(), 2);
        assert!(!state.is_loop_crossfading());

        state.start_loop_crossfade(vec![1.0, 1.0], Vec::new(), 2);
        assert!(!state.is_loop_crossfading());
    }
}
