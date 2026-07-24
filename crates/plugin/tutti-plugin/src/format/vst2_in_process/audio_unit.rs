//! `InProcessVst2Client` — fundsp [`AudioUnit`] node that drives a VST2
//! plugin from the host audio thread.
//!
//! The instance lives behind `Arc<Mutex<tutti_vst2_host::Vst2Instance>>` shared
//! with the matching control backend. Audio thread acquires with
//! `try_lock`; on contention it falls back to silence and bumps
//! [`InProcessVst2Client::contention_count`].
//!
//! All per-block scratch — channel buffers, ref-vector storage, MIDI
//! drain — is pre-allocated at construction. The `process()` /
//! `tick()` paths are allocation-free in steady state (verified by the
//! `assert_no_alloc` regression test in `tests/`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use tutti_core::{AudioUnit, BufferMut, BufferRef, SignalFrame, F64};
use tutti_midi_runtime::MidiSender;
use tutti_midi_types::MidiUnitId;
use tutti_vst2_host::{PluginInfo, ProcessContext, RenderScratch, Vst2Instance};

use crate::host::node::Midi;

/// Maximum block size we pre-size scratch for. Matches fundsp's
/// `MAX_BUFFER_SIZE` so a single block lands in one `process()` call.
const BLOCK_SIZE: usize = 64;

/// Per-channel f32/f64 staging buffers + reusable ref vectors.
///
/// The `vst2-host` API takes `&[&[f32]]` / `&mut [&mut [f32]]`, so we
/// stage caller samples into our own contiguous `Vec<f32>` arrays and
/// reborrow them as slice-of-slices each call. The Vecs are sized
/// once at construction; the ref-vector capacity is also pre-reserved.
struct ProcessScratch {
    f32_in: Vec<Vec<f32>>,
    f32_out: Vec<Vec<f32>>,
    f64_in: Vec<Vec<f64>>,
    f64_out: Vec<Vec<f64>>,
}

impl ProcessScratch {
    fn new(num_inputs: usize, num_outputs: usize) -> Self {
        Self {
            f32_in: (0..num_inputs).map(|_| vec![0.0; BLOCK_SIZE]).collect(),
            f32_out: (0..num_outputs).map(|_| vec![0.0; BLOCK_SIZE]).collect(),
            f64_in: (0..num_inputs).map(|_| vec![0.0; BLOCK_SIZE]).collect(),
            f64_out: (0..num_outputs).map(|_| vec![0.0; BLOCK_SIZE]).collect(),
        }
    }
}

/// fundsp-graph-facing audio node for an in-process VST2 plugin.
pub struct InProcessVst2Client {
    inner: Arc<Mutex<Vst2Instance>>,
    metadata: PluginInfo,
    midi: Midi,
    /// Per-clone audio scratch handed to `vst::AudioBuffer::from_raw`.
    scratch: RenderScratch,
    /// Per-clone f32/f64 staging arrays (pre-allocated, reused).
    process_scratch: ProcessScratch,
    sample_rate: f64,
    /// Bumped on every audio-thread `try_lock` failure. Shared across
    /// clones so the handle can read the global count.
    contention_count: Arc<AtomicU64>,
}

impl InProcessVst2Client {
    pub(crate) fn new(
        inner: Arc<Mutex<Vst2Instance>>,
        metadata: PluginInfo,
        sample_rate: f64,
        contention_count: Arc<AtomicU64>,
    ) -> Self {
        let scratch = RenderScratch::new(metadata.num_inputs, metadata.num_outputs, BLOCK_SIZE);
        let process_scratch = ProcessScratch::new(metadata.num_inputs.count() as usize, metadata.num_outputs.count() as usize);
        Self {
            inner,
            metadata,
            midi: Midi::new(),
            scratch,
            process_scratch,
            sample_rate,
            contention_count,
        }
    }

    /// Producer handle for this plugin's MIDI inbox. Cheap to clone.
    pub fn midi_sender(&self) -> MidiSender {
        self.midi.sender()
    }

    /// Install the outbound routing target so this plugin's MIDI-out re-enters
    /// the graph. See [`Midi::set_out`]. Off-RT; call once at wiring time.
    pub fn set_midi_out(
        &self,
        queue: Arc<dyn tutti_midi_types::MidiRouter>,
        routing: Arc<arc_swap::ArcSwap<tutti_midi_types::MidiRoutingSnapshot>>,
    ) {
        self.midi.set_out(queue, routing);
    }

    /// Drop the outbound routing target; subsequent blocks discard MIDI-out.
    pub fn clear_midi_out(&self) {
        self.midi.clear_out();
    }

    /// Cumulative audio-thread `try_lock` failures since construction.
    /// Shared across clones; intended for diagnostic introspection by
    /// embedders (no current internal caller).
    pub fn contention_count(&self) -> u64 {
        self.contention_count.load(Ordering::Relaxed)
    }
}

impl Clone for InProcessVst2Client {
    fn clone(&self) -> Self {
        // Arc-clone the live plugin; allocate fresh scratch + Midi for
        // this clone (matches Batcher::clone in the subprocess client).
        // Done at clone time, not on the audio thread.
        let scratch = RenderScratch::new(
            self.metadata.num_inputs,
            self.metadata.num_outputs,
            BLOCK_SIZE,
        );
        let process_scratch =
            ProcessScratch::new(self.metadata.num_inputs.count() as usize, self.metadata.num_outputs.count() as usize);
        Self {
            inner: Arc::clone(&self.inner),
            metadata: self.metadata.clone(),
            midi: self.midi.clone(),
            scratch,
            process_scratch,
            sample_rate: self.sample_rate,
            contention_count: Arc::clone(&self.contention_count),
        }
    }
}

impl InProcessVst2Client {
    fn ensure_scratch_size(&mut self, size: usize) {
        // If a graph reconfigures to a larger block size, grow once.
        // Steady-state never hits this branch.
        if size > BLOCK_SIZE {
            for ch in self.process_scratch.f32_in.iter_mut() {
                if ch.len() < size {
                    ch.resize(size, 0.0);
                }
            }
            for ch in self.process_scratch.f32_out.iter_mut() {
                if ch.len() < size {
                    ch.resize(size, 0.0);
                }
            }
            for ch in self.process_scratch.f64_in.iter_mut() {
                if ch.len() < size {
                    ch.resize(size, 0.0);
                }
            }
            for ch in self.process_scratch.f64_out.iter_mut() {
                if ch.len() < size {
                    ch.resize(size, 0.0);
                }
            }
        }
    }
}

impl AudioUnit for InProcessVst2Client {
    fn inputs(&self) -> usize {
        self.metadata.num_inputs.count() as usize
    }

    fn outputs(&self) -> usize {
        self.metadata.num_outputs.count() as usize
    }

    fn reset(&mut self) {
        if let Some(mut instance) = self.inner.try_lock() {
            // VST2 reset is suspend → resume; the vst crate doesn't
            // expose a discrete reset opcode. Re-set sample rate as a
            // best-effort no-op trigger.
            instance.set_sample_rate(self.sample_rate);
        }
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.sample_rate = sample_rate;
        if let Some(mut instance) = self.inner.try_lock() {
            instance.set_sample_rate(sample_rate);
        }
    }

    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        // Single-sample tick reuses process() with size=1.
        for (ch, &sample) in input.iter().enumerate().take(self.metadata.num_inputs.count() as usize) {
            self.process_scratch.f32_in[ch][0] = sample;
        }
        let processed = drive_f32(
            &self.inner,
            &self.contention_count,
            &mut self.midi,
            &mut self.scratch,
            &mut self.process_scratch,
            self.metadata.num_inputs.count() as usize,
            self.metadata.num_outputs.count() as usize,
            1,
            self.sample_rate,
        );
        if !processed {
            for slot in output.iter_mut() {
                *slot = 0.0;
            }
            return;
        }
        for (ch, slot) in output
            .iter_mut()
            .enumerate()
            .take(self.metadata.num_outputs.count() as usize)
        {
            *slot = self.process_scratch.f32_out[ch][0];
        }
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        self.ensure_scratch_size(size);

        // Stage caller samples into our pre-allocated f32 channel buffers.
        for ch in 0..self.metadata.num_inputs.count() as usize {
            let slot = &mut self.process_scratch.f32_in[ch][..size];
            for (i, dst) in slot.iter_mut().enumerate() {
                *dst = input.at_f32(ch, i);
            }
        }

        let processed = drive_f32(
            &self.inner,
            &self.contention_count,
            &mut self.midi,
            &mut self.scratch,
            &mut self.process_scratch,
            self.metadata.num_inputs.count() as usize,
            self.metadata.num_outputs.count() as usize,
            size,
            self.sample_rate,
        );

        if !processed {
            for ch in 0..self.metadata.num_outputs.count() as usize {
                for i in 0..size {
                    output.set_f32(ch, i, 0.0);
                }
            }
            return;
        }

        for ch in 0..self.metadata.num_outputs.count() as usize {
            let slot = &self.process_scratch.f32_out[ch][..size];
            for (i, &v) in slot.iter().enumerate() {
                output.set_f32(ch, i, v);
            }
        }
    }

    fn get_id(&self) -> u64 {
        crate::util::node::node_id::PLUGIN_CLIENT_ID
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        crate::host::node::route_with_latency(
            self.metadata.num_inputs.count() as usize,
            self.metadata.num_outputs.count() as usize,
            self.metadata.latency_samples as f64,
            input,
        )
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

impl AudioUnit<F64> for InProcessVst2Client {
    fn inputs(&self) -> usize {
        self.metadata.num_inputs.count() as usize
    }

    fn outputs(&self) -> usize {
        self.metadata.num_outputs.count() as usize
    }

    fn reset(&mut self) {
        if let Some(mut instance) = self.inner.try_lock() {
            instance.set_sample_rate(self.sample_rate);
        }
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.sample_rate = sample_rate;
        if let Some(mut instance) = self.inner.try_lock() {
            instance.set_sample_rate(sample_rate);
        }
    }

    fn tick(&mut self, input: &[f64], output: &mut [f64]) {
        for (ch, &sample) in input.iter().enumerate().take(self.metadata.num_inputs.count() as usize) {
            self.process_scratch.f64_in[ch][0] = sample;
        }
        let processed = drive_f64(
            &self.inner,
            &self.contention_count,
            &mut self.midi,
            &mut self.scratch,
            &mut self.process_scratch,
            self.metadata.num_inputs.count() as usize,
            self.metadata.num_outputs.count() as usize,
            1,
            self.sample_rate,
        );
        if !processed {
            for slot in output.iter_mut() {
                *slot = 0.0;
            }
            return;
        }
        for (ch, slot) in output
            .iter_mut()
            .enumerate()
            .take(self.metadata.num_outputs.count() as usize)
        {
            *slot = self.process_scratch.f64_out[ch][0];
        }
    }

    fn process(&mut self, size: usize, input: &BufferRef<F64>, output: &mut BufferMut<F64>) {
        self.ensure_scratch_size(size);

        for ch in 0..self.metadata.num_inputs.count() as usize {
            let slot = &mut self.process_scratch.f64_in[ch][..size];
            for (i, dst) in slot.iter_mut().enumerate() {
                *dst = input.at_scalar(ch, i);
            }
        }

        let processed = drive_f64(
            &self.inner,
            &self.contention_count,
            &mut self.midi,
            &mut self.scratch,
            &mut self.process_scratch,
            self.metadata.num_inputs.count() as usize,
            self.metadata.num_outputs.count() as usize,
            size,
            self.sample_rate,
        );

        if !processed {
            for ch in 0..self.metadata.num_outputs.count() as usize {
                for i in 0..size {
                    output.set_scalar(ch, i, 0.0);
                }
            }
            return;
        }

        for ch in 0..self.metadata.num_outputs.count() as usize {
            let slot = &self.process_scratch.f64_out[ch][..size];
            for (i, &v) in slot.iter().enumerate() {
                output.set_scalar(ch, i, v);
            }
        }
    }

    fn get_id(&self) -> u64 {
        crate::util::node::node_id::PLUGIN_CLIENT_ID
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        crate::host::node::route_with_latency(
            self.metadata.num_inputs.count() as usize,
            self.metadata.num_outputs.count() as usize,
            self.metadata.latency_samples as f64,
            input,
        )
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

impl InProcessVst2Client {
    /// This unit's MIDI routing address.
    pub fn midi_unit_id(&self) -> MidiUnitId {
        self.midi.unit_id()
    }
}

/// Reborrow the staging arrays as slice-of-slices and call into
/// `vst2-host`. Free function so we can take disjoint borrows of the
/// fields on the caller side without a self-borrow conflict.
#[allow(clippy::too_many_arguments)]
fn drive_f32(
    inner: &Arc<Mutex<Vst2Instance>>,
    contention: &AtomicU64,
    midi: &mut Midi,
    scratch: &mut RenderScratch,
    process_scratch: &mut ProcessScratch,
    num_inputs: usize,
    num_outputs: usize,
    size: usize,
    sample_rate: f64,
) -> bool {
    let midi_events = midi.drain_for_process(size).clone();
    match inner.try_lock() {
        Some(mut instance) => {
            // Build slice-of-slices on the stack via scratch arrays we
            // own — Vec<&[f32]>/Vec<&mut[f32]> would allocate, so drop
            // into stack arrays bounded by MAX_CHANNELS. 16 covers any
            // realistic VST2 (most are mono / stereo).
            const MAX_CHANNELS: usize = 16;
            debug_assert!(num_inputs <= MAX_CHANNELS, "VST2 input ch > 16");
            debug_assert!(num_outputs <= MAX_CHANNELS, "VST2 output ch > 16");

            let mut in_refs: [&[f32]; MAX_CHANNELS] = [&[]; MAX_CHANNELS];
            #[allow(clippy::needless_range_loop)]
            for ch in 0..num_inputs.min(MAX_CHANNELS) {
                in_refs[ch] = &process_scratch.f32_in[ch][..size];
            }
            let in_slice = &in_refs[..num_inputs.min(MAX_CHANNELS)];

            // Splitting f32_out into N disjoint mutable slices via
            // `split_first_mut` lets us hand `vst2-host` a slice-of-slices
            // without per-call allocation.
            run_with_mut_channels_f32(
                &mut process_scratch.f32_out[..num_outputs.min(MAX_CHANNELS)],
                size,
                |out_slice| {
                    let ctx = ProcessContext::new(sample_rate).midi(&midi_events);
                    let midi_out = instance.process_f32(in_slice, out_slice, size, &ctx, scratch);
                    // Re-inject the plugin's MIDI-out into routing (no-op if no
                    // out-target installed). Emitting here, inside the block,
                    // keeps each event's frame_offset intact.
                    midi.emit(midi_out);
                },
            );
            true
        }
        None => {
            contention.fetch_add(1, Ordering::Relaxed);
            false
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn drive_f64(
    inner: &Arc<Mutex<Vst2Instance>>,
    contention: &AtomicU64,
    midi: &mut Midi,
    scratch: &mut RenderScratch,
    process_scratch: &mut ProcessScratch,
    num_inputs: usize,
    num_outputs: usize,
    size: usize,
    sample_rate: f64,
) -> bool {
    let midi_events = midi.drain_for_process(size).clone();
    match inner.try_lock() {
        Some(mut instance) => {
            const MAX_CHANNELS: usize = 16;
            debug_assert!(num_inputs <= MAX_CHANNELS, "VST2 input ch > 16");
            debug_assert!(num_outputs <= MAX_CHANNELS, "VST2 output ch > 16");

            let mut in_refs: [&[f64]; MAX_CHANNELS] = [&[]; MAX_CHANNELS];
            #[allow(clippy::needless_range_loop)]
            for ch in 0..num_inputs.min(MAX_CHANNELS) {
                in_refs[ch] = &process_scratch.f64_in[ch][..size];
            }
            let in_slice = &in_refs[..num_inputs.min(MAX_CHANNELS)];

            run_with_mut_channels_f64(
                &mut process_scratch.f64_out[..num_outputs.min(MAX_CHANNELS)],
                size,
                |out_slice| {
                    let ctx = ProcessContext::new(sample_rate).midi(&midi_events);
                    let midi_out = instance.process_f64(in_slice, out_slice, size, &ctx, scratch);
                    // Re-inject the plugin's MIDI-out into routing (see `drive_f32`).
                    midi.emit(midi_out);
                },
            );
            true
        }
        None => {
            contention.fetch_add(1, Ordering::Relaxed);
            false
        }
    }
}

/// Recursively peel one `&mut [f32]` of length `size` off `channels`
/// at a time, building a slice-of-slices on the stack and invoking
/// `f` once it's complete. Allocation-free — every reborrow lives in
/// stack frames.
fn run_with_mut_channels_f32<F: FnOnce(&mut [&mut [f32]])>(
    channels: &mut [Vec<f32>],
    size: usize,
    f: F,
) {
    fn recurse<'a, F: FnOnce(&mut [&mut [f32]])>(
        rest: &'a mut [Vec<f32>],
        size: usize,
        acc: &mut [&'a mut [f32]],
        depth: usize,
        f: F,
    ) {
        if depth == acc.len() {
            f(acc);
            return;
        }
        let (head, tail) = rest
            .split_first_mut()
            .expect("channel count mismatch (f32)");
        acc[depth] = &mut head[..size];
        recurse(tail, size, acc, depth + 1, f);
    }
    // Stack-allocated ref array, sized to actual channel count.
    let mut acc: [&mut [f32]; 16] = [
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
    ];
    let n = channels.len().min(16);
    recurse(channels, size, &mut acc[..n], 0, f);
}

fn run_with_mut_channels_f64<F: FnOnce(&mut [&mut [f64]])>(
    channels: &mut [Vec<f64>],
    size: usize,
    f: F,
) {
    fn recurse<'a, F: FnOnce(&mut [&mut [f64]])>(
        rest: &'a mut [Vec<f64>],
        size: usize,
        acc: &mut [&'a mut [f64]],
        depth: usize,
        f: F,
    ) {
        if depth == acc.len() {
            f(acc);
            return;
        }
        let (head, tail) = rest
            .split_first_mut()
            .expect("channel count mismatch (f64)");
        acc[depth] = &mut head[..size];
        recurse(tail, size, acc, depth + 1, f);
    }
    let mut acc: [&mut [f64]; 16] = [
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
    ];
    let n = channels.len().min(16);
    recurse(channels, size, &mut acc[..n], 0, f);
}
