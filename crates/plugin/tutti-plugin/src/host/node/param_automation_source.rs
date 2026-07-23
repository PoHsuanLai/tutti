//! Beat-scheduled parameter automation as a sample-accurate producer.
//!
//! The parameter-automation counterpart of [`super::harmony_source::HarmonySource`]:
//! a [`ParamAutomationSource`] holds one [`AutomationEnvelope`] per plugin
//! parameter id plus a [`Timeline`]. Each block it reads the transport
//! beat, walks the block sample-by-sample stepping the beat cursor, and fills a
//! reused [`ParameterChanges`] with one [`ParameterPoint`] per parameter at
//! sample-accurate offsets across the block window.
//!
//! Unlike chord/scale (stepwise *context* emitted only at change boundaries) an
//! automation envelope is a *continuous* signal, so we densely sample it: one
//! point per `stride` samples, matching how `AutomationLane::process` fills an
//! audio block. Plugins receive real per-block parameter ramps instead of the
//! single frame-rate `set_parameter` value the old `PluginParam` ECS path sent.
//!
//! This is deliberately the *only* automation path for hosted plugins — the
//! frame-rate `set_parameter` route was never wired for hosted-plugin params
//! (no `AutomationTarget` fed it), so there is no legacy behaviour to preserve;
//! automation reaches plugins sample-accurate or not at all.

use std::sync::Arc;

use audio_automation::AutomationEnvelope;
use tutti_core::transport::Timeline;

use crate::host::node::input_slot::{BlockCtx, BlockInput, BlockReset};
use crate::protocol::ParameterChanges;

/// One plugin parameter's automation curve, keyed by the plugin's numeric
/// parameter id. The envelope is evaluated against the transport beat.
#[derive(Clone)]
pub struct TimedParam {
    pub param_id: u32,
    pub envelope: Arc<AutomationEnvelope<f32>>,
}

/// How many samples between successive automation points within one block. A
/// block is at most `fundsp::MAX_BUFFER_SIZE` (64) samples, so a stride of 8
/// yields up to 8 points per parameter per block — dense enough for smooth
/// ramps, cheap enough to stay allocation-light. The block boundaries
/// themselves are always sampled (offset 0 and the last sample).
const SAMPLE_STRIDE: usize = 8;

/// Beat-scheduled parameter-automation producer. Cheap to clone (envelopes
/// shared via `Arc`, transport shared via `Arc`) so the fundsp graph-commit
/// clone of the parent node doesn't reallocate the curves.
#[derive(Clone)]
pub struct ParamAutomationSource {
    params: Arc<[TimedParam]>,
    transport: Arc<dyn Timeline>,
    sample_rate: f64,
}

impl ParamAutomationSource {
    /// Build a parameter-automation source from one envelope per parameter id.
    pub fn new(
        params: impl IntoIterator<Item = TimedParam>,
        transport: Arc<dyn Timeline>,
        sample_rate: f64,
    ) -> Self {
        Self {
            params: params.into_iter().collect::<Vec<_>>().into(),
            transport,
            sample_rate,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.params.is_empty()
    }

    /// Fill `out` (cleared first) with one [`ParameterQueue`] per parameter,
    /// densely sampled across this block's beat window. No points are emitted
    /// while the transport is stopped or at non-positive tempo/rate — the
    /// plugin keeps its last value, matching how a paused transport freezes the
    /// playhead.
    ///
    /// [`ParameterQueue`]: crate::protocol::ParameterQueue
    pub fn fill(&self, block_size: usize, out: &mut ParameterChanges) {
        out.clear();
        if block_size == 0 || self.params.is_empty() {
            return;
        }
        if !self.transport.is_rolling() {
            return;
        }
        let start_beat = self.transport.beat();
        let tempo_bpm = self.transport.tempo().get();
        if tempo_bpm <= 0.0 || self.sample_rate <= 0.0 {
            return;
        }
        let beats_per_sample = tempo_bpm / 60.0 / self.sample_rate;
        let loop_range = self.transport.loop_range();
        let last = block_size - 1;

        for param in self.params.iter() {
            if param.envelope.is_empty() {
                continue;
            }
            let mut queue = crate::protocol::ParameterQueue::new(param.param_id);
            // Sample at 0, every `SAMPLE_STRIDE`, and always the final sample so
            // the block's end value is exact (the next block starts from here).
            let mut offset = 0usize;
            loop {
                let beat = start_beat + offset as f64 * beats_per_sample;
                let eff_beat = match loop_range {
                    Some((ls, le)) if le > ls && beat >= le => ls + ((beat - ls) % (le - ls)),
                    _ => beat,
                };
                if let Some(v) = param.envelope.get_value_at(eff_beat) {
                    queue.add_point(offset as i32, v as f64);
                }
                if offset == last {
                    break;
                }
                offset = (offset + SAMPLE_STRIDE).min(last);
            }
            if !queue.is_empty() {
                out.add_queue(queue);
            }
        }
    }
}

impl BlockInput for ParamAutomationSource {
    type Out = ParameterChanges;
    fn fill(&self, ctx: BlockCtx, out: &mut ParameterChanges) {
        // Inherent `fill` self-clears, so it satisfies the "fully overwrite
        // `out`" contract.
        ParamAutomationSource::fill(self, ctx.block_size, out);
    }
}

impl BlockReset for ParameterChanges {
    fn reset(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_float::AtomicF64;
    use audio_automation::AutomationPoint;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tutti_core::params::Bpm;

    struct TestTransport {
        beat: AtomicF64,
        tempo: f64,
        playing: AtomicBool,
    }
    impl TestTransport {
        fn new(tempo: f64) -> Self {
            Self {
                beat: AtomicF64::new(0.0),
                tempo,
                playing: AtomicBool::new(true),
            }
        }
        fn set_beat(&self, b: f64) {
            self.beat.store(b, Ordering::Release);
        }
    }
    impl Timeline for TestTransport {
        fn beat(&self) -> f64 {
            self.beat.load(Ordering::Acquire)
        }
        fn loop_range(&self) -> Option<(f64, f64)> {
            None
        }
        fn is_rolling(&self) -> bool {
            self.playing.load(Ordering::Acquire)
        }
        fn tempo(&self) -> Bpm {
            Bpm(self.tempo)
        }
    }

    /// A 0→1 ramp over 4 beats, labelled with parameter id `7`.
    fn ramp(param_id: u32) -> TimedParam {
        let mut env: AutomationEnvelope<f32> = AutomationEnvelope::new(0.0f32);
        env.add_point(AutomationPoint::new(0.0, 0.0));
        env.add_point(AutomationPoint::new(4.0, 1.0));
        TimedParam {
            param_id,
            envelope: Arc::new(env),
        }
    }

    #[test]
    fn fills_one_queue_per_param_with_ramp() {
        let transport = Arc::new(TestTransport::new(120.0)); // 22050 samples/beat @ 44.1k
        let src = ParamAutomationSource::new(
            vec![ramp(7)],
            Arc::clone(&transport) as Arc<dyn Timeline>,
            44100.0,
        );
        let mut out = ParameterChanges::new();
        src.fill(64, &mut out);
        assert_eq!(out.queues.len(), 1);
        let q = &out.queues[0];
        assert_eq!(q.param_id, 7);
        // First point at offset 0, beat 0 → value 0.
        assert_eq!(q.points[0].sample_offset, 0);
        assert!(q.points[0].value.abs() < 1e-6);
        // Points ascend in offset and the last is the block's final sample.
        assert_eq!(q.points.last().unwrap().sample_offset, 63);
        for w in q.points.windows(2) {
            assert!(w[1].sample_offset > w[0].sample_offset);
            assert!(w[1].value >= w[0].value); // ramp is monotonic up
        }
    }

    #[test]
    fn paused_emits_nothing() {
        let transport = Arc::new(TestTransport::new(120.0));
        transport.playing.store(false, Ordering::Release);
        let src = ParamAutomationSource::new(
            vec![ramp(7)],
            Arc::clone(&transport) as Arc<dyn Timeline>,
            44100.0,
        );
        let mut out = ParameterChanges::new();
        src.fill(64, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn advancing_transport_moves_the_value() {
        let transport = Arc::new(TestTransport::new(120.0));
        let src = ParamAutomationSource::new(
            vec![ramp(7)],
            Arc::clone(&transport) as Arc<dyn Timeline>,
            44100.0,
        );
        let mut out = ParameterChanges::new();
        src.fill(64, &mut out);
        let start_v = out.queues[0].points[0].value;
        // Jump to beat 4 (envelope top) — the first point should now read ~1.0.
        transport.set_beat(4.0);
        src.fill(64, &mut out);
        let later_v = out.queues[0].points[0].value;
        assert!(later_v > start_v);
        assert!((later_v - 1.0).abs() < 1e-3);
    }

    #[test]
    fn empty_source_and_empty_envelope_emit_nothing() {
        let transport = Arc::new(TestTransport::new(120.0)) as Arc<dyn Timeline>;
        let empty_src = ParamAutomationSource::new(Vec::new(), Arc::clone(&transport), 44100.0);
        let mut out = ParameterChanges::new();
        empty_src.fill(64, &mut out);
        assert!(out.is_empty());

        let empty_env: AutomationEnvelope<f32> = AutomationEnvelope::new(0.0f32);
        let src = ParamAutomationSource::new(
            vec![TimedParam {
                param_id: 3,
                envelope: Arc::new(empty_env),
            }],
            transport,
            44100.0,
        );
        src.fill(64, &mut out);
        assert!(out.is_empty());
    }
}
