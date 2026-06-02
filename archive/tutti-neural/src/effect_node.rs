//! Audio-thread effect node.
//!
//! [`Effect`] is a newtype around `InferenceNode<AudioBlock, SlotSink>`.
//! Two leaves carry the meaning:
//!
//! | Leaf | Role |
//! | ---- | ---- |
//! | [`AudioBlock`] | [`Trigger`] — accumulates one buffer of audio frames, yields an `Arc<[f32]>` from a round-robin pool. |
//! | [`SlotSink`] | [`Sink`] — owns the [`SlotReader`], mints a fresh [`SlotWriter`](crate::ipc::SlotWriter) for each request, renders received output. |
//!
//! `Effect::tick` is purely:
//!
//! ```text
//! ingest one frame → maybe submit → render one frame from latest output
//! ```
//!
//! # Real-time guarantees
//!
//! - [`AudioBlock::ingest`] uses an [`ArcPool`] — a
//!   `POOL_SIZE = 4` round-robin buffer pool, no heap alloc on the hot path.
//! - [`SlotSink::response`] mints a writer via [`SlotReader::new_writer`] —
//!   one `Arc` clone, no allocation.
//! - [`SlotSink::render`] is a bounded atomic-load + memcpy.
//! - Submission is [`submit`](crate::ipc::submit) = `try_send`; drops the
//!   request if the engine's event channel is full.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crossbeam_channel::Sender;
use tutti_core::{AudioUnit, BufferMut, BufferRef, SignalFrame};

use crate::ipc::{self, ArcPool, Event, Response, Shape, SlotReader};
use crate::model_id::ModelId;
use crate::node::{InferenceNode, Sink, Trigger};

const POOL_SIZE: usize = 4;

/// Audio [`Trigger`]: accumulates one full block of audio frames before
/// yielding a buffer for inference.
pub struct AudioBlock {
    accum: Vec<f32>,
    write_pos: usize,
    channels: usize,
    buffer_size: usize,
    pool: ArcPool,
}

impl AudioBlock {
    pub fn new(channels: usize, buffer_size: usize) -> Self {
        let total = channels * buffer_size;
        Self {
            accum: vec![0.0f32; total],
            write_pos: 0,
            channels,
            buffer_size,
            pool: ArcPool::new(POOL_SIZE, total),
        }
    }
}

impl Trigger for AudioBlock {
    type Input<'a> = &'a [f32];

    fn shape(&self) -> Shape {
        Shape::new(1, self.channels * self.buffer_size)
    }

    fn ingest(&mut self, frame: &[f32]) -> Option<Arc<[f32]>> {
        for (ch, &sample) in frame.iter().enumerate().take(self.channels) {
            let idx = self.write_pos * self.channels + ch;
            if idx < self.accum.len() {
                self.accum[idx] = sample;
            }
        }
        self.write_pos += 1;
        if self.write_pos >= self.buffer_size {
            self.write_pos = 0;
            self.pool.fill(&self.accum)
        } else {
            None
        }
    }
}

/// Audio [`Sink`]: owns the engine-side write target and the audio-thread
/// reader, renders the latest received block sample-by-sample.
pub struct SlotSink {
    reader: SlotReader,
    channels: usize,
}

impl SlotSink {
    pub fn new(reader: SlotReader, channels: usize) -> Self {
        Self { reader, channels }
    }
}

impl Sink for SlotSink {
    fn response(&mut self) -> Response {
        Response::Audio(self.reader.new_writer())
    }

    fn render(&mut self, output: &mut [f32]) {
        if self.reader.has_output() {
            for ch in 0..self.channels.min(output.len()) {
                output[ch] = self.reader.read(ch);
            }
        } else {
            for s in output.iter_mut() {
                *s = 0.0;
            }
        }
    }
}

/// Neural-effect audio unit. Stereo-in, stereo-out (or whatever `channels`
/// specifies). Latency is whatever the probe measured (converted from
/// [`ProbeReport::latency`](crate::ProbeReport::latency) to samples) so the
/// graph's PDC can align around the model's constant processing delay.
///
/// Latency lives behind an [`Arc<AtomicUsize>`] so callers can update the
/// reported delay after construction via [`Effect::latency_handle`]. That's
/// the seam to use if you later re-bench the model under real load or
/// re-probe at a different batch size. A subsequent `graph.commit()`
/// re-runs PDC and the rest of the graph re-aligns.
pub struct Effect {
    inner: InferenceNode<AudioBlock, SlotSink>,
    channels: usize,
    buffer_size: usize,
    latency_samples: Arc<AtomicUsize>,
    frame_in: Vec<f32>,
    frame_out: Vec<f32>,
}

impl Effect {
    /// Handle for updating this node's reported latency without rebuilding.
    ///
    /// Stores go through `Ordering::Release`; [`AudioUnit::latency`] loads
    /// with `Ordering::Acquire`. After storing a new value, call
    /// `graph.commit()` for PDC to re-run with the updated number — the
    /// atomic itself is just the publish channel.
    ///
    /// Clones of this [`Effect`] (e.g. ones installed into a live graph)
    /// share the same cell, so one update reflects everywhere the node
    /// was placed.
    pub fn latency_handle(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.latency_samples)
    }
}

impl AudioUnit for Effect {
    fn inputs(&self) -> usize {
        self.channels
    }
    fn outputs(&self) -> usize {
        self.channels
    }

    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        self.inner.step(input, output);
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        for i in 0..size {
            for ch in 0..self.channels {
                self.frame_in[ch] = input.at_f32(ch, i);
            }
            self.inner.step(&self.frame_in, &mut self.frame_out);
            for ch in 0..self.channels {
                output.set_f32(ch, i, self.frame_out[ch]);
            }
        }
    }

    fn set_sample_rate(&mut self, _sample_rate: tutti_core::SampleRate) {}
    fn reset(&mut self) {}

    fn get_id(&self) -> u64 {
        tutti_core::node_id::NEURAL_EFFECT_ID
    }

    fn latency(&mut self) -> Option<f64> {
        Some(self.latency_samples.load(Ordering::Acquire) as f64)
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        input.clone()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

impl Clone for Effect {
    /// Share the latency cell with the clone, so a subsequent
    /// [`latency_handle`](Effect::latency_handle) store reaches both. The
    /// rest is rebuilt via [`effect_node`] (new ipc slot, new ArcPool).
    fn clone(&self) -> Self {
        let (reader, _initial_writer) = ipc::slot(self.channels, self.buffer_size);
        let trigger = AudioBlock::new(self.channels, self.buffer_size);
        let sink = SlotSink::new(reader, self.channels);
        Effect {
            inner: InferenceNode::new(self.inner.id, self.inner.tx.clone(), trigger, sink),
            channels: self.channels,
            buffer_size: self.buffer_size,
            latency_samples: Arc::clone(&self.latency_samples),
            frame_in: vec![0.0f32; self.channels],
            frame_out: vec![0.0f32; self.channels],
        }
    }
}

/// Build an [`Effect`] driven by `id`. `channels` sets the IO width,
/// `buffer_size` sets how many frames per inference batch, and
/// `latency_samples` is the constant processing delay the node will report
/// to the graph's PDC — typically
/// [`loaded.report.latency_samples(engine.sample_rate())`](crate::ProbeReport::latency_samples).
/// The value is stored in an atomic cell and can be updated post-hoc via
/// [`Effect::latency_handle`].
pub fn effect_node(
    id: ModelId,
    channels: usize,
    buffer_size: usize,
    latency_samples: usize,
    tx: Sender<Event>,
) -> Effect {
    let (reader, _initial_writer) = ipc::slot(channels, buffer_size);
    let trigger = AudioBlock::new(channels, buffer_size);
    let sink = SlotSink::new(reader, channels);
    Effect {
        inner: InferenceNode::new(id, tx, trigger, sink),
        channels,
        buffer_size,
        latency_samples: Arc::new(AtomicUsize::new(latency_samples)),
        frame_in: vec![0.0f32; channels],
        frame_out: vec![0.0f32; channels],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;
    use tutti_core::AudioUnit;

    #[test]
    fn test_audio_block_yields_at_boundary() {
        let mut t = AudioBlock::new(2, 4);
        for _ in 0..3 {
            assert!(t.ingest(&[0.5, 0.5]).is_none());
        }
        let out = t.ingest(&[0.5, 0.5]).expect("4th frame closes the block");
        assert_eq!(out.len(), 8);
    }

    #[test]
    fn test_audio_block_shape() {
        let t = AudioBlock::new(2, 16);
        assert_eq!(t.shape(), Shape::new(1, 32));
    }

    #[test]
    fn test_slot_sink_renders_silence_when_empty() {
        let (reader, _w) = ipc::slot(2, 4);
        let mut sink = SlotSink::new(reader, 2);
        let mut out = [1.0, 1.0];
        sink.render(&mut out);
        assert_eq!(out, [0.0, 0.0]);
    }

    #[test]
    fn test_effect_node_io() {
        let (tx, _rx) = unbounded();
        let node = effect_node(ModelId::new(), 2, 512, 512, tx);
        assert_eq!(node.inputs(), 2);
        assert_eq!(node.outputs(), 2);
    }

    #[test]
    fn test_effect_latency_reports_probe_samples() {
        let (tx, _rx) = unbounded();
        let mut node = effect_node(ModelId::new(), 2, 512, 1024, tx);
        assert_eq!(node.latency(), Some(1024.0));
    }

    #[test]
    fn test_latency_handle_updates_reported_latency() {
        let (tx, _rx) = unbounded();
        let mut node = effect_node(ModelId::new(), 2, 512, 512, tx);
        assert_eq!(node.latency(), Some(512.0));
        node.latency_handle().store(2048, Ordering::Release);
        assert_eq!(node.latency(), Some(2048.0));
    }

    #[test]
    fn test_clone_shares_latency_cell() {
        let (tx, _rx) = unbounded();
        let node = effect_node(ModelId::new(), 2, 512, 512, tx);
        let handle = node.latency_handle();
        let mut cloned = node.clone();
        handle.store(4096, Ordering::Release);
        // Clone sees the update because both share the same Arc<AtomicUsize>.
        assert_eq!(cloned.latency(), Some(4096.0));
    }

    #[test]
    fn test_effect_node_submits_on_full_buffer() {
        let (tx, rx) = unbounded::<Event>();
        let id = ModelId::new();
        let mut node = effect_node(id, 2, 4, 4, tx);

        for _ in 0..4 {
            let mut out = [0.0f32, 0.0];
            node.tick(&[0.5, 0.5], &mut out);
        }

        match rx.try_recv().expect("submit fired") {
            Event::Req(req) => {
                assert_eq!(req.id, id);
                assert_eq!(req.input.len(), 8);
                assert_eq!(req.shape, Shape::new(1, 8));
            }
            _ => panic!("expected Req"),
        }
    }
}
