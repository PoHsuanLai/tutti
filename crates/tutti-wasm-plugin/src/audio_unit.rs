//! `InProcessWasmClient` — fundsp [`AudioUnit`] node that drives a WASM
//! Component Model audio plugin from the host audio thread.
//!
//! The instance lives behind `Arc<parking_lot::Mutex<WasmInstance>>`
//! shared with the matching [`InProcessWasmBackend`](crate::control_backend::InProcessWasmBackend).
//! The audio thread always takes the lock with `try_lock`; on contention
//! it falls back to silence and bumps
//! [`InProcessWasmClient::contention_count`].
//!
//! Allocation profile of `process()`:
//! - The cached `inputs_planar` tree inside [`WasmInstance`] is reused
//!   across blocks (one growth on first call).
//! - The host-side lift of the guest's `list<list<f32>>` return *does*
//!   allocate per block — this is unavoidable at the v0.1 WIT
//!   contract. Fixing it is a v0.2 WIT bump tracked separately.
//! - Per-channel staging Vec<Vec<f32>> for inputs/outputs is allocated
//!   once at construction and grown if a block exceeds `BLOCK_SIZE`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use tutti_midi_types::{MidiTarget, MidiUnitId};
use tutti_core::{AudioUnit, BufferMut, BufferRef, SignalFrame, F64};
use tutti_midi_runtime::MidiSender;

use tutti_plugin::backend::Midi;
use tutti_plugin_types::PluginInfo;

use crate::instance::WasmInstance;

/// Maximum block size we pre-size scratch for. Matches fundsp's
/// `MAX_BUFFER_SIZE` so a single block lands in one `process_f32` call.
const BLOCK_SIZE: usize = 64;

/// Per-channel staging buffers. The WIT contract is planar f32 only
/// (`f64_support = false` in metadata), so we only need f32 scratch —
/// the F64 trait impl down-converts at the edge.
struct ProcessScratch {
    f32_in: Vec<Vec<f32>>,
    f32_out: Vec<Vec<f32>>,
}

impl ProcessScratch {
    fn new(num_inputs: usize, num_outputs: usize) -> Self {
        Self {
            f32_in: (0..num_inputs).map(|_| vec![0.0; BLOCK_SIZE]).collect(),
            f32_out: (0..num_outputs).map(|_| vec![0.0; BLOCK_SIZE]).collect(),
        }
    }
}

pub struct InProcessWasmClient {
    inner: Arc<Mutex<WasmInstance>>,
    metadata: PluginInfo,
    midi: Midi,
    process_scratch: ProcessScratch,
    sample_rate: f64,
    /// Bumped on every audio-thread `try_lock` failure. Shared across
    /// clones so a handle / control side can read the global count.
    contention_count: Arc<AtomicU64>,
}

impl InProcessWasmClient {
    pub(crate) fn new(
        inner: Arc<Mutex<WasmInstance>>,
        metadata: PluginInfo,
        sample_rate: f64,
        contention_count: Arc<AtomicU64>,
    ) -> Self {
        let process_scratch =
            ProcessScratch::new(metadata.audio_io.inputs, metadata.audio_io.outputs);
        Self {
            inner,
            metadata,
            midi: Midi::new(),
            process_scratch,
            sample_rate,
            contention_count,
        }
    }

    pub fn midi_sender(&self) -> MidiSender {
        self.midi.sender()
    }

    /// Cumulative audio-thread `try_lock` failures since construction.
    /// Shared across clones; intended for diagnostic introspection by
    /// embedders (no current internal caller).
    pub fn contention_count(&self) -> u64 {
        self.contention_count.load(Ordering::Relaxed)
    }

    fn ensure_scratch_size(&mut self, size: usize) {
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
        }
    }
}

impl Clone for InProcessWasmClient {
    fn clone(&self) -> Self {
        let process_scratch =
            ProcessScratch::new(self.metadata.audio_io.inputs, self.metadata.audio_io.outputs);
        Self {
            inner: Arc::clone(&self.inner),
            metadata: self.metadata.clone(),
            midi: self.midi.clone(),
            process_scratch,
            sample_rate: self.sample_rate,
            contention_count: Arc::clone(&self.contention_count),
        }
    }
}

impl AudioUnit for InProcessWasmClient {
    fn inputs(&self) -> usize {
        self.metadata.audio_io.inputs
    }

    fn outputs(&self) -> usize {
        self.metadata.audio_io.outputs
    }

    fn reset(&mut self) {
        self.midi.reset_sample_pos();
        // No discrete `reset` call in WIT v0.1; the guest exposes one
        // but it's optional and we don't need to disturb its state here.
        // Sample-rate reapply mimics VST2 reset behavior as a best
        // effort.
        if let Some(mut inst) = self.inner.try_lock() {
            inst.set_sample_rate(self.sample_rate);
        }
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.sample_rate = sample_rate;
        // Control-side may also adjust; we use `try_lock` to stay
        // audio-thread-safe. A missed rate change here will be picked
        // up by the control backend's blocking `lock()` call.
        if let Some(mut inst) = self.inner.try_lock() {
            inst.set_sample_rate(sample_rate);
        }
    }

    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        let n_in = self.metadata.audio_io.inputs;
        let n_out = self.metadata.audio_io.outputs;
        for (ch, &sample) in input.iter().enumerate().take(n_in) {
            self.process_scratch.f32_in[ch][0] = sample;
        }
        if !drive(
            &self.inner,
            &self.contention_count,
            &mut self.midi,
            &mut self.process_scratch,
            n_in,
            n_out,
            1,
        ) {
            for slot in output.iter_mut() {
                *slot = 0.0;
            }
            return;
        }
        for (ch, slot) in output.iter_mut().enumerate().take(n_out) {
            *slot = self.process_scratch.f32_out[ch][0];
        }
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        self.ensure_scratch_size(size);
        let n_in = self.metadata.audio_io.inputs;
        let n_out = self.metadata.audio_io.outputs;

        // Stage caller samples into our pre-allocated f32 channel buffers.
        for ch in 0..n_in {
            let slot = &mut self.process_scratch.f32_in[ch][..size];
            for (i, dst) in slot.iter_mut().enumerate() {
                *dst = input.at_f32(ch, i);
            }
        }

        if !drive(
            &self.inner,
            &self.contention_count,
            &mut self.midi,
            &mut self.process_scratch,
            n_in,
            n_out,
            size,
        ) {
            for ch in 0..n_out {
                for i in 0..size {
                    output.set_f32(ch, i, 0.0);
                }
            }
            return;
        }

        for ch in 0..n_out {
            let slot = &self.process_scratch.f32_out[ch][..size];
            for (i, &v) in slot.iter().enumerate() {
                output.set_f32(ch, i, v);
            }
        }
    }

    fn get_id(&self) -> u64 {
        tutti_plugin::backend::PLUGIN_CLIENT_ID
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        tutti_plugin::backend::route_with_latency(
            self.metadata.audio_io.inputs,
            self.metadata.audio_io.outputs,
            self.metadata.latency_samples as f64,
            input,
        )
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

impl AudioUnit<F64> for InProcessWasmClient {
    fn inputs(&self) -> usize {
        self.metadata.audio_io.inputs
    }

    fn outputs(&self) -> usize {
        self.metadata.audio_io.outputs
    }

    fn reset(&mut self) {
        self.midi.reset_sample_pos();
        if let Some(mut inst) = self.inner.try_lock() {
            inst.set_sample_rate(self.sample_rate);
        }
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.sample_rate = sample_rate;
        if let Some(mut inst) = self.inner.try_lock() {
            inst.set_sample_rate(sample_rate);
        }
    }

    fn tick(&mut self, input: &[f64], output: &mut [f64]) {
        let n_in = self.metadata.audio_io.inputs;
        let n_out = self.metadata.audio_io.outputs;
        // WIT v0.1 is f32-only; down-convert at the edge.
        for (ch, &sample) in input.iter().enumerate().take(n_in) {
            self.process_scratch.f32_in[ch][0] = sample as f32;
        }
        if !drive(
            &self.inner,
            &self.contention_count,
            &mut self.midi,
            &mut self.process_scratch,
            n_in,
            n_out,
            1,
        ) {
            for slot in output.iter_mut() {
                *slot = 0.0;
            }
            return;
        }
        for (ch, slot) in output.iter_mut().enumerate().take(n_out) {
            *slot = self.process_scratch.f32_out[ch][0] as f64;
        }
    }

    fn process(&mut self, size: usize, input: &BufferRef<F64>, output: &mut BufferMut<F64>) {
        self.ensure_scratch_size(size);
        let n_in = self.metadata.audio_io.inputs;
        let n_out = self.metadata.audio_io.outputs;

        for ch in 0..n_in {
            let slot = &mut self.process_scratch.f32_in[ch][..size];
            for (i, dst) in slot.iter_mut().enumerate() {
                *dst = input.at_scalar(ch, i) as f32;
            }
        }

        if !drive(
            &self.inner,
            &self.contention_count,
            &mut self.midi,
            &mut self.process_scratch,
            n_in,
            n_out,
            size,
        ) {
            for ch in 0..n_out {
                for i in 0..size {
                    output.set_scalar(ch, i, 0.0);
                }
            }
            return;
        }

        for ch in 0..n_out {
            let slot = &self.process_scratch.f32_out[ch][..size];
            for (i, &v) in slot.iter().enumerate() {
                output.set_scalar(ch, i, v as f64);
            }
        }
    }

    fn get_id(&self) -> u64 {
        tutti_plugin::backend::PLUGIN_CLIENT_ID
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        tutti_plugin::backend::route_with_latency(
            self.metadata.audio_io.inputs,
            self.metadata.audio_io.outputs,
            self.metadata.latency_samples as f64,
            input,
        )
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

impl MidiTarget for InProcessWasmClient {
    fn midi_unit_id(&self) -> MidiUnitId {
        self.midi.unit_id()
    }
}

/// Acquire the instance lock and run one block. Returns `false` on
/// contention (audio thread emits silence) or guest error (output zero,
/// caller continues).
fn drive(
    inner: &Arc<Mutex<WasmInstance>>,
    contention: &AtomicU64,
    midi: &mut Midi,
    scratch: &mut ProcessScratch,
    n_in: usize,
    n_out: usize,
    size: usize,
) -> bool {
    let midi_events = midi.drain_for_process(size).clone();
    let Some(mut inst) = inner.try_lock() else {
        contention.fetch_add(1, Ordering::Relaxed);
        return false;
    };

    // Build slice-of-slices on the stack via fixed-size arrays — avoids
    // per-call Vec allocation. 16 channels covers any realistic audio
    // plugin (most are mono / stereo / 5.1 / 7.1).
    const MAX_CHANNELS: usize = 16;
    debug_assert!(n_in <= MAX_CHANNELS, "WASM plugin input channels > 16");
    debug_assert!(n_out <= MAX_CHANNELS, "WASM plugin output channels > 16");

    let mut in_refs: [&[f32]; MAX_CHANNELS] = [&[]; MAX_CHANNELS];
    #[allow(clippy::needless_range_loop)]
    for ch in 0..n_in.min(MAX_CHANNELS) {
        in_refs[ch] = &scratch.f32_in[ch][..size];
    }
    let in_slice = &in_refs[..n_in.min(MAX_CHANNELS)];

    run_with_mut_channels(
        &mut scratch.f32_out[..n_out.min(MAX_CHANNELS)],
        size,
        |out_slice| {
            // Discard guest-emitted MIDI for v0.1 — the plugin's MIDI-out
            // path needs routing infrastructure not wired through this
            // node yet. Drop quietly; matches the VST2 in-process
            // backend's current behavior.
            let _ = inst.process_f32(in_slice, &midi_events, out_slice, size);
        },
    );
    true
}

/// Recursively peel one `&mut [f32]` of length `size` off `channels`
/// at a time, building a slice-of-slices on the stack and invoking `f`
/// once it's complete. Allocation-free — every reborrow lives in stack
/// frames.
fn run_with_mut_channels<F: FnOnce(&mut [&mut [f32]])>(
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
