//! Block-loop driver for offline rendering.
//!
//! Sets the net's sample rate, then pumps the net through [`MAX_BUFFER_SIZE`]
//! blocks until `plan.total_samples` have been produced. Each block is
//! handed to the [`RenderSink`] along with a [`BlockCursor`] describing its
//! position in the full render. The optional [`OfflineTransport`] advances
//! in lockstep with the net so transport-aware nodes see the correct beat
//! position for each block. Progress is reported via the supplied
//! [`ProgressEmitter`].

use crate::progress::ProgressEmitter;
use crate::render::{BlockCursor, RenderPlan, RenderSink};
use crate::Result;
use std::sync::Arc;
use tutti_core::transport::OfflineTransport;
use tutti_core::{AudioUnit, BufferRef, BufferVec, MAX_BUFFER_SIZE};

/// Drive `net` for `plan.total_samples` samples, pumping each block into
/// `sink` and emitting progress via `progress`. If `timeline` is provided,
/// advance it in lockstep with the net so transport-aware nodes receive
/// correct beat positions.
pub(crate) fn drive(
    net: &mut tutti_core::dsp::Net,
    sample_rate: f64,
    plan: &RenderPlan,
    timeline: Option<&Arc<OfflineTransport>>,
    sink: &mut dyn RenderSink,
    progress: &mut ProgressEmitter<'_>,
) -> Result<()> {
    net.set_sample_rate(tutti_core::SampleRate(sample_rate));

    let mut buffer = BufferVec::new(net.outputs().max(2));
    let empty_input = BufferRef::new(&[]);
    let stereo = net.outputs() >= 2;

    progress.start();

    // Advance by 1 sample before the first block so the first processed
    // sample sees "1 sample in" — matches advance-then-tick semantics used
    // elsewhere in the engine.
    if let Some(t) = timeline {
        t.advance(1);
    }

    let mut produced = 0usize;
    let mut kept = 0usize;
    while produced < plan.total_samples {
        let block_size = (plan.total_samples - produced).min(MAX_BUFFER_SIZE);

        let mut buffer_mut = buffer.buffer_mut();
        net.process(block_size, &empty_input, &mut buffer_mut);

        if let Some(t) = timeline {
            t.advance(block_size);
        }

        let left = &buffer_mut.channel_f32(0)[..block_size];
        // Duplicate the left channel for mono nets so the sink always sees
        // stereo slices.
        let right_owned: Vec<f32>;
        let right: &[f32] = if stereo {
            &buffer_mut.channel_f32(1)[..block_size]
        } else {
            right_owned = left.to_vec();
            &right_owned
        };

        let cursor = BlockCursor {
            block_start_sample: produced,
            latency_samples: plan.latency_samples,
            samples_kept_so_far: kept,
            output_length: plan.output_length,
        };
        kept += sink.accept_block(left, right, cursor)?;

        produced += block_size;
        progress.tick(produced);
    }

    Ok(())
}
