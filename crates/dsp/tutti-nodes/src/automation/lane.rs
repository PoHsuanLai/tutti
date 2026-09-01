//! AutomationLaneNode AudioUnit node.

use std::sync::Arc;

use tutti_core::{beat_from_ports, AudioUnit, BufferMut, BufferRef, SignalFrame, BEAT_PORTS};
use tutti_types::Beat;

use super::Curve;

/// An automation lane that evaluates a [`Curve`] against musical time.
///
/// The beat arrives on the node's [`BEAT_PORTS`] inputs (port 0 whole beats,
/// port 1 the fraction), wired from `TransportClock`. The lane holds no
/// transport: it is a pure function of the beat it is handed, which makes it
/// per-sample accurate and lets an offline render drive it from its own clock
/// without any special casing.
///
/// The beat arriving here is already loop-wrapped by the clock, so the lane
/// does not consult a loop range.
///
/// The curve is held behind `Arc<dyn Curve>` so the node stays cheap to clone
/// (the fundsp graph-commit clones nodes) and agnostic to how the curve is
/// stored — an [`AutomationEnvelope`], a constant, an LFO shape.
///
/// [`AutomationEnvelope`]: audio_automation::AutomationEnvelope
///
/// # Example
///
/// A four-beat ramp from silence to unity, pushed into a [`Net`] whose two
/// inputs are the [`BEAT_PORTS`] pair a `TransportClock` drives.
///
/// ```
/// use tutti_core::dsp::{AudioUnit, Net};
/// use tutti_nodes::automation::{AutomationEnvelope, AutomationLaneNode, AutomationPoint};
///
/// let mut envelope: AutomationEnvelope<f32> = AutomationEnvelope::new(0.0);
/// envelope.add_point(AutomationPoint::new(0.0, 0.0));
/// envelope.add_point(AutomationPoint::new(4.0, 1.0));
///
/// // Two in (whole beats, fraction), one out (the curve's value).
/// let mut net = Net::new(2, 1);
/// let lane = net.push(Box::new(AutomationLaneNode::new(envelope)));
/// net.pipe_input(lane);
/// net.pipe_output(lane);
/// net.check();
///
/// // Beat 2.0 is halfway along the ramp.
/// let mut out = [0.0f32; 1];
/// net.tick(&[2.0, 0.0], &mut out);
/// assert!((out[0] - 0.5).abs() < 1e-3, "midpoint of a 0..1 ramp, got {}", out[0]);
/// ```
///
/// [`Net`]: tutti_core::dsp::Net
pub struct AutomationLaneNode {
    curve: Arc<dyn Curve>,
    last_value: f32,
}

/// Alias for the `graph.node_as::<LiveAutomationLane>(..)` lookups in
/// consumers. The lane is not generic over the envelope's label.
pub type LiveAutomationLane = AutomationLaneNode;

impl AutomationLaneNode {
    /// Builds a lane that evaluates `curve` at the transport beat.
    ///
    /// The curve is held behind an `Arc` and read on the audio thread, so it
    /// must be cheap to evaluate and must not allocate in `value_at`.
    pub fn new(curve: impl Curve + 'static) -> Self {
        Self {
            curve: Arc::new(curve),
            last_value: 0.0,
        }
    }

    /// Replaces the curve, allocating a new `Arc`.
    ///
    /// `&mut self`, so it cannot reach a node already live in the graph — a
    /// live curve swap goes through a respawn. Does not clear
    /// [`last_value`](Self::last_value).
    pub fn set_curve(&mut self, curve: impl Curve + 'static) {
        self.curve = Arc::new(curve);
    }

    /// Most recent value emitted by `tick()` / `process()`.
    ///
    /// After `tick()`, this is that single sample. After `process(size, ...)`,
    /// this is the value at `output[size - 1]` — i.e. the end-of-block value,
    /// not the average. `reset()` clears it to zero.
    pub fn last_value(&self) -> f32 {
        self.last_value
    }

    /// Evaluates the curve at `beat` **without** recording it as the last
    /// value.
    ///
    /// Returns `0.0` where the curve has no value — before its first point, or
    /// on an empty envelope. Use [`update_to`](Self::update_to) to evaluate and
    /// record in one step.
    pub fn get_value_at(&self, beat: Beat) -> f32 {
        self.curve.value_at(beat).unwrap_or(0.0)
    }

    /// Evaluate at `beat` and record it as the last value.
    pub fn update_to(&mut self, beat: Beat) -> f32 {
        self.last_value = self.get_value_at(beat);
        self.last_value
    }
}

impl AudioUnit for AutomationLaneNode {
    fn inputs(&self) -> usize {
        BEAT_PORTS
    }

    fn outputs(&self) -> usize {
        1
    }

    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        output[0] = self.update_to(beat_from_ports(input[0], input[1]));
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        for i in 0..size {
            let beat = beat_from_ports(input.at_f32(0, i), input.at_f32(1, i));
            output.set_f32(0, i, self.get_value_at(beat));
        }

        if size > 0 {
            self.last_value = output.at_f32(0, size - 1);
        }
    }

    fn reset(&mut self) {
        self.last_value = 0.0;
    }

    /// No-op: the beat arrives on the input ports, so the lane derives nothing
    /// from the sample rate.
    fn set_sample_rate(&mut self, _sample_rate: tutti_core::SampleRate) {}

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        SignalFrame::new(1)
    }

    fn get_id(&self) -> u64 {
        crate::node_id::AUTOMATION_LANE_ID
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

impl Clone for AutomationLaneNode {
    fn clone(&self) -> Self {
        Self {
            curve: Arc::clone(&self.curve),
            last_value: self.last_value,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_automation::{AutomationEnvelope, AutomationPoint};
    use tutti_core::BufferVec;

    /// Split a beat the way `TransportClock` does, for feeding the input ports.
    fn ports(beat: f64) -> [f32; 2] {
        let whole = beat.floor();
        [whole as f32, (beat - whole) as f32]
    }

    fn ramp_envelope() -> AutomationEnvelope<&'static str> {
        let mut env: AutomationEnvelope<&str> = AutomationEnvelope::new("volume");
        env.add_point(AutomationPoint::new(0.0, 0.0));
        env.add_point(AutomationPoint::new(4.0, 1.0));
        env.add_point(AutomationPoint::new(8.0, 0.5));
        env
    }

    #[test]
    fn declares_two_beat_inputs() {
        let lane = AutomationLaneNode::new(ramp_envelope());
        assert_eq!(lane.inputs(), BEAT_PORTS);
        assert_eq!(lane.outputs(), 1);
    }

    #[test]
    fn test_update_tracks_beat_position() {
        let mut lane = AutomationLaneNode::new(ramp_envelope());

        assert!((lane.update_to(Beat(0.0)) - 0.0).abs() < 0.01);
        assert!((lane.update_to(Beat(2.0)) - 0.5).abs() < 0.01);
        assert!((lane.update_to(Beat(4.0)) - 1.0).abs() < 0.01);
        assert!((lane.update_to(Beat(6.0)) - 0.75).abs() < 0.01);
    }

    /// The clock emits an already-wrapped beat, so a lane fed beat 6 behaves
    /// identically whether or not a loop produced it. This replaces the old
    /// `get_value_looped` tests, which duplicated the clock's wrap.
    #[test]
    fn wrapped_beat_needs_no_loop_handling_in_the_lane() {
        let mut lane = AutomationLaneNode::new(ramp_envelope());
        // A 4..8 loop wraps beat 10 to beat 6 in the clock.
        let wrapped = lane.update_to(Beat(6.0));
        assert!(
            (wrapped - 0.75).abs() < 0.01,
            "expected ~0.75, got {wrapped}"
        );
    }

    #[test]
    fn test_tick_outputs_current_value() {
        let mut lane = AutomationLaneNode::new(ramp_envelope());
        let mut output = [0.0f32; 1];

        lane.tick(&ports(4.0), &mut output);
        assert!((output[0] - 1.0).abs() < 0.01);

        lane.tick(&ports(0.0), &mut output);
        assert!((output[0] - 0.0).abs() < 0.01);
    }

    #[test]
    fn tick_reads_the_fractional_port() {
        let mut lane = AutomationLaneNode::new(ramp_envelope());
        let mut split = [0.0f32; 1];
        let mut whole = [0.0f32; 1];

        // Beat 2.0 delivered as (0 whole + 2.0 frac) must match (2.0 + 0).
        lane.tick(&[0.0, 2.0], &mut split);
        lane.tick(&[2.0, 0.0], &mut whole);
        assert!((split[0] - whole[0]).abs() < 1e-6);
    }

    #[test]
    fn test_tick_updates_last_value() {
        let mut lane = AutomationLaneNode::new(ramp_envelope());
        assert_eq!(lane.last_value(), 0.0);

        let mut output = [0.0f32; 1];
        lane.tick(&ports(4.0), &mut output);
        assert!((lane.last_value() - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_set_curve_changes_output() {
        let mut lane = AutomationLaneNode::new(ramp_envelope());
        assert!((lane.update_to(Beat(2.0)) - 0.5).abs() < 0.01);

        let mut flat: AutomationEnvelope<&str> = AutomationEnvelope::new("flat");
        flat.add_point(AutomationPoint::new(0.0, 0.9));
        flat.add_point(AutomationPoint::new(8.0, 0.9));
        lane.set_curve(flat);

        assert!((lane.update_to(Beat(2.0)) - 0.9).abs() < 0.01);
    }

    #[test]
    fn test_reset_clears_last_value() {
        let mut lane = AutomationLaneNode::new(ramp_envelope());
        lane.update_to(Beat(4.0));
        assert!((lane.last_value() - 1.0).abs() < 0.01);

        lane.reset();
        assert_eq!(lane.last_value(), 0.0);
    }

    #[test]
    fn test_empty_envelope_returns_zero() {
        let empty: AutomationEnvelope<&str> = AutomationEnvelope::new("empty");
        let mut lane = AutomationLaneNode::new(empty);
        assert_eq!(lane.update_to(Beat(5.0)), 0.0);

        let mut output = [0.0f32; 1];
        lane.tick(&ports(5.0), &mut output);
        assert_eq!(output[0], 0.0);
    }

    /// Build a beat-ramp input buffer the way `TransportClock` would emit it:
    /// channel 0 whole beats, channel 1 the fraction. Written through the same
    /// `set_f32` API the node reads with, so the SIMD layout stays theirs.
    fn beat_ramp(size: usize, start: f64, per_sample: f64) -> BufferVec {
        let mut buf = BufferVec::new(BEAT_PORTS);
        {
            let mut view = buf.buffer_mut();
            for i in 0..size {
                let beat = start + i as f64 * per_sample;
                let whole = beat.floor();
                view.set_f32(0, i, whole as f32);
                view.set_f32(1, i, (beat - whole) as f32);
            }
        }
        buf
    }

    #[test]
    fn test_process_fills_block_from_beat_input() {
        use tutti_core::dsp::F32x;

        let mut lane = AutomationLaneNode::new(ramp_envelope());
        let block_size = 32;

        // Hold the beat at 4.0 for the whole block -> constant 1.0 output.
        let input_buf = beat_ramp(block_size, 4.0, 0.0);
        let input_ref = input_buf.buffer_ref();
        let mut output_simd = vec![F32x::ZERO; 8];
        let mut output_buf = BufferMut::new(&mut output_simd);

        lane.process(block_size, &input_ref, &mut output_buf);

        for i in 0..block_size {
            let val = output_buf.at_f32(0, i);
            assert!(
                (val - 1.0).abs() < 0.01,
                "Sample {i} expected ~1.0, got {val}"
            );
        }
    }

    #[test]
    fn test_process_updates_last_value() {
        use tutti_core::dsp::F32x;

        let mut lane = AutomationLaneNode::new(ramp_envelope());
        assert_eq!(lane.last_value(), 0.0);

        let input_buf = beat_ramp(64, 2.0, 0.0);
        let input_ref = input_buf.buffer_ref();
        let mut output_simd = vec![F32x::ZERO; 16];
        let mut output_buf = BufferMut::new(&mut output_simd);

        lane.process(64, &input_ref, &mut output_buf);

        assert!(
            (lane.last_value() - 0.5).abs() < 0.01,
            "Expected ~0.5, got {}",
            lane.last_value()
        );
    }

    /// The whole point of beat-as-signal: the value moves WITHIN a block,
    /// following the per-sample beat, rather than being held block-constant.
    #[test]
    fn test_process_per_sample_varies() {
        use tutti_core::dsp::F32x;

        let mut lane = AutomationLaneNode::new(ramp_envelope());
        // 120 BPM at 44.1 kHz.
        let per_sample = (120.0 / 60.0) / 44100.0;
        let input_buf = beat_ramp(32, 0.0, per_sample);
        let input_ref = input_buf.buffer_ref();
        let mut output_simd = vec![F32x::ZERO; 8];
        let mut output_buf = BufferMut::new(&mut output_simd);

        lane.process(32, &input_ref, &mut output_buf);

        assert!((output_buf.at_f32(0, 0) - 0.0).abs() < 0.001);

        let last = output_buf.at_f32(0, 31);
        assert!(last > 0.0, "Expected > 0.0, got {last}");
        assert!(last < 0.01, "Expected small value, got {last}");

        for i in 1..32 {
            assert!(
                output_buf.at_f32(0, i) >= output_buf.at_f32(0, i - 1),
                "Sample {i} should be >= sample {}",
                i - 1
            );
        }
    }
}
