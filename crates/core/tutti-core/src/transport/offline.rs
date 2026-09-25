//! Offline transport — a simulated [`super::Timeline`] that advances
//! deterministically by sample count rather than wall clock.
//!
//! Primary consumer is offline audio export, but anything that needs a
//! reproducible timeline without a real CPAL callback (golden tests,
//! automation scrubbing) can use it.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::state::LoopRange;
use crate::{AtomicF64, Ordering};
use crate::{Beat, BeatDuration, Bpm, FrameClock, SampleRate, Samples};

/// The timeline an offline render advances, one block at a time.
///
/// Handed to every node as `&dyn Any` by
/// `PendingClone::isolate_for_offline` (`fundsp-tutti`),
/// so this alias is the agreed shape on both sides of that cast — recover it
/// with `ctx.downcast_ref::<OfflineTransport>()`. It is an alias rather than a
/// named type because `fundsp-tutti` cannot name [`Timeline`](super::Timeline),
/// not because the indirection buys anything.
///
/// # What a node does on rebind
///
/// Nodes holding a transport re-point at this. Nodes carrying their own internal
/// clock re-seat it from [`Timeline::beat`](super::Timeline::beat) and
/// [`Timeline::tempo`](super::Timeline::tempo): `isolate()` severs the live
/// links but leaves the clock at whatever beat the *live* playhead held, so
/// without this every beat-driven node (LFO, automation) renders from an
/// arbitrary position and the output depends on *when* the render started.
/// Read at rebind time, before the renderer has advanced anything, so these are
/// the seeded start values rather than a moving position.
///
/// # No scalars beside the timeline
///
/// It carries **no** `start_beat` or `tempo` of its own. Both are things a
/// timeline already answers, and a copy beside it can disagree — one rebind path
/// reading the scalar while another follows the timeline renders half the graph
/// at one tempo and half at another, silently.
pub type OfflineTransport = Arc<dyn super::Timeline>;

/// Configuration for constructing an [`OfflineTimeline`].
#[derive(Debug, Clone)]
pub struct OfflineTimelineConfig {
    /// Start position in beats.
    pub start_beat: Beat,
    /// Tempo in BPM.
    pub tempo: Bpm,
    /// Sample rate in Hz.
    pub sample_rate: SampleRate,
    /// Loop region, if looping. Already validated — build one with
    /// [`LoopRange::new`], which rejects empty and inverted regions.
    pub loop_range: Option<LoopRange>,
}

impl Default for OfflineTimelineConfig {
    fn default() -> Self {
        Self {
            start_beat: Beat(0.0),
            tempo: Bpm(120.0),
            sample_rate: SampleRate(44100.0),
            loop_range: None,
        }
    }
}

/// Simulated transport that advances by sample count.
///
/// Implements [`Timeline`](super::Timeline), so any node that accepts a
/// `&dyn Timeline` treats it interchangeably with the live
/// [`Transport`](super::Transport).
///
/// # Example
/// ```
/// # use tutti_core::{Beat, Bpm, SampleRate, Timeline};
/// # use tutti_core::transport::{OfflineTimeline, OfflineTimelineConfig};
/// let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
///     start_beat: Beat(0.0),
///     tempo: Bpm(120.0),
///     sample_rate: SampleRate(44100.0),
///     loop_range: None,
/// });
///
/// // Advance by 44100 samples (1 second at 44.1kHz)
/// // At 120 BPM, that's 2 beats
/// timeline.advance(44100);
/// assert!((timeline.beat().get() - 2.0).abs() < 0.001);
/// ```
///
/// # The frame is the source of truth
///
/// The playhead is a frame count on a segment (doc 013 §6), and the beat is
/// derived from it in closed form, never accumulated: at 90 BPM and 48 kHz,
/// 1 500 blocks of 64 frames is beat 3 to the bit, where adding
/// `beats_per_sample × 64` per block reads `2.999999999999891` and a clip
/// placed at beat 3 enters a block late. A loop wrap starts a new segment on
/// the frame that reaches the loop's end, as the live `TransportClock` does,
/// so the two agree to the bit however each is stepped.
///
/// # Shared vs. fixed state
///
/// Only the position is shared: `advance`/`seek_to` take `&self` because the
/// timeline is held as an `Arc` and read by clip readers and samplers while
/// the export driver advances it. Everything else is render configuration
/// fixed at construction — there are no setters — so it is a plain value.
///
/// The position is a [`FrameClock`] behind a `Mutex`: a move (`advance`,
/// `seek_to`) takes the lock, moves the clock, and publishes the beat it
/// derives before letting go, so two writers racing (a seek against an
/// advance) serialise, and neither can publish a position the other half
/// wrote. Readers read the published beat alone, lock-free. An offline
/// render, not the audio thread, is what takes the lock.
///
/// The alignment keeps the published beat — the one genuinely contended word
/// — on its own cache line, away from the immutable fields readers also
/// touch.
#[derive(Debug)]
#[repr(align(64))]
pub struct OfflineTimeline {
    /// The beat `clock` derives, published on every move: what readers
    /// read.
    current_beat: AtomicF64,
    /// The playhead: the only writer-side state, moved under its lock.
    clock: Mutex<FrameClock>,
    tempo: Bpm,
    sample_rate: SampleRate,
    /// Musical time per sample, precomputed from `tempo` and `sample_rate`.
    beats_per_sample: BeatDuration,
    /// The active loop region, validated at construction — so `advance()` needs
    /// no `end > start` guard of its own.
    loop_range: Option<LoopRange>,
}

impl OfflineTimeline {
    /// Build a timeline seated at `config.start_beat`, precomputing the
    /// per-sample beat increment from its tempo and sample rate.
    pub fn new(config: &OfflineTimelineConfig) -> Self {
        Self {
            current_beat: AtomicF64::new(config.start_beat.get()),
            clock: Mutex::new(FrameClock::new(
                config.start_beat,
                config.tempo,
                config.sample_rate,
            )),
            tempo: config.tempo,
            sample_rate: config.sample_rate,
            beats_per_sample: super::state::beats_per_sample(config.tempo, config.sample_rate),
            loop_range: config.loop_range,
        }
    }

    /// Advance the playhead by `samples` **frames** of render.
    ///
    /// If a loop region is set and the timeline crosses its end, the position
    /// wraps back into the region, on the frame that reaches the end.
    ///
    /// Counts frames and derives the beat in closed form (the type docs), with
    /// the live `TransportClock`'s own code, so the two land on the same beat
    /// to the bit whether either is stepped a frame or a block at a time —
    /// see the sample-for-sample test below.
    pub fn advance(&self, samples: usize) {
        let mut clock = self.lock();
        // `LoopRange` is non-empty by construction, so no guard. Only a
        // crossing wraps, as for the clock (`LoopRange::advance`).
        clock.advance(Samples(samples), self.loop_range);
        self.publish(&clock);
    }

    /// The playhead, for moving: held until the moved position is published.
    fn lock(&self) -> MutexGuard<'_, FrameClock> {
        // A panic mid-move leaves a whole `FrameClock` (it is `Copy` and
        // moved by value), so a poisoned lock still holds a position.
        self.clock.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Publish `clock`'s beat to readers. Called with the lock held.
    fn publish(&self, clock: &FrameClock) {
        self.current_beat
            .store(clock.beat().get(), Ordering::Release);
    }

    /// The current playhead.
    #[inline]
    pub fn beat(&self) -> Beat {
        Beat(self.current_beat.load(Ordering::Acquire))
    }

    /// The render tempo. Fixed at construction — an offline render does not
    /// ramp.
    #[inline]
    pub fn tempo(&self) -> Bpm {
        self.tempo
    }

    /// The render sample rate. Fixed at construction.
    #[inline]
    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// Move the playhead to `beat`, for reusing one timeline across several
    /// renders.
    ///
    /// Named for the seek, not for a reset: it clears nothing, and `beat` is a
    /// destination rather than a default. Everything else on this timeline —
    /// tempo, sample rate, loop region — is fixed at construction.
    pub fn seek_to(&self, beat: impl Into<Beat>) {
        let mut clock = self.lock();
        clock.seat(beat.into());
        self.publish(&clock);
    }

    /// Musical time one frame covers, precomputed at construction.
    #[inline]
    pub fn beats_per_sample(&self) -> BeatDuration {
        self.beats_per_sample
    }

    /// The active loop region, or `None` when not looping.
    ///
    /// An inherent method, not a [`Timeline`](super::Timeline) one: looping is a
    /// live-transport concept ([`TransportState`](super::TransportState)), and
    /// the offline render never reads it through a trait — `advance()` folds the
    /// wrap in directly.
    #[inline]
    pub fn loop_range(&self) -> Option<LoopRange> {
        self.loop_range
    }

    /// The block about to be rendered, as a native graph executor takes it:
    /// the transport at the block's first frame, and the changes inside it.
    ///
    /// The transport is this timeline at its current playhead: rolling (an
    /// offline render always is), at its tempo, looping over its region, and
    /// counted from its segment's origin, so an `EnvClock` in the graph
    /// continues this timeline's arithmetic. There are never changes: the
    /// tempo and the loop are fixed for the render, and a loop wrap inside the block is not a change: the graph
    /// derives it from the snapshot, as it does live
    /// ([`Env::transport_at`](tutti_graph::Env::transport_at), and
    /// [`EnvClock`](super::EnvClock) frame by frame). A render with a
    /// tempo map would put its tempo steps here.
    ///
    /// Read **before** the block is processed and advance after, as
    /// [`render_graph`](Self::render_graph) does: the snapshot's beat is the
    /// block's first frame, the one clip readers and samplers holding this
    /// timeline read during the block (see [`RenderClock`](super::RenderClock)).
    pub fn graph_block(&self) -> (tutti_graph::Transport, tutti_graph::TransportChanges) {
        let origin = self.lock().origin();
        let transport = tutti_graph::Transport::counted(
            true,
            self.tempo,
            origin,
            self.loop_range.map(|r| tutti_graph::LoopRange {
                start: r.start(),
                end: r.end(),
            }),
        );
        (transport, tutti_graph::TransportChanges::NONE)
    }

    /// Render one block of `frames` through `exec` under this timeline, then
    /// advance the timeline by it: [`graph_block`](Self::graph_block),
    /// [`Executor::process_with_changes`](tutti_graph::Executor::process_with_changes),
    /// then [`advance`](Self::advance), in the one order that keeps every
    /// reader of this timeline on the frame the graph renders — once per
    /// 64-frame chunk while the graph holds a `Legacy` unit (see
    /// [`RenderClock::render_graph`](super::RenderClock::render_graph)), so
    /// a clip reader polling this timeline reads the positions a `Net`
    /// render's 64-frame `advance`s give it, to the bit.
    ///
    /// The graph's frames and this timeline's beats both start where they
    /// stand: the executor keeps its own frame clock, and the beat is this
    /// timeline's playhead (seat it with [`seek_to`](Self::seek_to)).
    ///
    /// # Panics
    ///
    /// As `Executor::process_with_changes`: if `frames` is zero or past the
    /// executor's prepared maximum block.
    pub fn render_graph(
        &self,
        exec: &mut tutti_graph::Executor,
        frames: usize,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
    ) {
        // The trait's, so an export driving any `RenderClock` and a caller
        // holding this timeline run the one sequence.
        super::RenderClock::render_graph(self, exec, frames, inputs, outputs);
    }
}

impl super::Timeline for OfflineTimeline {
    fn beat(&self) -> Beat {
        self.beat()
    }

    fn tempo(&self) -> Bpm {
        self.tempo()
    }

    fn is_rolling(&self) -> bool {
        // An offline timeline advances whenever asked — there is nothing to
        // pause it.
        true
    }
}

impl super::RenderClock for OfflineTimeline {
    fn advance(&self, frames: tutti_types::Samples) {
        // The inherent `advance` takes a raw count; this is the same call with
        // the frame-count type at the trait boundary.
        OfflineTimeline::advance(self, frames.get());
    }

    fn graph_block(&self) -> (tutti_graph::Transport, tutti_graph::TransportChanges) {
        OfflineTimeline::graph_block(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An inverted region must not arm a loop that never wraps.
    ///
    /// Carried as an unvalidated pair plus an `enabled` flag, `end < start`
    /// arms the loop and then silently never wraps — the render runs straight
    /// past the loop end with no diagnostic. `Option<LoopRange>` makes that
    /// state unrepresentable: it is rejected at the boundary and the timeline
    /// is honestly un-looped.
    #[test]
    fn an_inverted_loop_region_is_rejected_not_silently_ignored() {
        let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: Beat(0.0),
            tempo: Bpm(120.0),
            sample_rate: SampleRate(44100.0),
            loop_range: LoopRange::new(8.0, 4.0),
        });

        assert_eq!(
            timeline.loop_range(),
            None,
            "an inverted region must not report as an active loop"
        );

        // ...and the timeline runs free rather than pretending to loop.
        let samples_per_beat = 44100.0 / 2.0;
        timeline.advance((10.0 * samples_per_beat) as usize);
        assert!(
            (timeline.beat().get() - 10.0).abs() < 0.01,
            "expected free-running beat 10.0, got {}",
            timeline.beat().get()
        );
    }

    /// The empty region — the case a `loop_length > 0.0` guard inside `advance`
    /// would catch. `LoopRange` catches it one layer earlier, at construction.
    #[test]
    fn an_empty_loop_region_is_rejected() {
        let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: Beat(0.0),
            tempo: Bpm(120.0),
            sample_rate: SampleRate(44100.0),
            loop_range: LoopRange::new(4.0, 4.0),
        });
        assert_eq!(timeline.loop_range(), None);
    }

    /// A block longer than the loop lands where its frames do: inside the
    /// loop, exactly on the start when the crossings are on frames, and bit
    /// for bit where the same frames one at a time land when they are not.
    ///
    /// Renamed from `advance_wraps_once_per_block_not_once_per_sample`, whose
    /// premise was the accumulating clock's: a per-sample walk wrapped
    /// repeatedly and drifted, so `advance` added a block in bulk and wrapped
    /// once. `advance` now counts frames and starts a new segment on every
    /// crossing's frame (`FrameClock::advance`), as the live clock does frame
    /// by frame, so the two agree to the bit.
    ///
    /// Mutation (run): wrap once per call, at the call's end (the old bulk
    /// rule) → the block of 50 000 frames (five crossings of a 0.33-beat loop)
    /// lands off the frame-by-frame position → fails.
    #[test]
    fn a_block_longer_than_the_loop_lands_where_its_frames_do() {
        let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: Beat(0.0),
            tempo: Bpm(120.0),
            sample_rate: SampleRate(44100.0),
            loop_range: LoopRange::new(0.0, 1.0),
        });

        // Five beats over a one-beat loop: a crossing every 22 050 frames, each
        // on a frame (one beat is exactly 22 050), so each wraps to 0.0.
        let samples_per_beat = 22050usize;
        timeline.advance(5 * samples_per_beat);
        assert_eq!(
            timeline.beat(),
            Beat(0.0),
            "5 beats over a 1-beat loop land exactly on the start"
        );

        // Crossings between frames, at a tempo whose frame step is not
        // representable: a block at once and its frames one by one agree.
        let config = OfflineTimelineConfig {
            start_beat: Beat(0.1),
            tempo: Bpm(97.0),
            sample_rate: SampleRate(44100.0),
            loop_range: LoopRange::new(0.1, 0.43),
        };
        let (block, frames) = (OfflineTimeline::new(&config), OfflineTimeline::new(&config));
        block.advance(50_000);
        for _ in 0..50_000 {
            frames.advance(1);
        }
        assert!(config.loop_range.expect("a loop").contains(block.beat()));
        assert_eq!(block.beat().get().to_bits(), frames.beat().get().to_bits());
    }

    /// A region render drives BOTH clocks over the same net: the in-net
    /// `TransportClock` feeds beat-input nodes (LFO, AutomationLaneNode) while this
    /// `OfflineTimeline` feeds clip readers and samplers. Started at the same
    /// beat, they must report the same beat for the same sample.
    ///
    /// The order is emit-then-advance. A driver that primes with `advance(1)`
    /// before the first block — "advance-then-tick semantics" — puts the two
    /// clocks exactly one `beats_per_sample` apart for the entire render, which
    /// reads as "the samplers are slightly late" and nothing else.
    #[test]
    fn offline_timeline_agrees_with_transport_clock_sample_for_sample() {
        use crate::transport::TransportClock;
        use crate::{AtomicBool, AtomicF64, AudioUnit};
        use std::sync::Arc;

        let sample_rate = 44100.0;
        let tempo = 120.0;
        let start_beat = Beat(4.0);

        let mut clock = TransportClock::new(
            crate::transport::ClockLinks::bare(
                Arc::new(AtomicF64::new(tempo)),
                Arc::new(AtomicBool::new(false)),
            ),
            sample_rate,
        )
        .starting_at(start_beat);

        let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat,
            tempo: Bpm(tempo),
            sample_rate: SampleRate(sample_rate),
            loop_range: None,
        });

        // Sample 0: both must report the start beat, before either advances.
        let mut out = [0.0f32; 2];
        clock.tick(&[], &mut out);
        let clock_beat = out[0] as f64 + out[1] as f64;
        assert!(
            (clock_beat - timeline.beat().get()).abs() < 1e-9,
            "first sample disagrees: clock={clock_beat} timeline={}",
            timeline.beat().get()
        );

        // And they must stay in step across a block boundary. The driver ticks
        // the net per sample, then advances the timeline by the block size.
        let block = 512;
        for _ in 1..block {
            clock.tick(&[], &mut out);
        }
        timeline.advance(block);

        let clock_beat = out[0] as f64 + out[1] as f64;
        let expected_lag = timeline.beats_per_sample();
        // After the block the timeline sits one sample ahead of the last
        // EMITTED sample, because emit-then-advance means sample N-1 carried
        // the beat before the final increment.
        assert!(
            ((timeline.beat().get() - clock_beat) - expected_lag.get()).abs() < 1e-9,
            "drifted across the block: clock={clock_beat} timeline={} \
             (expected exactly one beats_per_sample apart)",
            timeline.beat().get()
        );
    }

    /// The reviewer's case: at 90 BPM / 48 kHz, 1 500 blocks of 64 frames
    /// is beat 3 and 500 is beat 1, to the bit. An accumulated playhead
    /// (`beat += beats_per_sample × 64` per block) read `2.999999999999891`
    /// and `1.0000000000000007`.
    ///
    /// Mutation (run): accumulate in `FrameClock::advance` → 1.0000000000000007
    /// → fails.
    #[test]
    fn a_block_on_a_beat_is_that_beat_exactly() {
        let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: Beat(0.0),
            tempo: Bpm(90.0),
            sample_rate: SampleRate(48_000.0),
            loop_range: None,
        });
        for _ in 0..500 {
            timeline.advance(64);
        }
        assert_eq!(timeline.beat(), Beat(1.0));
        for _ in 500..1_500 {
            timeline.advance(64);
        }
        assert_eq!(timeline.beat(), Beat(3.0));
    }

    /// After N blocks of arbitrary lengths, at arbitrary tempos and rates,
    /// both clocks of a render — this timeline, advanced a block at a time,
    /// and a `TransportClock`, processed 64 frames a call as a `Net` runs it
    /// — stand on the closed form `start + frames × tempo / (60 × rate)`, to
    /// the bit. Correctly rounded `*`, `/` and `+` only (no libm), so the
    /// comparison is portable.
    ///
    /// Mutation (run): accumulate in `FrameClock::advance` → the first
    /// tempo whose per-frame step is not representable fails.
    #[test]
    fn a_long_render_stands_on_the_closed_form() {
        use crate::transport::TransportClock;
        use crate::{AtomicBool, AtomicF64, AudioUnit, BufferRef};
        use fundsp::prelude::{BufferArray, U2};

        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..20 {
            let tempo = 40.0 + (next() % 20_000) as f64 / 100.0;
            let rate = [44_100.0, 48_000.0, 96_000.0][(next() % 3) as usize];
            let start = (next() % 64) as f64 / 3.0;
            let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
                start_beat: Beat(start),
                tempo: Bpm(tempo),
                sample_rate: SampleRate(rate),
                loop_range: None,
            });
            let mut clock = TransportClock::new(
                crate::transport::ClockLinks::bare(
                    Arc::new(AtomicF64::new(tempo)),
                    Arc::new(AtomicBool::new(false)),
                ),
                rate,
            )
            .starting_at(start);
            let empty = BufferRef::new(&[]);
            let mut scratch = BufferArray::<U2>::new();
            let mut frames = 0u64;
            for _ in 0..1_000 {
                let n = 1 + (next() % 2_048) as usize;
                timeline.advance(n);
                frames += n as u64;
            }
            for _ in 0..frames / 64 {
                clock.process(64, &empty, &mut scratch.buffer_mut());
            }
            clock.process((frames % 64) as usize, &empty, &mut scratch.buffer_mut());
            let closed = start + (frames as f64 * tempo) / (60.0 * rate);
            assert_eq!(
                timeline.beat().get().to_bits(),
                closed.to_bits(),
                "the timeline, {tempo} BPM at {rate} Hz, {frames} frames"
            );
            assert_eq!(
                clock.current_beat().get().to_bits(),
                closed.to_bits(),
                "the clock, {tempo} BPM at {rate} Hz, {frames} frames"
            );
        }
    }

    #[test]
    fn test_timeline_seek_to() {
        let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: Beat(0.0),
            tempo: Bpm(120.0),
            sample_rate: SampleRate(44100.0),
            loop_range: None,
        });

        let samples_per_beat = 44100.0 / 2.0;
        timeline.advance((5.0 * samples_per_beat) as usize);
        assert!((timeline.beat().get() - 5.0).abs() < 0.01);

        // Seek back to beat 2
        timeline.seek_to(2.0);
        assert!((timeline.beat().get() - 2.0).abs() < 0.001);
    }

    #[test]
    fn timeline_impl_reports_the_loop_region() {
        let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: Beat(0.0),
            tempo: Bpm(120.0),
            sample_rate: SampleRate(44100.0),
            loop_range: LoopRange::new(0.0, 8.0),
        });

        use crate::Timeline;

        assert_eq!(timeline.loop_range(), LoopRange::new(0.0, 8.0));
        assert_eq!(timeline.tempo().get(), 120.0);
        // An offline timeline has nothing to pause it.
        assert!(timeline.is_rolling());
    }

    #[test]
    fn timeline_impl_reports_no_loop_when_unset() {
        let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: Beat(0.0),
            tempo: Bpm(120.0),
            sample_rate: SampleRate(44100.0),
            loop_range: None,
        });

        assert_eq!(timeline.loop_range(), None);
    }
}
