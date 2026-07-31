//! Offline transport — a simulated [`super::Timeline`] that advances
//! deterministically by sample count rather than wall clock.
//!
//! Primary consumer is offline audio export, but anything that needs a
//! reproducible timeline without a real CPAL callback (golden tests,
//! automation scrubbing) can use it.

use std::sync::Arc;

use super::state::LoopRange;
use crate::params::{Beat, BeatDuration, Bpm, SampleRate};
use crate::{AtomicF64, Ordering};

/// The timeline an offline render advances, one block at a time.
///
/// Handed to every node as `&dyn Any` by
/// [`PendingClone::isolate_for_offline`](crate::dsp::PendingClone::isolate_for_offline),
/// so this alias is the agreed shape on both sides of that cast — recover it
/// with `ctx.downcast_ref::<OfflineTransport>()`. It is an alias rather than a
/// named type because `fundsp-tutti` cannot name [`Timeline`](super::Timeline),
/// not because the indirection buys anything.
///
/// Nodes holding a transport re-point at this. Nodes carrying their own internal
/// clock re-seat it from [`Timeline::beat`](super::Timeline::beat) and
/// [`Timeline::tempo`](super::Timeline::tempo): `isolate()` severs the live
/// links but leaves the clock at whatever beat the *live* playhead happened to
/// be at, so without this every beat-driven node (LFO, automation) would render
/// from an arbitrary position. Read at rebind time — before the renderer has
/// advanced anything — so those are the seeded start values, not a moving
/// position.
///
/// This carried `start_beat` and `tempo` as separate fields once. Both are
/// things a timeline already answers, so the copies could disagree with it, and
/// two rebind paths read different ones — `TransportClock` took the scalars
/// while `MemorySource` followed the transport. A mismatch rendered half the
/// graph at one tempo and half at another, silently.
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
/// Implements [`super::Timeline`], so any node that accepts a
/// `&dyn Timeline` treats it interchangeably with the live
/// [`super::TransportHandle`].
///
/// # Example
/// ```ignore
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
/// # Shared vs. fixed state
///
/// Only `current_beat` is shared: `advance`/`reset` take `&self` because the
/// timeline is held as an `Arc` and read by clip readers and samplers while the
/// export driver advances it. Everything else is render configuration fixed at
/// construction — there are no setters — so it is a plain value. As atomics
/// they cost an `Acquire` load per read on `advance()` and put nothing on the
/// other end of the release/acquire edge.
///
/// The alignment keeps `current_beat` — the one genuinely contended word — on
/// its own cache line, away from the immutable fields readers also touch.
#[derive(Debug)]
#[repr(align(64))]
pub struct OfflineTimeline {
    /// Current position. The only mutable, shared field.
    current_beat: AtomicF64,
    tempo: Bpm,
    sample_rate: SampleRate,
    /// Musical time per sample, precomputed from `tempo` and `sample_rate`.
    beats_per_sample: BeatDuration,
    /// The active loop region, validated at construction — so `advance()` needs
    /// no `end > start` guard of its own.
    loop_range: Option<LoopRange>,
}

impl OfflineTimeline {
    pub fn new(config: &OfflineTimelineConfig) -> Self {
        Self {
            current_beat: AtomicF64::new(config.start_beat.get()),
            tempo: config.tempo,
            sample_rate: config.sample_rate,
            beats_per_sample: super::state::beats_per_sample(config.tempo, config.sample_rate),
            loop_range: config.loop_range,
        }
    }

    /// Advance the timeline by the given number of samples.
    ///
    /// If a loop region is set and the timeline crosses its end, the position
    /// wraps back into the region.
    ///
    /// The increment is added in **bulk** for the whole block and wrapped once,
    /// not accumulated per sample. That is what keeps this timeline
    /// bit-identical to the in-net `TransportClock` for the unlooped case every
    /// production render uses — see the sample-for-sample test below.
    pub fn advance(&self, samples: usize) {
        let mut beat = Beat(self.current_beat.load(Ordering::Acquire));
        beat += self.beats_per_sample * samples as f64;

        if let Some(region) = self.loop_range {
            // `LoopRange` is non-empty by construction, so `wrap` needs no
            // guard — the same reason `TransportClock` needs none.
            beat = region.wrap(beat);
        }

        self.current_beat.store(beat.get(), Ordering::Release);
    }

    #[inline]
    pub fn beat(&self) -> Beat {
        Beat(self.current_beat.load(Ordering::Acquire))
    }

    #[inline]
    pub fn tempo(&self) -> Bpm {
        self.tempo
    }

    #[inline]
    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    pub fn reset(&self, start_beat: impl Into<Beat>) {
        self.current_beat
            .store(start_beat.into().get(), Ordering::Release);
    }

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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: the config used to carry an unvalidated `(f64, f64)`. An
    /// inverted pair produced `loop_enabled: true` with `end < start`, so
    /// `advance` armed the loop and then silently never wrapped — the render ran
    /// straight past the loop end with no diagnostic. `Option<LoopRange>` makes
    /// that state unrepresentable: it is rejected at the boundary and the
    /// timeline is honestly un-looped.
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

    /// The empty region is what the deleted `loop_length > 0.0` guard existed to
    /// catch. `LoopRange` catches it one layer earlier.
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

    /// `advance` adds the whole block's beats at once and wraps ONCE, rather
    /// than accumulating and wrapping per sample. For a loop shorter than one
    /// block the two differ — a per-sample walk would wrap repeatedly and land
    /// elsewhere. This is a lock, not a new assertion: it passes identically
    /// before and after the refactor, and exists so a future "simplification"
    /// into a per-sample loop fails loudly.
    #[test]
    fn advance_wraps_once_per_block_not_once_per_sample() {
        let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: Beat(0.0),
            tempo: Bpm(120.0),
            sample_rate: SampleRate(44100.0),
            loop_range: LoopRange::new(0.0, 1.0),
        });

        // Five beats over a one-beat loop. Bulk: `5.0 % 1.0` → 0.0, the modulo
        // absorbing all four intervening crossings at once.
        let samples_per_beat = 22050usize;
        timeline.advance(5 * samples_per_beat);

        let beat = timeline.beat().get();
        assert!(
            (0.0..1.0).contains(&beat),
            "must land inside the region, got {beat}"
        );
        assert!(
            beat.abs() < 1e-9,
            "5 beats over a 1-beat loop must land exactly at the start, got {beat}"
        );
    }

    /// A region render drives BOTH clocks over the same net: the in-net
    /// `TransportClock` feeds beat-input nodes (LFO, AutomationLane) while this
    /// `OfflineTimeline` feeds clip readers and samplers. Started at the same
    /// beat, they must report the same beat for the same sample.
    ///
    /// Regression: the export driver used to `advance(1)` before the first
    /// block, justified as matching "advance-then-tick semantics". The clock is
    /// emit-then-advance, so that prime put the two exactly one
    /// `beats_per_sample` apart for the entire render.
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

    #[test]
    fn test_timeline_advances() {
        let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: Beat(0.0),
            tempo: Bpm(120.0),
            sample_rate: SampleRate(44100.0),
            loop_range: None,
        });

        // At 120 BPM, 2 beats/second, 44100 samples/second
        // So 22050 samples = 1 beat
        timeline.advance(22050);
        assert!((timeline.beat().get() - 1.0).abs() < 0.001);

        timeline.advance(22050);
        assert!((timeline.beat().get() - 2.0).abs() < 0.001);
    }

    #[test]
    fn test_timeline_loop_wrap() {
        let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: Beat(0.0),
            tempo: Bpm(120.0),
            sample_rate: SampleRate(44100.0),
            loop_range: LoopRange::new(0.0, 4.0),
        });

        // 4 beats at 120 BPM = 2 seconds = 88200 samples
        let samples_per_beat = 44100.0 / 2.0;

        // Advance to beat 3
        timeline.advance((3.0 * samples_per_beat) as usize);
        assert!((timeline.beat().get() - 3.0).abs() < 0.01);

        // Advance 2 more beats - should wrap to beat 1
        timeline.advance((2.0 * samples_per_beat) as usize);
        assert!((timeline.beat().get() - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_timeline_no_loop() {
        let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: Beat(0.0),
            tempo: Bpm(120.0),
            sample_rate: SampleRate(44100.0),
            loop_range: None,
        });

        let samples_per_beat = 44100.0 / 2.0;

        // Advance past where loop end would be
        timeline.advance((10.0 * samples_per_beat) as usize);
        assert!((timeline.beat().get() - 10.0).abs() < 0.01);
    }

    #[test]
    fn test_timeline_start_offset() {
        let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: Beat(4.0),
            tempo: Bpm(120.0),
            sample_rate: SampleRate(44100.0),
            loop_range: None,
        });

        assert!((timeline.beat().get() - 4.0).abs() < 0.001);

        let samples_per_beat = 44100.0 / 2.0;
        timeline.advance(samples_per_beat as usize);
        assert!((timeline.beat().get() - 5.0).abs() < 0.01);
    }

    #[test]
    fn test_timeline_reset() {
        let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: Beat(0.0),
            tempo: Bpm(120.0),
            sample_rate: SampleRate(44100.0),
            loop_range: None,
        });

        let samples_per_beat = 44100.0 / 2.0;
        timeline.advance((5.0 * samples_per_beat) as usize);
        assert!((timeline.beat().get() - 5.0).abs() < 0.01);

        // Reset to beat 2
        timeline.reset(2.0);
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
