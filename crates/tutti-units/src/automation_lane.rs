//! AutomationLane AudioUnit node.

use audio_automation::AutomationEnvelope;
use tutti_core::{AudioUnit, BufferMut, BufferRef, SignalFrame, TransportHandle, TransportReader};

/// An automation lane that outputs control signals based on transport position.
///
/// Reads the current beat position from the transport and evaluates
/// the envelope to produce a control signal output.
///
/// The lane is generic over `R: TransportReader`, allowing it to work with
/// either a live `TransportHandle` or an `OfflineTransport` for offline rendering.
///
/// # Sample rate
///
/// `AutomationLane::new` initializes `sample_rate` to `44_100.0`. `process()`
/// uses it to advance the beat cursor within a block; inserting a lane into
/// a graph causes the host to call [`AudioUnit::set_sample_rate`] once at
/// insertion time and again whenever the device rate changes. Call
/// [`AudioUnit::tick`] instead of `process` if you need to drive the lane
/// before it has been inserted into a graph.
///
/// # Example
///
/// ```ignore
/// use tutti_automation::{AutomationLane, AutomationEnvelope, AutomationPoint};
///
/// let mut envelope = AutomationEnvelope::new("volume");
/// envelope.add_point(AutomationPoint::new(0.0, 0.0));
/// envelope.add_point(AutomationPoint::new(4.0, 1.0));
///
/// let lane = AutomationLane::new(envelope, transport_handle);
/// ```
pub struct AutomationLane<T, R: TransportReader = TransportHandle> {
    envelope: AutomationEnvelope<T>,
    transport: R,
    last_value: f32,
    sample_rate: f64,
}

/// Type alias for automation lane with live transport.
pub type LiveAutomationLane<T> = AutomationLane<T, TransportHandle>;

impl<T, R: TransportReader> AutomationLane<T, R> {
    pub fn new(envelope: AutomationEnvelope<T>, transport: R) -> Self {
        Self {
            envelope,
            transport,
            last_value: 0.0,
            sample_rate: 44100.0,
        }
    }

    pub fn set_envelope(&mut self, envelope: AutomationEnvelope<T>) {
        self.envelope = envelope;
    }

    pub fn envelope(&self) -> &AutomationEnvelope<T> {
        &self.envelope
    }

    pub fn envelope_mut(&mut self) -> &mut AutomationEnvelope<T> {
        &mut self.envelope
    }

    /// Most recent value emitted by `tick()` / `process()`.
    ///
    /// After `tick()`, this is that single sample. After `process(size, ...)`,
    /// this is the value at `output[size - 1]` — i.e. the end-of-block value,
    /// not the average. `reset()` clears it to zero.
    pub fn last_value(&self) -> f32 {
        self.last_value
    }

    pub fn get_value_at(&self, beat: f64) -> f32 {
        self.envelope.get_value_at(beat).unwrap_or(0.0)
    }

    /// Get value accounting for transport loop.
    ///
    /// Wraps beat positions beyond `loop_end` back into `[loop_start, loop_end)`.
    /// Falls back to direct evaluation if the loop range is invalid.
    pub fn get_value_looped(&self, beat: f64, loop_start: f64, loop_end: f64) -> f32 {
        let loop_len = loop_end - loop_start;

        if loop_len <= 0.0 {
            return self.envelope.get_value_at(beat).unwrap_or(0.0);
        }

        let effective_beat = if beat < loop_end {
            beat
        } else {
            loop_start + ((beat - loop_start) % loop_len)
        };

        self.envelope.get_value_at(effective_beat).unwrap_or(0.0)
    }

    /// Update and return the current value using the transport position.
    ///
    /// Handles loop wrapping automatically when looping is enabled.
    pub fn update(&mut self) -> f32 {
        let beat = self.transport.current_beat_f64();

        self.last_value = self
            .transport
            .get_loop_range()
            .map(|(ls, le)| self.get_value_looped(beat, ls, le))
            .unwrap_or_else(|| self.envelope.get_value_at(beat).unwrap_or(0.0));

        self.last_value
    }
}

impl<T: Clone + Send + Sync + 'static, R: TransportReader + Clone + 'static> AudioUnit
    for AutomationLane<T, R>
{
    fn inputs(&self) -> usize {
        0
    }

    fn outputs(&self) -> usize {
        1
    }

    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        output[0] = self.update();
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        let beat = self.transport.current_beat_f64();
        let tempo = self.transport.tempo().get();
        let beats_per_sample = (tempo / 60.0) / self.sample_rate;
        let loop_range = self.transport.get_loop_range();

        for i in 0..size {
            let sample_beat = beat + i as f64 * beats_per_sample;
            let value = loop_range
                .map(|(ls, le)| self.get_value_looped(sample_beat, ls, le))
                .unwrap_or_else(|| self.envelope.get_value_at(sample_beat).unwrap_or(0.0));
            output.set_f32(0, i, value);
        }

        if size > 0 {
            self.last_value = output.at_f32(0, size - 1);
        }
    }

    fn reset(&mut self) {
        self.last_value = 0.0;
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.sample_rate = sample_rate;
    }

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        SignalFrame::new(1)
    }

    fn get_id(&self) -> u64 {
        tutti_core::node_id::AUTOMATION_LANE_ID
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

impl<T: Clone, R: TransportReader + Clone> Clone for AutomationLane<T, R> {
    fn clone(&self) -> Self {
        Self {
            envelope: self.envelope.clone(),
            transport: self.transport.clone(),
            last_value: self.last_value,
            sample_rate: self.sample_rate,
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    use super::*;
    use alloc::vec;
    use audio_automation::AutomationPoint;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    #[derive(Clone)]
    struct MockTransport {
        beat: Arc<AtomicU64>,
        loop_enabled: bool,
        loop_range: Option<(f64, f64)>,
    }

    impl MockTransport {
        fn new(beat: f64) -> Self {
            Self {
                beat: Arc::new(AtomicU64::new(beat.to_bits())),
                loop_enabled: false,
                loop_range: None,
            }
        }

        fn with_loop(beat: f64, loop_start: f64, loop_end: f64) -> Self {
            Self {
                beat: Arc::new(AtomicU64::new(beat.to_bits())),
                loop_enabled: true,
                loop_range: Some((loop_start, loop_end)),
            }
        }

        fn set_beat(&self, beat: f64) {
            self.beat.store(beat.to_bits(), Ordering::Relaxed);
        }
    }

    impl TransportReader for MockTransport {
        fn current_beat(&self) -> f64 {
            f64::from_bits(self.beat.load(Ordering::Relaxed))
        }
        fn is_loop_enabled(&self) -> bool {
            self.loop_enabled
        }
        fn get_loop_range(&self) -> Option<(f64, f64)> {
            self.loop_range
        }
        fn is_playing(&self) -> bool {
            true
        }
        fn is_recording(&self) -> bool {
            false
        }
        fn is_in_preroll(&self) -> bool {
            false
        }
        fn tempo(&self) -> tutti_core::Bpm {
            tutti_core::Bpm(120.0)
        }
    }

    fn ramp_envelope() -> AutomationEnvelope<&'static str> {
        let mut env: AutomationEnvelope<&str> = AutomationEnvelope::new("volume");
        env.add_point(AutomationPoint::new(0.0, 0.0));
        env.add_point(AutomationPoint::new(4.0, 1.0));
        env.add_point(AutomationPoint::new(8.0, 0.5));
        env
    }

    #[test]
    fn test_update_tracks_transport_position() {
        let transport = MockTransport::new(0.0);
        let mut lane = AutomationLane::new(ramp_envelope(), transport.clone());

        transport.set_beat(0.0);
        assert!((lane.update() - 0.0).abs() < 0.01);

        transport.set_beat(2.0);
        assert!((lane.update() - 0.5).abs() < 0.01);

        transport.set_beat(4.0);
        assert!((lane.update() - 1.0).abs() < 0.01);

        transport.set_beat(6.0);
        assert!((lane.update() - 0.75).abs() < 0.01);
    }

    #[test]
    fn test_update_with_loop_wraps_correctly() {
        let transport = MockTransport::with_loop(10.0, 4.0, 8.0);
        let mut lane = AutomationLane::new(ramp_envelope(), transport);

        let val = lane.update();
        assert!((val - 0.75).abs() < 0.01, "Expected ~0.75, got {}", val);
    }

    #[test]
    fn test_update_loop_enabled_but_no_range_falls_back() {
        let transport = MockTransport {
            beat: Arc::new(AtomicU64::new(2.0f64.to_bits())),
            loop_enabled: true,
            loop_range: None,
        };
        let mut lane = AutomationLane::new(ramp_envelope(), transport);
        assert!((lane.update() - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_tick_outputs_current_value() {
        let transport = MockTransport::new(0.0);
        let mut lane = AutomationLane::new(ramp_envelope(), transport.clone());

        let mut output = [0.0f32; 1];

        transport.set_beat(4.0);
        lane.tick(&[], &mut output);
        assert!((output[0] - 1.0).abs() < 0.01);

        transport.set_beat(0.0);
        lane.tick(&[], &mut output);
        assert!((output[0] - 0.0).abs() < 0.01);
    }

    #[test]
    fn test_tick_updates_last_value() {
        let transport = MockTransport::new(4.0);
        let mut lane = AutomationLane::new(ramp_envelope(), transport);

        assert_eq!(lane.last_value(), 0.0);
        let mut output = [0.0f32; 1];
        lane.tick(&[], &mut output);
        assert!((lane.last_value() - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_looped_value_at_exact_boundary_wraps() {
        let lane = AutomationLane::new(ramp_envelope(), MockTransport::new(0.0));
        let val = lane.get_value_looped(8.0, 4.0, 8.0);
        let at_start = lane.get_value_at(4.0);
        assert!((val - at_start).abs() < 0.01);
    }

    #[test]
    fn test_looped_value_with_invalid_range_doesnt_crash() {
        let lane = AutomationLane::new(ramp_envelope(), MockTransport::new(0.0));
        let val = lane.get_value_looped(2.0, 8.0, 4.0);
        assert!((val - 0.5).abs() < 0.01);
        let val = lane.get_value_looped(2.0, 4.0, 4.0);
        assert!((val - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_set_envelope_changes_output() {
        let transport = MockTransport::new(2.0);
        let mut lane = AutomationLane::new(ramp_envelope(), transport);

        assert!((lane.update() - 0.5).abs() < 0.01);

        let mut flat: AutomationEnvelope<&str> = AutomationEnvelope::new("flat");
        flat.add_point(AutomationPoint::new(0.0, 0.9));
        flat.add_point(AutomationPoint::new(8.0, 0.9));
        lane.set_envelope(flat);

        assert!((lane.update() - 0.9).abs() < 0.01);
    }

    #[test]
    fn test_reset_clears_last_value() {
        let mut lane = AutomationLane::new(ramp_envelope(), MockTransport::new(4.0));
        lane.update();
        assert!((lane.last_value() - 1.0).abs() < 0.01);

        lane.reset();
        assert_eq!(lane.last_value(), 0.0);
    }

    #[test]
    fn test_empty_envelope_returns_zero() {
        let empty: AutomationEnvelope<&str> = AutomationEnvelope::new("empty");
        let mut lane = AutomationLane::new(empty, MockTransport::new(5.0));
        assert_eq!(lane.update(), 0.0);

        let mut output = [0.0f32; 1];
        lane.tick(&[], &mut output);
        assert_eq!(output[0], 0.0);
    }

    #[test]
    fn test_process_fills_block_with_automation_value() {
        use tutti_core::dsp::F32x;

        let transport = MockTransport::new(4.0);
        let mut lane = AutomationLane::new(ramp_envelope(), transport);

        let mut output_simd = vec![F32x::ZERO; 8];
        let input_ref = BufferRef::empty();
        let mut output_buf = BufferMut::new(&mut output_simd);

        let block_size = 32;
        lane.process(block_size, &input_ref, &mut output_buf);

        for i in 0..block_size {
            let val = output_buf.at_f32(0, i);
            assert!(
                (val - 1.0).abs() < 0.01,
                "Sample {} expected ~1.0, got {}",
                i,
                val
            );
        }
    }

    #[test]
    fn test_process_updates_last_value() {
        use tutti_core::dsp::F32x;

        let transport = MockTransport::new(2.0);
        let mut lane = AutomationLane::new(ramp_envelope(), transport);

        assert_eq!(lane.last_value(), 0.0);

        let mut output_simd = vec![F32x::ZERO; 8];
        let input_ref = BufferRef::empty();
        let mut output_buf = BufferMut::new(&mut output_simd);

        lane.process(64, &input_ref, &mut output_buf);

        assert!(
            (lane.last_value() - 0.5).abs() < 0.01,
            "Expected ~0.5, got {}",
            lane.last_value()
        );
    }

    #[test]
    fn test_process_per_sample_varies() {
        use tutti_core::dsp::F32x;

        // Ramp from 0.0 at beat 0 to 1.0 at beat 4. At beat 0, each sample
        // should produce a slightly different value as the beat advances.
        let transport = MockTransport::new(0.0);
        let mut lane = AutomationLane::new(ramp_envelope(), transport);
        lane.sample_rate = 44100.0;

        let mut output_simd = vec![F32x::ZERO; 8];
        let input_ref = BufferRef::empty();
        let mut output_buf = BufferMut::new(&mut output_simd);

        lane.process(32, &input_ref, &mut output_buf);

        // First sample at beat 0 should be 0.0
        assert!((output_buf.at_f32(0, 0) - 0.0).abs() < 0.001);

        // Last sample should be slightly > 0.0 (per-sample beat advance)
        // beats_per_sample = (120/60) / 44100 ≈ 0.0000453515
        // beat at sample 31 = 31 * 0.0000453515 ≈ 0.001406
        // value = 0.001406 / 4.0 ≈ 0.000351 (ramp 0→1 over 4 beats)
        let last = output_buf.at_f32(0, 31);
        assert!(last > 0.0, "Expected > 0.0, got {last}");
        assert!(last < 0.01, "Expected small value, got {last}");

        // Values should be monotonically increasing (ramp up)
        for i in 1..32 {
            assert!(
                output_buf.at_f32(0, i) >= output_buf.at_f32(0, i - 1),
                "Sample {i} should be >= sample {}",
                i - 1
            );
        }
    }

    #[test]
    fn test_process_with_transport_advancing() {
        use tutti_core::dsp::F32x;

        let transport = MockTransport::new(0.0);
        let mut lane = AutomationLane::new(ramp_envelope(), transport.clone());

        let mut output_simd = vec![F32x::ZERO; 8];

        {
            let input_ref = BufferRef::empty();
            let mut output_buf = BufferMut::new(&mut output_simd);
            lane.process(16, &input_ref, &mut output_buf);
            assert!((output_buf.at_f32(0, 0) - 0.0).abs() < 0.01);
        }

        transport.set_beat(4.0);
        {
            let input_ref = BufferRef::empty();
            let mut output_buf = BufferMut::new(&mut output_simd);
            lane.process(16, &input_ref, &mut output_buf);
            assert!(
                (output_buf.at_f32(0, 0) - 1.0).abs() < 0.01,
                "Expected ~1.0 after transport advance, got {}",
                output_buf.at_f32(0, 0)
            );
        }
    }
}
