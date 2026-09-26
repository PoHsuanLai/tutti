//! Shared state between butler and audio thread.
//!
//! Every field is atomic or lock-free, so both sides read and publish without a
//! lock ever reaching the audio callback. The fields are grouped into orthogonal
//! sub-structs (playback, health) — one `Arc`, one allocation — each
//! `#[repr(align(64))]` so a butler write to one group cannot false-share a
//! cache line with an audio-thread read of another.
//!
//! # Which side writes what
//!
//! * **Butler (or control) writes, audio reads** — speed, direction,
//!   `src_ratio`, gain, the stretch rate. `src_ratio` is re-derived by the
//!   control thread when the session rate moves (`SessionRate::set`, a device
//!   restart), under the plan's lock, which the butler also holds when it
//!   writes one.
//! * **Audio writes, butler reads** — `underrun_count`; the butler writes
//!   `buffer_fill_level` for its own pacing.
//!
//! Where a stream stands and how to read it is the ring's, not this cell's
//! (`prefetch::Ring`): the reader publishes the position it plays and the
//! butler follows it, so there is nothing to request and nothing to flush.

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use tutti_core::{Amplitude, AtomicF32, AtomicReadRate, PlaybackRate, ReadRate, SrcRatio};

use crate::voice::types::Direction;

/// Varispeed, direction, rate conversion, stretch and gain — the cells the audio
/// thread loads on the per-sample path.
///
/// Cache-line aligned so butler writes here do not false-share with
/// [`BufferHealth`], which the audio thread writes on the same block.
#[repr(align(64))]
pub struct PlaybackParams {
    speed: AtomicF32,
    /// 0 = forward, 1 = reverse.
    direction: AtomicU8,
    /// file_sample_rate / session_sample_rate. 1.0 = no conversion.
    src_ratio: AtomicF32,
    /// Linear output gain, an [`Amplitude`] in the cell.
    ///
    /// Here rather than on `DiskSource` for the reason `tutti_nodes`' crate docs
    /// give: a control stored **by value** in a unit cannot be changed on a live
    /// node, because `Net`'s frontend holds clones and `Net::migrate` discards
    /// edits to them. A plain `Amplitude` field would make a clip's fader do
    /// nothing once its voice existed — silently, with the knob still moving.
    gain: AtomicF32,
    /// Source samples consumed per output sample by a wrapping time-stretcher:
    /// `1 / stretch`. 1.0 when the voice does not stretch.
    ///
    /// Held here rather than in `DiskSource` because the stretch filter and the
    /// ring reader never meet: the filter lives in the `PlaybackSlot`, the reader
    /// behind the butler's `SharedReader`. `RtState` is the cell both already
    /// share, and it is where the other two rate factors compose.
    ///
    /// `AtomicReadRate`, not `AtomicF32`, unlike the two neighbours: those hold
    /// `f32`-backed units, while [`ReadRate`] is `f64` precisely because a disk
    /// voice accumulates it into a read position once per sample. Narrowing it
    /// through the cell would reinstate that drift on every load — the error
    /// does not average out, it integrates.
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

/// Ring occupancy and underrun counts.
///
/// Cache-line aligned: the audio thread writes `buffer_fill_level` and
/// `underrun_count` every block, and that must not evict [`PlaybackParams`]
/// out from under the butler.
#[repr(align(64))]
pub struct BufferHealth {
    /// Underruns since the last [`RtState::take_underruns`]. Audio thread
    /// increments, butler drains.
    underrun_count: AtomicU64,
    /// Ring occupancy as 0-1000, i.e. 0.0-1.0 in thousandths. Fixed-point
    /// because the cell is an integer atomic; the accessors do the scaling.
    buffer_fill_level: AtomicU32,
}

impl Default for BufferHealth {
    fn default() -> Self {
        Self {
            underrun_count: AtomicU64::new(0),
            buffer_fill_level: AtomicU32::new(0),
        }
    }
}

/// The one cell a streaming channel's butler side and audio side both hold.
///
/// Lives behind an `Arc` shared by the [`ChannelPlan`](super::plan::ChannelPlan)
/// and the disk voice, which is why every publisher below takes `&self`: no
/// exclusive reference exists on either side, and none is needed.
///
/// Outlives any single stream — `stop_streaming` resets the fields but keeps the
/// `Arc` alive, because the audio thread may still hold its clone.
pub struct RtState {
    /// Varispeed, direction, conversion ratio, stretch and gain.
    pub playback: PlaybackParams,
    /// Ring occupancy and underruns.
    pub health: BufferHealth,
    // No crossfaders: a loop's fade is written into the ring, and a jump's
    // fade is the reader's own, from the ring (doc 013, "The live disk
    // reposition (after #48)").
}

impl Default for RtState {
    fn default() -> Self {
        Self::new()
    }
}

impl RtState {
    /// A state at rest: unity speed and conversion ratio, unity gain,
    /// forward, all counters zero.
    pub fn new() -> Self {
        Self {
            playback: PlaybackParams::default(),
            health: BufferHealth::default(),
        }
    }

    /// A new state holding this one's playback controls (speed, direction,
    /// gain, conversion ratio, stretch rate) at their current values, and
    /// nothing else: no counters.
    ///
    /// What a disk voice severed for an offline render keeps
    /// (`DiskVoice::isolate`): its controls as a snapshot, like every other
    /// forked unit's, in a cell no butler and no live voice shares.
    pub(crate) fn detached(&self) -> Self {
        let state = Self::new();
        state.set_speed(self.speed());
        state.set_direction(self.direction());
        state.set_gain(self.gain());
        state.set_src_ratio(self.src_ratio());
        state.set_stretch_rate(self.stretch_rate());
        state
    }

    /// The current varispeed. [`PlaybackRate::UNITY`] is normal speed.
    #[inline]
    pub fn speed(&self) -> PlaybackRate {
        PlaybackRate::new(self.playback.speed.load(Ordering::Acquire))
    }

    /// Publish a new varispeed.
    ///
    /// Takes the already-bounded [`PlaybackRate`] rather than a raw `f32` so the
    /// range is enforced where the value is *built*. A clamp applied here
    /// instead would bind only this tier — the in-memory sampler never calls
    /// this setter, so the same command would produce different audio depending
    /// on which tier happened to be playing.
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

    /// The varispeed as the audio thread's per-sample path names it. Identical
    /// to [`speed`](Self::speed); the separate name marks the hot call site
    /// rather than the raw atomic load.
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
    /// Drop the stretch term and it degenerates into varispeed on this tier: a
    /// 2x-stretched disk voice consumes exactly as many source frames as an
    /// unstretched one (512 vs 512, measured), so the vocoder is fed at full
    /// rate and has nothing to spread.
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

    /// Publish the playback direction. The butler reads it each refill cycle to
    /// choose the forward or the reversed refill path.
    pub fn set_direction(&self, direction: Direction) {
        self.playback
            .direction
            .store(u8::from(direction.is_reverse()), Ordering::Release);
    }

    /// Whether playback currently runs backwards through the file.
    #[inline]
    pub fn is_reverse(&self) -> bool {
        self.direction().is_reverse()
    }

    #[cfg(test)]
    pub fn set_reverse(&self, reverse: bool) {
        self.set_direction(Direction::from_reverse(reverse));
    }

    /// Sample-rate conversion ratio: file rate over session rate.
    /// [`SrcRatio::UNITY`] means the file already plays at the session rate.
    #[inline]
    pub fn src_ratio(&self) -> SrcRatio {
        SrcRatio::new(self.playback.src_ratio.load(Ordering::Acquire))
    }

    /// Publish the conversion ratio. Written by the butler when a stream starts,
    /// and re-derived when the session rate moves, from [`SrcRatio::for_rates`]
    /// — the same derivation the in-memory tier uses, so neither tier can pick
    /// the ratio up backwards.
    pub fn set_src_ratio(&self, ratio: SrcRatio) {
        self.playback
            .src_ratio
            .store(ratio.get(), Ordering::Release);
    }

    /// Count one frame the ring could not supply. Audio side: a single relaxed
    /// increment, allocation-free and safe on the hot path.
    #[inline]
    pub fn report_underrun(&self) {
        self.health.underrun_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Read and zero the underrun count, returning the frames missed since the
    /// previous call. Draining rather than peeking is what makes the figure a
    /// per-interval rate instead of an ever-growing total.
    pub fn take_underruns(&self) -> u64 {
        self.health.underrun_count.swap(0, Ordering::Relaxed)
    }

    /// Publish ring occupancy as a fraction, clamped to `0.0..=1.0` and stored
    /// in thousandths. The butler compares it against a refill threshold to
    /// decide both chunk size and whether it may park on a timer.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stretch rate survives the cell bit-for-bit.
    ///
    /// Narrowing it through an `AtomicF32` would make every load return a value
    /// the control thread never wrote. That matters here and not for the
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
        // The clamp lives in `PlaybackRate`, not in this setter, so BOTH
        // playback tiers get it — the in-memory sampler never calls this
        // setter, and a clamp here would leave it accepting out-of-range speeds.
        let state = RtState::new();

        // A fresh cell reads UNITY, not the atomic's zero — which would be
        // silence rather than normal speed, and is what `PlaybackParams`'
        // derived `Default` would give if it stopped spelling this out.
        assert_eq!(state.speed(), PlaybackRate::UNITY);

        state.set_speed(PlaybackRate::new_clamped(0.1));
        assert_eq!(state.speed(), PlaybackRate::MIN);

        state.set_speed(PlaybackRate::new_clamped(10.0));
        assert_eq!(state.speed(), PlaybackRate::MAX);

        state.set_speed(PlaybackRate::new_clamped(2.0));
        assert_eq!(state.speed(), PlaybackRate::new(2.0));
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
}
