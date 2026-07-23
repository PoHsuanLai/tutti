//! Offline transport — a simulated [`super::TransportClockRead`] that advances
//! deterministically by sample count rather than wall clock.
//!
//! Primary consumer is offline audio export, but anything that needs a
//! reproducible timeline without a real CPAL callback (golden tests,
//! automation scrubbing) can use it.

use crate::params::{Bpm, SampleRate};
use crate::{AtomicBool, AtomicF64, Ordering};

/// Configuration for constructing an [`OfflineTransport`].
#[derive(Debug, Clone)]
pub struct OfflineTransportConfig {
    /// Start position in beats.
    pub start_beat: f64,
    /// Tempo in BPM.
    pub tempo: Bpm,
    /// Sample rate in Hz.
    pub sample_rate: SampleRate,
    /// Loop range (start, end) in beats, if looping.
    pub loop_range: Option<(f64, f64)>,
}

impl Default for OfflineTransportConfig {
    fn default() -> Self {
        Self {
            start_beat: 0.0,
            tempo: Bpm(120.0),
            sample_rate: SampleRate(44100.0),
            loop_range: None,
        }
    }
}

/// Simulated transport that advances by sample count.
///
/// Implements [`super::TransportClockRead`], so any node that accepts a
/// `&dyn TransportClockRead` treats it interchangeably with the live
/// [`super::TransportHandle`].
///
/// # Example
/// ```ignore
/// let timeline = OfflineTransport::new(&OfflineTransportConfig {
///     start_beat: 0.0,
///     tempo: Bpm(120.0),
///     sample_rate: SampleRate(44100.0),
///     loop_range: None,
/// });
///
/// // Advance by 44100 samples (1 second at 44.1kHz)
/// // At 120 BPM, that's 2 beats
/// timeline.advance(44100);
/// assert!((timeline.current_beat() - 2.0).abs() < 0.001);
/// ```
#[derive(Debug)]
#[repr(align(64))]
pub struct OfflineTransport {
    /// Current position in beats.
    current_beat: AtomicF64,
    /// Tempo in BPM.
    tempo: AtomicF64,
    /// Sample rate in Hz.
    sample_rate: f64,
    /// Beats per sample (precomputed for efficiency).
    beats_per_sample: f64,
    /// Loop start in beats.
    loop_start: AtomicF64,
    /// Loop end in beats.
    loop_end: AtomicF64,
    /// Whether loop is enabled.
    loop_enabled: AtomicBool,
}

impl OfflineTransport {
    pub fn new(config: &OfflineTransportConfig) -> Self {
        let tempo_raw = config.tempo.get();
        let sr_raw = config.sample_rate.get();
        let beats_per_second = tempo_raw / 60.0;
        let beats_per_sample = beats_per_second / sr_raw;

        let (loop_start, loop_end, loop_enabled) = match config.loop_range {
            Some((start, end)) => (start, end, true),
            None => (0.0, 0.0, false),
        };

        Self {
            current_beat: AtomicF64::new(config.start_beat),
            tempo: AtomicF64::new(tempo_raw),
            sample_rate: sr_raw,
            beats_per_sample,
            loop_start: AtomicF64::new(loop_start),
            loop_end: AtomicF64::new(loop_end),
            loop_enabled: AtomicBool::new(loop_enabled),
        }
    }

    /// Advance the timeline by the given number of samples.
    ///
    /// If loop is enabled and the timeline crosses the loop end,
    /// it will wrap back to the loop start.
    pub fn advance(&self, samples: usize) {
        let mut beat = self.current_beat.load(Ordering::Acquire);
        beat += samples as f64 * self.beats_per_sample;

        // Handle loop wrap
        if self.loop_enabled.load(Ordering::Acquire) {
            let loop_start = self.loop_start.load(Ordering::Acquire);
            let loop_end = self.loop_end.load(Ordering::Acquire);

            if beat >= loop_end {
                let loop_length = loop_end - loop_start;
                if loop_length > 0.0 {
                    beat = loop_start + ((beat - loop_start) % loop_length);
                }
            }
        }

        self.current_beat.store(beat, Ordering::Release);
    }

    #[inline]
    pub fn current_beat(&self) -> f64 {
        self.current_beat.load(Ordering::Acquire)
    }

    #[inline]
    pub fn tempo(&self) -> Bpm {
        Bpm(self.tempo.load(Ordering::Acquire))
    }

    #[inline]
    pub fn sample_rate(&self) -> SampleRate {
        SampleRate(self.sample_rate)
    }

    pub fn reset(&self, start_beat: f64) {
        self.current_beat.store(start_beat, Ordering::Release);
    }

    #[inline]
    pub fn beats_per_sample(&self) -> f64 {
        self.beats_per_sample
    }
}

impl super::TransportClockRead for OfflineTransport {
    fn current_beat(&self) -> f64 {
        self.current_beat.load(Ordering::Acquire)
    }

    fn is_loop_enabled(&self) -> bool {
        self.loop_enabled.load(Ordering::Acquire)
    }

    fn get_loop_range(&self) -> Option<(f64, f64)> {
        self.loop_enabled.load(Ordering::Acquire).then(|| {
            (
                self.loop_start.load(Ordering::Acquire),
                self.loop_end.load(Ordering::Acquire),
            )
        })
    }

    fn is_playing(&self) -> bool {
        // Offline transport is always "playing" — it advances whenever asked.
        true
    }

    fn is_recording(&self) -> bool {
        false
    }

    fn is_in_preroll(&self) -> bool {
        false
    }

    fn tempo(&self) -> Bpm {
        Bpm(self.tempo.load(Ordering::Acquire))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A region render drives BOTH clocks over the same net: the in-net
    /// `TransportClock` feeds beat-input nodes (LFO, AutomationLane) while this
    /// `OfflineTransport` feeds clip readers and samplers. Started at the same
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
        let start_beat = 4.0;

        let mut clock = TransportClock::new(
            Arc::new(AtomicF64::new(tempo)),
            Arc::new(AtomicBool::new(false)),
            sample_rate,
        )
        .starting_at(start_beat);

        let timeline = OfflineTransport::new(&OfflineTransportConfig {
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
            (clock_beat - timeline.current_beat()).abs() < 1e-9,
            "first sample disagrees: clock={clock_beat} timeline={}",
            timeline.current_beat()
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
            ((timeline.current_beat() - clock_beat) - expected_lag).abs() < 1e-9,
            "drifted across the block: clock={clock_beat} timeline={} \
             (expected exactly one beats_per_sample apart)",
            timeline.current_beat()
        );
    }

    #[test]
    fn test_timeline_advances() {
        let timeline = OfflineTransport::new(&OfflineTransportConfig {
            start_beat: 0.0,
            tempo: Bpm(120.0),
            sample_rate: SampleRate(44100.0),
            loop_range: None,
        });

        // At 120 BPM, 2 beats/second, 44100 samples/second
        // So 22050 samples = 1 beat
        timeline.advance(22050);
        assert!((timeline.current_beat() - 1.0).abs() < 0.001);

        timeline.advance(22050);
        assert!((timeline.current_beat() - 2.0).abs() < 0.001);
    }

    #[test]
    fn test_timeline_loop_wrap() {
        let timeline = OfflineTransport::new(&OfflineTransportConfig {
            start_beat: 0.0,
            tempo: Bpm(120.0),
            sample_rate: SampleRate(44100.0),
            loop_range: Some((0.0, 4.0)),
        });

        // 4 beats at 120 BPM = 2 seconds = 88200 samples
        let samples_per_beat = 44100.0 / 2.0;

        // Advance to beat 3
        timeline.advance((3.0 * samples_per_beat) as usize);
        assert!((timeline.current_beat() - 3.0).abs() < 0.01);

        // Advance 2 more beats - should wrap to beat 1
        timeline.advance((2.0 * samples_per_beat) as usize);
        assert!((timeline.current_beat() - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_timeline_no_loop() {
        let timeline = OfflineTransport::new(&OfflineTransportConfig {
            start_beat: 0.0,
            tempo: Bpm(120.0),
            sample_rate: SampleRate(44100.0),
            loop_range: None,
        });

        let samples_per_beat = 44100.0 / 2.0;

        // Advance past where loop end would be
        timeline.advance((10.0 * samples_per_beat) as usize);
        assert!((timeline.current_beat() - 10.0).abs() < 0.01);
    }

    #[test]
    fn test_timeline_start_offset() {
        let timeline = OfflineTransport::new(&OfflineTransportConfig {
            start_beat: 4.0,
            tempo: Bpm(120.0),
            sample_rate: SampleRate(44100.0),
            loop_range: None,
        });

        assert!((timeline.current_beat() - 4.0).abs() < 0.001);

        let samples_per_beat = 44100.0 / 2.0;
        timeline.advance(samples_per_beat as usize);
        assert!((timeline.current_beat() - 5.0).abs() < 0.01);
    }

    #[test]
    fn test_timeline_reset() {
        let timeline = OfflineTransport::new(&OfflineTransportConfig {
            start_beat: 0.0,
            tempo: Bpm(120.0),
            sample_rate: SampleRate(44100.0),
            loop_range: None,
        });

        let samples_per_beat = 44100.0 / 2.0;
        timeline.advance((5.0 * samples_per_beat) as usize);
        assert!((timeline.current_beat() - 5.0).abs() < 0.01);

        // Reset to beat 2
        timeline.reset(2.0);
        assert!((timeline.current_beat() - 2.0).abs() < 0.001);
    }

    #[test]
    fn test_transport_reader_impl() {
        use crate::TransportClockRead;

        let timeline = OfflineTransport::new(&OfflineTransportConfig {
            start_beat: 0.0,
            tempo: Bpm(120.0),
            sample_rate: SampleRate(44100.0),
            loop_range: Some((0.0, 8.0)),
        });

        // TransportClockRead methods
        assert!(timeline.is_playing());
        assert!(!timeline.is_recording());
        assert!(!timeline.is_in_preroll());
        assert!(timeline.is_loop_enabled());
        assert_eq!(timeline.get_loop_range(), Some((0.0, 8.0)));
    }

    #[test]
    fn test_transport_reader_no_loop() {
        use crate::TransportClockRead;

        let timeline = OfflineTransport::new(&OfflineTransportConfig {
            start_beat: 0.0,
            tempo: Bpm(120.0),
            sample_rate: SampleRate(44100.0),
            loop_range: None,
        });

        assert!(!timeline.is_loop_enabled());
        assert_eq!(timeline.get_loop_range(), None);
    }
}
