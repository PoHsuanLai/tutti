//! [`Batcher`] — buffers one `BATCH_SIZE` block of per-channel audio
//! between the caller and the plugin-server bridge.
//!
//! `write()` accumulates one input sample per channel per call. Once
//! `should_flush()` is true, `flush()` ships the whole block across the
//! bridge, stages the server's reply into output storage, and resets
//! cursors so `read()` can replay the outputs sample-by-sample. `process()`
//! is the block-in/block-out variant that bypasses tick storage entirely.
//!
//! # Pipelined, never waiting
//!
//! Both paths **submit block N and consume block N−1's output**, never waiting
//! for a reply. This replaced a synchronous version that spun on the audio
//! thread, where each node's wait was individually reasonable — half its own
//! block period — but the budgets *summed*: fundsp runs nodes serially in one
//! callback (`for &node_index in self.order`), so three stalled plugins spent
//! 3 × 667 µs against a 1333 µs deadline. Parallelising fundsp would not have
//! helped; plugins in series are a dependency chain. The defect was the waiting.
//!
//! Not waiting makes a stalled plugin cost zero, however many there are and
//! whatever the graph's shape. The price is one block of latency per
//! out-of-process plugin — 64 samples, 1.33 ms at 48 kHz — *declared to PDC*
//! (see [`PIPELINE_LATENCY_SAMPLES`]) and so compensated rather than heard. This
//! is what JACK, PipeWire and AUv3 all do.
//!
//! Whether N−1's output is really there is decided by the slab's per-slot
//! sequence numbers, not by a reply arriving: a mismatch yields silence. See
//! `util::transport::shm::header`.
//!
//! Dual f32/f64 support: fundsp drives a `PluginClient` through either
//! `AudioUnit<F32>` or `AudioUnit<F64>` (runtime choice). The wire format
//! is the one the plugin negotiated (independent of fundsp's choice), so
//! the tick scalar and wire scalar can differ — cross-format conversion
//! happens in the wire scratch. Each of `tick` and `wire` is a
//! format-tagged enum; only one variant exists per `Batcher` instance at
//! runtime.

use crate::error::Result;
use crate::host::ipc_client::PluginBridge;
use crate::host::node::BlockPayload;
use crate::protocol::{MidiEventVec, SampleFormat};
use tutti_core::{BufferMut, BufferRef, Sample as FundspSample, F32, F64};

/// Matches fundsp's `MAX_BUFFER_SIZE` (`1 << 6`). Blocks of this size are
/// fundsp's natural unit and what the plugin server is sized for.
///
/// This is a hard ceiling on the per-block sample count, not just a preference:
/// `AudioUnit::process` is documented "process up to 64 samples" and every
/// fundsp entry point `debug_assert!(size <= MAX_BUFFER_SIZE)`, with
/// `BigBlockAdapter` chunking anything larger before it reaches a node. So the
/// shared-memory slab is sized to this rather than to `config.max_buffer_size`
/// — see `subprocess::launch::setup_shm`, the other consumer of this constant.
pub(crate) const BATCH_SIZE: usize = 64;

/// `count` per-channel buffers, each `size` samples of zeroed `T`.
fn zero_channels<T: Copy + Default>(count: usize, size: usize) -> Vec<Vec<T>> {
    (0..count).map(|_| vec![T::default(); size]).collect()
}

/// Sealed dispatch over `f32`/`f64` scalars. Every method stages through
/// `Batcher`'s pre-sized wire scratch, so the audio thread never allocates.
pub(super) trait Scalar: Copy + Default + private::Seal {
    /// fundsp's format marker (`F32`/`F64`) for block-mode buffer types.
    type Marker: FundspSample<Scalar = Self>;

    fn write_in(batcher: &mut Batcher, ch: usize, pos: usize, value: Self);
    fn read_out(batcher: &Batcher, ch: usize, pos: usize) -> Self;

    /// Zero-fill tick output up to `size` samples for every channel.
    fn silence_tick(batcher: &mut Batcher, size: usize, outputs: usize);

    /// Copy one channel of tick storage into block `seq`'s input slot. Staging
    /// only — the caller publishes once, after the last channel.
    fn stage_tick(
        batcher: &mut Batcher,
        bridge: &PluginBridge,
        seq: u64,
        ch: usize,
        size: usize,
    ) -> Result<()>;
    /// Copy one channel of block `seq`'s output slot into tick storage.
    /// Zero-pads if the slab holds fewer than `size` samples.
    ///
    /// Only call this for a `seq` the slab has confirmed (see
    /// `Batcher::collectable`): the copy count proves nothing about who wrote
    /// the bytes.
    fn recv_tick(batcher: &mut Batcher, bridge: &PluginBridge, seq: u64, ch: usize, size: usize);

    /// Block-mode counterpart of [`stage_tick`](Self::stage_tick).
    fn stage_block(
        batcher: &mut Batcher,
        bridge: &PluginBridge,
        seq: u64,
        ch: usize,
        size: usize,
        input: &BufferRef<'_, Self::Marker>,
    ) -> Result<()>;
    /// Block-mode counterpart of [`recv_tick`](Self::recv_tick), with the same
    /// confirmed-`seq` precondition.
    fn recv_block(
        batcher: &mut Batcher,
        bridge: &PluginBridge,
        seq: u64,
        ch: usize,
        size: usize,
        output: &mut BufferMut<'_, Self::Marker>,
    );

    /// Zero-fill the block-mode output buffer.
    fn silence_block(output: &mut BufferMut<'_, Self::Marker>, size: usize, outputs: usize);
}

mod private {
    pub trait Seal {}
    impl Seal for f32 {}
    impl Seal for f64 {}
}

/// Per-channel tick scratch, keyed by whichever fundsp scalar type is
/// driving this `Batcher`. `Unset` until the first `write` / `flush` /
/// `process` locks in the scalar; the `Scalar::ensure_*_mut` helpers
/// allocate on demand.
pub(super) enum TickStorage {
    Unset,
    F32 {
        input: Vec<Vec<f32>>,
        output: Vec<Vec<f32>>,
    },
    F64 {
        input: Vec<Vec<f64>>,
        output: Vec<Vec<f64>>,
    },
}

/// Single-block wire scratch in the plugin-negotiated format. Allocated
/// at `Batcher::new` because the wire format is known up-front.
pub(super) enum WireStorage {
    F32 { input: Vec<f32>, output: Vec<f32> },
    F64 { input: Vec<f64>, output: Vec<f64> },
}

impl WireStorage {
    fn new(format: SampleFormat, max_block: usize) -> Self {
        match format {
            SampleFormat::Float32 => Self::F32 {
                input: vec![0.0; max_block],
                output: vec![0.0; max_block],
            },
            SampleFormat::Float64 => Self::F64 {
                input: vec![0.0; max_block],
                output: vec![0.0; max_block],
            },
        }
    }
}

/// Extra latency, in samples, that pipelining introduces: exactly one block.
///
/// Declared to PDC through `AudioUnit::route`, so the graph compensates for it
/// instead of the user hearing it. Three deliberate choices:
///
/// - **`BATCH_SIZE`, not the per-block `size`.** `route()` is never told the
///   block size, and a *varying* declared latency would be uncompensable anyway
///   — PDC sizes a fixed delay ring once, at plan time.
/// - **Not `config.max_buffer_size`.** That is 8192 by default: 171 ms of
///   declared latency for a 1.33 ms pipeline. Catastrophically wrong in the
///   direction that sounds broken.
/// - **An upper bound, deliberately.** Exact for a full 64-sample block,
///   pessimistic by `64 - size` for a partial one. Over-declaring keeps every
///   path aligned with every other; under-declaring would not.
pub(super) const PIPELINE_LATENCY_SAMPLES: usize = BATCH_SIZE;

/// One block of audio between fundsp callers and the plugin-server bridge.
pub(crate) struct Batcher {
    /// Total input ports (main + sidechain/aux input buses). Each port `ch` is
    /// written to input-region channel `ch` (bus-ordered), so a fundsp
    /// `connect(src, 0, target, 1)` feeds the plugin's sidechain bus.
    pub(super) inputs: usize,
    pub(super) outputs: usize,
    format: SampleFormat,

    tick: TickStorage,
    wire: WireStorage,

    // Shared across both tick paths — only one is ever live per instance.
    write_pos: usize,
    read_pos: usize,
    filled: usize,

    max_block: usize,

    /// The sequence number the next submitted block will carry. Starts at 1
    /// because 0 means "nothing published" in a freshly zeroed slab.
    next_seq: u64,
    /// The block whose output we expect to collect on the *next* call, or `None`
    /// when nothing is in flight (start-up, and after a reset).
    expect_seq: Option<u64>,
}

impl Clone for Batcher {
    fn clone(&self) -> Self {
        // Fresh buffers, reset cursors. Safe because fundsp clones on
        // graph commit before processing starts — no in-flight samples.
        let mut cloned = Self::new(self.inputs, self.outputs, self.format, self.max_block);
        // Carry the sequence forward rather than restarting at 1. A commit can
        // land during playback (PDC re-plans), and a clone that restarted would
        // accept a *pre-commit* block's late publish as its own block 1. The
        // in-flight block itself is deliberately dropped — `expect_seq` stays
        // `None` — because the clone is a different object and the old one may
        // still collect it.
        cloned.next_seq = self.next_seq;
        cloned
    }
}

impl Batcher {
    pub(super) fn new(
        inputs: usize,
        outputs: usize,
        format: SampleFormat,
        max_block: usize,
    ) -> Self {
        Self {
            inputs,
            outputs,
            format,
            tick: TickStorage::Unset,
            wire: WireStorage::new(format, max_block),
            write_pos: 0,
            read_pos: 0,
            filled: 0,
            max_block,
            next_seq: 1,
            expect_seq: None,
        }
    }

    /// Drop the in-flight block and start collecting fresh.
    ///
    /// **`next_seq` is deliberately not reset.** This is the load-bearing
    /// subtlety of the whole pipeline: if the count restarted at 1, a block
    /// submitted before a seek could be published after it and accepted as the
    /// new block 1 — the seek would replay a fragment of pre-seek audio. Keeping
    /// the sequence monotonic makes a late publish structurally unmatchable.
    /// (A `u64` at ~750 blocks/s takes on the order of 780,000 years to wrap.)
    ///
    /// The server's own `Reset` is a no-op for this purpose, so clearing
    /// `expect_seq` here is the *entire* mechanism by which a seek stops old
    /// audio from arriving.
    pub(super) fn reset(&mut self) {
        self.write_pos = 0;
        self.read_pos = 0;
        self.filled = 0;
        self.expect_seq = None;
    }

    /// True when the input column has accumulated a full batch.
    pub(super) fn should_flush(&self) -> bool {
        self.write_pos >= BATCH_SIZE
    }

    /// Append one input sample per channel at the current write cursor.
    pub(super) fn write<T: Scalar>(&mut self, input: &[T]) {
        let pos = self.write_pos;
        let n = input.len().min(self.inputs);
        for (ch, &sample) in input.iter().enumerate().take(n) {
            T::write_in(self, ch, pos, sample);
        }
        self.write_pos += 1;
    }

    /// Read one output sample per channel at the current read cursor.
    /// Silent during pre-roll (before the first flush).
    pub(super) fn read<T: Scalar>(&mut self, output: &mut [T]) {
        let pos = self.read_pos;
        if pos < self.filled {
            let n = output.len().min(self.outputs);
            for (ch, slot) in output.iter_mut().enumerate().take(n) {
                *slot = T::read_out(self, ch, pos);
            }
            for slot in output.iter_mut().skip(n) {
                *slot = T::default();
            }
        } else {
            for slot in output.iter_mut() {
                *slot = T::default();
            }
        }
        self.read_pos += 1;
    }

    /// Send inputs, process, receive outputs, reset cursors. Any bridge
    /// failure zero-fills `size` output samples and returns.
    /// Unpack a [`BlockPayload`] into the positional `bridge.process` call. The
    /// one place the host-side aggregate meets the IPC boundary; `midi_out` is
    /// the caller-owned sink the plugin's MIDI-out is drained into.
    fn dispatch(
        &self,
        bridge: &PluginBridge,
        seq: u64,
        size: usize,
        payload: BlockPayload,
        midi_out: &mut MidiEventVec,
    ) -> bool {
        bridge.submit(
            seq,
            size,
            payload.midi,
            payload.params,
            payload.note_expression,
            payload.harmony,
            payload.transport,
            midi_out,
        )
    }

    /// Whether block `expect_seq`'s output is really available to read.
    ///
    /// Two independent conditions, and the crash check is first on purpose. A
    /// crashed bridge never publishes, so the sequence would mismatch anyway and
    /// the audio would come out silent either way — but relying on that makes
    /// correct behaviour a *coincidence* of the sequence numbering rather than a
    /// decision. One relaxed atomic load, already on this path.
    fn collectable(&self, bridge: &PluginBridge) -> Option<u64> {
        if bridge.is_crashed() {
            return None;
        }
        let seq = self.expect_seq?;
        bridge.audio_buffer().has_output(seq).then_some(seq)
    }

    /// Finish submitting block `seq`: publish the input slot and hand the block
    /// to the bridge. Split from the per-channel staging above it only because
    /// the two paths stage differently (tick storage vs. a fundsp buffer) but
    /// submit identically.
    ///
    /// The publish happens **exactly once, after the last channel**. A
    /// per-channel publish would let the server observe the slot as valid while
    /// later channels are still being copied, and half of this block spliced
    /// onto half of the previous one sounds almost right — far worse than
    /// silence.
    ///
    /// `payload` bundles this block's host-produced inputs (see
    /// `crate::host::node::BlockPayload`). `note_expression` is default (empty):
    /// a live, spec-native channel the loaders DO consume (native
    /// `note_id`-addressed note-expression), but per-note expression currently
    /// reaches plugins via the MIDI-2 UMP stream converted at the format
    /// boundary — the field awaits a producer.
    fn submit(
        &mut self,
        bridge: &PluginBridge,
        seq: u64,
        size: usize,
        payload: BlockPayload,
        midi_out: &mut MidiEventVec,
    ) -> bool {
        bridge.audio_buffer().publish_input(seq);
        if !self.dispatch(bridge, seq, size, payload, midi_out) {
            return false;
        }
        self.next_seq += 1;
        self.expect_seq = Some(seq);
        true
    }

    pub(super) fn flush<T: Scalar>(
        &mut self,
        bridge: &PluginBridge,
        payload: BlockPayload,
        midi_out: &mut MidiEventVec,
    ) {
        let size = self.write_pos;
        if size == 0 {
            return;
        }

        // Decide what is collectable BEFORE overwriting the input ring with this
        // block — at ring depth 2, block N's input slot is the one block N-2
        // used.
        let collected = self.collectable(bridge);

        let seq = self.next_seq;
        let mut staged = true;
        for ch in 0..self.inputs {
            if T::stage_tick(self, bridge, seq, ch, size).is_err() {
                staged = false;
                break;
            }
        }
        let submitted = staged && self.submit(bridge, seq, size, payload, midi_out);

        match collected {
            Some(seq) => {
                for ch in 0..self.outputs {
                    T::recv_tick(self, bridge, seq, ch, size);
                }
            }
            // Nothing to collect: start-up, after a reset, a crashed bridge, or
            // a block the server never answered. All four are silence.
            None => T::silence_tick(self, size, self.outputs),
        }

        if !submitted {
            // The block we just failed to submit will never be collectable.
            self.expect_seq = None;
        }
        self.drain_to(size);
    }

    /// Block-mode counterpart of [`Self::flush`]. Bypasses tick storage —
    /// reads directly from `input`, writes directly to `output`.
    pub(super) fn process<T: Scalar>(
        &mut self,
        bridge: &PluginBridge,
        size: usize,
        input: &BufferRef<'_, T::Marker>,
        output: &mut BufferMut<'_, T::Marker>,
        payload: BlockPayload,
        midi_out: &mut MidiEventVec,
    ) {
        let collected = self.collectable(bridge);

        let seq = self.next_seq;
        let mut staged = true;
        for ch in 0..self.inputs {
            if T::stage_block(self, bridge, seq, ch, size, input).is_err() {
                staged = false;
                break;
            }
        }
        let submitted = staged && self.submit(bridge, seq, size, payload, midi_out);

        match collected {
            Some(collect_seq) => {
                for ch in 0..self.outputs {
                    T::recv_block(self, bridge, collect_seq, ch, size, output);
                }
            }
            None => T::silence_block(output, size, self.outputs),
        }

        if !submitted {
            self.expect_seq = None;
        }
    }

    fn drain_to(&mut self, size: usize) {
        self.filled = size;
        self.write_pos = 0;
        self.read_pos = 0;
    }

    /// Lock the tick storage to f32 on first call; panic in debug if it
    /// was already locked to f64 (fundsp doesn't switch scalar types
    /// mid-session on one Batcher instance).
    fn ensure_tick_f32(&mut self) {
        if !matches!(self.tick, TickStorage::F32 { .. }) {
            debug_assert!(
                matches!(self.tick, TickStorage::Unset),
                "fundsp scalar type switched mid-session on one Batcher"
            );
            self.tick = TickStorage::F32 {
                input: zero_channels(self.inputs, BATCH_SIZE),
                output: zero_channels(self.outputs, BATCH_SIZE),
            };
        }
    }

    fn ensure_tick_f64(&mut self) {
        if !matches!(self.tick, TickStorage::F64 { .. }) {
            debug_assert!(
                matches!(self.tick, TickStorage::Unset),
                "fundsp scalar type switched mid-session on one Batcher"
            );
            self.tick = TickStorage::F64 {
                input: zero_channels(self.inputs, BATCH_SIZE),
                output: zero_channels(self.outputs, BATCH_SIZE),
            };
        }
    }

    /// Borrow `tick` and `wire` disjointly, after ensuring `tick` is f32.
    fn tick_f32_and_wire(&mut self) -> (&mut Vec<Vec<f32>>, &mut Vec<Vec<f32>>, &mut WireStorage) {
        self.ensure_tick_f32();
        let TickStorage::F32 { input, output } = &mut self.tick else {
            unreachable!()
        };
        (input, output, &mut self.wire)
    }

    fn tick_f64_and_wire(&mut self) -> (&mut Vec<Vec<f64>>, &mut Vec<Vec<f64>>, &mut WireStorage) {
        self.ensure_tick_f64();
        let TickStorage::F64 { input, output } = &mut self.tick else {
            unreachable!()
        };
        (input, output, &mut self.wire)
    }
}

impl Scalar for f32 {
    type Marker = F32;

    fn write_in(batcher: &mut Batcher, ch: usize, pos: usize, value: Self) {
        batcher.ensure_tick_f32();
        let TickStorage::F32 { input, .. } = &mut batcher.tick else {
            unreachable!()
        };
        input[ch][pos] = value;
    }
    fn read_out(batcher: &Batcher, ch: usize, pos: usize) -> Self {
        match &batcher.tick {
            TickStorage::F32 { output, .. } => output[ch][pos],
            _ => 0.0,
        }
    }

    fn silence_tick(batcher: &mut Batcher, size: usize, outputs: usize) {
        batcher.ensure_tick_f32();
        let TickStorage::F32 { output, .. } = &mut batcher.tick else {
            unreachable!()
        };
        for buf in output.iter_mut().take(outputs) {
            let end = size.min(buf.len());
            buf[..end].fill(0.0);
        }
    }

    fn stage_tick(
        batcher: &mut Batcher,
        bridge: &PluginBridge,
        seq: u64,
        ch: usize,
        size: usize,
    ) -> Result<()> {
        let (input, _, wire) = batcher.tick_f32_and_wire();
        match wire {
            WireStorage::F32 { input: wire_in, .. } => {
                wire_in[..size].copy_from_slice(&input[ch][..size]);
                bridge.audio_buffer().write_input(seq, ch, &wire_in[..size])
            }
            WireStorage::F64 { input: wire_in, .. } => {
                for (d, &s) in wire_in[..size].iter_mut().zip(&input[ch][..size]) {
                    *d = s as f64;
                }
                bridge.audio_buffer().write_input(seq, ch, &wire_in[..size])
            }
        }
    }

    fn recv_tick(batcher: &mut Batcher, bridge: &PluginBridge, seq: u64, ch: usize, size: usize) {
        let (_, output, wire) = batcher.tick_f32_and_wire();
        let tick = &mut output[ch];
        match wire {
            WireStorage::F32 {
                output: wire_out, ..
            } => {
                let n = bridge
                    .audio_buffer()
                    .read_output_into(seq, ch, &mut wire_out[..size])
                    .unwrap_or(0);
                tick[..n].copy_from_slice(&wire_out[..n]);
                tick[n..size].fill(0.0);
            }
            WireStorage::F64 {
                output: wire_out, ..
            } => {
                let n = bridge
                    .audio_buffer()
                    .read_output_into(seq, ch, &mut wire_out[..size])
                    .unwrap_or(0);
                for (o, &v) in tick[..n].iter_mut().zip(&wire_out[..n]) {
                    *o = v as f32;
                }
                tick[n..size].fill(0.0);
            }
        }
    }

    fn stage_block(
        batcher: &mut Batcher,
        bridge: &PluginBridge,
        seq: u64,
        ch: usize,
        size: usize,
        input: &BufferRef<'_, F32>,
    ) -> Result<()> {
        match &mut batcher.wire {
            WireStorage::F32 { input: wire_in, .. } => {
                let wire = &mut wire_in[..size];
                for (i, slot) in wire.iter_mut().enumerate() {
                    *slot = input.at_f32(ch, i);
                }
                bridge.audio_buffer().write_input(seq, ch, wire)
            }
            WireStorage::F64 { input: wire_in, .. } => {
                let wire = &mut wire_in[..size];
                for (i, slot) in wire.iter_mut().enumerate() {
                    *slot = input.at_f32(ch, i) as f64;
                }
                bridge.audio_buffer().write_input(seq, ch, wire)
            }
        }
    }

    fn recv_block(
        batcher: &mut Batcher,
        bridge: &PluginBridge,
        seq: u64,
        ch: usize,
        size: usize,
        output: &mut BufferMut<'_, F32>,
    ) {
        match &mut batcher.wire {
            WireStorage::F32 {
                output: wire_out, ..
            } => {
                let wire = &mut wire_out[..size];
                let n = bridge
                    .audio_buffer()
                    .read_output_into(seq, ch, wire)
                    .unwrap_or(0);
                for (i, &v) in wire[..n].iter().enumerate() {
                    output.set_f32(ch, i, v);
                }
                for i in n..size {
                    output.set_f32(ch, i, 0.0);
                }
            }
            WireStorage::F64 {
                output: wire_out, ..
            } => {
                let wire = &mut wire_out[..size];
                let n = bridge
                    .audio_buffer()
                    .read_output_into(seq, ch, wire)
                    .unwrap_or(0);
                for (i, &v) in wire[..n].iter().enumerate() {
                    output.set_f32(ch, i, v as f32);
                }
                for i in n..size {
                    output.set_f32(ch, i, 0.0);
                }
            }
        }
    }

    fn silence_block(output: &mut BufferMut<'_, F32>, size: usize, outputs: usize) {
        for ch in 0..outputs {
            for i in 0..size {
                output.set_f32(ch, i, 0.0);
            }
        }
    }
}

impl Scalar for f64 {
    type Marker = F64;

    fn write_in(batcher: &mut Batcher, ch: usize, pos: usize, value: Self) {
        batcher.ensure_tick_f64();
        let TickStorage::F64 { input, .. } = &mut batcher.tick else {
            unreachable!()
        };
        input[ch][pos] = value;
    }
    fn read_out(batcher: &Batcher, ch: usize, pos: usize) -> Self {
        match &batcher.tick {
            TickStorage::F64 { output, .. } => output[ch][pos],
            _ => 0.0,
        }
    }

    fn silence_tick(batcher: &mut Batcher, size: usize, outputs: usize) {
        batcher.ensure_tick_f64();
        let TickStorage::F64 { output, .. } = &mut batcher.tick else {
            unreachable!()
        };
        for buf in output.iter_mut().take(outputs) {
            let end = size.min(buf.len());
            buf[..end].fill(0.0);
        }
    }

    fn stage_tick(
        batcher: &mut Batcher,
        bridge: &PluginBridge,
        seq: u64,
        ch: usize,
        size: usize,
    ) -> Result<()> {
        let (input, _, wire) = batcher.tick_f64_and_wire();
        match wire {
            WireStorage::F64 { input: wire_in, .. } => {
                wire_in[..size].copy_from_slice(&input[ch][..size]);
                bridge.audio_buffer().write_input(seq, ch, &wire_in[..size])
            }
            WireStorage::F32 { input: wire_in, .. } => {
                for (d, &s) in wire_in[..size].iter_mut().zip(&input[ch][..size]) {
                    *d = s as f32;
                }
                bridge.audio_buffer().write_input(seq, ch, &wire_in[..size])
            }
        }
    }

    fn recv_tick(batcher: &mut Batcher, bridge: &PluginBridge, seq: u64, ch: usize, size: usize) {
        let (_, output, wire) = batcher.tick_f64_and_wire();
        let tick = &mut output[ch];
        match wire {
            WireStorage::F64 {
                output: wire_out, ..
            } => {
                let n = bridge
                    .audio_buffer()
                    .read_output_into(seq, ch, &mut wire_out[..size])
                    .unwrap_or(0);
                tick[..n].copy_from_slice(&wire_out[..n]);
                tick[n..size].fill(0.0);
            }
            WireStorage::F32 {
                output: wire_out, ..
            } => {
                let n = bridge
                    .audio_buffer()
                    .read_output_into(seq, ch, &mut wire_out[..size])
                    .unwrap_or(0);
                for (o, &v) in tick[..n].iter_mut().zip(&wire_out[..n]) {
                    *o = v as f64;
                }
                tick[n..size].fill(0.0);
            }
        }
    }

    fn stage_block(
        batcher: &mut Batcher,
        bridge: &PluginBridge,
        seq: u64,
        ch: usize,
        size: usize,
        input: &BufferRef<'_, F64>,
    ) -> Result<()> {
        match &mut batcher.wire {
            WireStorage::F64 { input: wire_in, .. } => {
                let wire = &mut wire_in[..size];
                for (i, slot) in wire.iter_mut().enumerate() {
                    *slot = input.at_scalar(ch, i);
                }
                bridge.audio_buffer().write_input(seq, ch, wire)
            }
            WireStorage::F32 { input: wire_in, .. } => {
                let wire = &mut wire_in[..size];
                for (i, slot) in wire.iter_mut().enumerate() {
                    *slot = input.at_scalar(ch, i) as f32;
                }
                bridge.audio_buffer().write_input(seq, ch, wire)
            }
        }
    }

    fn recv_block(
        batcher: &mut Batcher,
        bridge: &PluginBridge,
        seq: u64,
        ch: usize,
        size: usize,
        output: &mut BufferMut<'_, F64>,
    ) {
        match &mut batcher.wire {
            WireStorage::F64 {
                output: wire_out, ..
            } => {
                let wire = &mut wire_out[..size];
                let n = bridge
                    .audio_buffer()
                    .read_output_into(seq, ch, wire)
                    .unwrap_or(0);
                for (i, &v) in wire[..n].iter().enumerate() {
                    output.set_scalar(ch, i, v);
                }
                for i in n..size {
                    output.set_scalar(ch, i, 0.0);
                }
            }
            WireStorage::F32 {
                output: wire_out, ..
            } => {
                let wire = &mut wire_out[..size];
                let n = bridge
                    .audio_buffer()
                    .read_output_into(seq, ch, wire)
                    .unwrap_or(0);
                for (i, &v) in wire[..n].iter().enumerate() {
                    output.set_scalar(ch, i, v as f64);
                }
                for i in n..size {
                    output.set_scalar(ch, i, 0.0);
                }
            }
        }
    }

    fn silence_block(output: &mut BufferMut<'_, F64>, size: usize, outputs: usize) {
        for ch in 0..outputs {
            for i in 0..size {
                output.set_scalar(ch, i, 0.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stage-4 input-port accounting: a 3-input-channel batcher (stereo main +
    /// mono sidechain) accepts writes on all three ports and stages them into
    /// tick storage. Port 2 is the sidechain — it must not be dropped, so a
    /// fundsp `connect(src, 0, target, 2)` reaches the plugin.
    #[test]
    fn write_accepts_sidechain_port() {
        let mut b = Batcher::new(3, 2, SampleFormat::Float32, 64);
        // One frame: distinct value per input port.
        b.write::<f32>(&[1.0, 2.0, 3.0]);
        match &b.tick {
            TickStorage::F32 { input, .. } => {
                assert_eq!(input.len(), 3, "all three input ports are stored");
                assert_eq!(input[0][0], 1.0);
                assert_eq!(input[1][0], 2.0);
                assert_eq!(input[2][0], 3.0, "sidechain port survived");
            }
            _ => panic!("expected f32 tick storage"),
        }
    }

    /// A fresh batcher has nothing in flight, so its first block must be silence
    /// rather than whatever the output slot happens to contain.
    #[test]
    fn a_fresh_batcher_expects_nothing() {
        let b = Batcher::new(2, 2, SampleFormat::Float32, 64);
        assert_eq!(b.expect_seq, None);
        assert_eq!(b.next_seq, 1, "0 means 'never published' in the slab");
    }

    /// The graph-commit clone carries the sequence forward rather than
    /// restarting.
    ///
    /// Restarting would be the subtle bug: a commit can land mid-playback (PDC
    /// re-plans), and a clone that began again at 1 would accept a *pre-commit*
    /// block's late publish as its own block 1. The in-flight block is dropped
    /// on purpose — the clone is a different object, and the original may still
    /// collect it.
    #[test]
    fn clone_preserves_the_pipeline_sequence() {
        let mut b = Batcher::new(3, 2, SampleFormat::Float32, 64);
        b.next_seq = 42;
        b.expect_seq = Some(41);

        let c = b.clone();
        assert_eq!(c.inputs, 3);
        assert_eq!(c.outputs, 2);
        assert_eq!(c.next_seq, 42, "a clone must not reuse a spent sequence");
        assert_eq!(c.expect_seq, None, "the in-flight block is not inherited");
    }

    /// `reset` (a seek) drops the in-flight block but must NOT rewind the
    /// sequence. Rewinding would let a block submitted before the seek be
    /// published after it and accepted as the new block 1, replaying a fragment
    /// of pre-seek audio.
    #[test]
    fn reset_drops_the_in_flight_block_without_rewinding() {
        let mut b = Batcher::new(2, 2, SampleFormat::Float32, 64);
        b.next_seq = 100;
        b.expect_seq = Some(99);

        b.reset();
        assert_eq!(b.expect_seq, None, "the pre-seek block is abandoned");
        assert_eq!(
            b.next_seq, 100,
            "the sequence is monotonic across a seek — this is what makes a \
             late pre-seek publish structurally unmatchable"
        );
    }

    /// The *consequence* of that monotonicity: no sequence issued after a seek
    /// can equal one issued before it, so a late pre-seek publish cannot be
    /// mistaken for post-seek audio however long it takes to arrive.
    ///
    /// `has_output` compares sequences for equality, so this disjointness is what
    /// actually stops the replay. The test above pins the two fields; this pins
    /// what they buy.
    ///
    /// Asserted here rather than end-to-end, deliberately. Driving a real seek
    /// through the mock server means suspending it mid-block so the held reply
    /// lands after the reset — but the mock is single-threaded, so a suspended
    /// server also stops draining the 128-slot command queue. Every post-seek
    /// submit is then rejected and the pipeline goes silent for a reason that has
    /// nothing to do with `reset`. Measured, not assumed: all 8 post-seek submits
    /// failed. A real subprocess keeps reading its socket however slow its DSP
    /// is, so that failure is an artifact of the harness and the end-to-end test
    /// would have been pinning the artifact.
    #[test]
    fn no_post_seek_sequence_can_collide_with_a_pre_seek_one() {
        let mut b = Batcher::new(2, 2, SampleFormat::Float32, 64);

        let mut pre_seek = Vec::new();
        for _ in 0..5 {
            pre_seek.push(b.next_seq);
            b.next_seq += 1;
        }
        // One of them is still in flight when the seek happens.
        b.expect_seq = Some(*pre_seek.last().unwrap());

        b.reset();

        let mut post_seek = Vec::new();
        for _ in 0..5 {
            post_seek.push(b.next_seq);
            b.next_seq += 1;
        }

        for issued in &post_seek {
            assert!(
                !pre_seek.contains(issued),
                "sequence {issued} was issued both before and after the seek: a \
                 pre-seek block publishing late would satisfy `has_output` for a \
                 post-seek block and replay its audio"
            );
        }
        assert!(
            post_seek[0] > *pre_seek.last().unwrap(),
            "post-seek sequences must continue past the pre-seek ones, not \
             restart alongside them"
        );
    }

    /// A change detector on the declared pipeline latency, and named as one.
    ///
    /// It compares two constants in this file, so it cannot fail unless someone
    /// edits one of them — it does not observe `route()` and proves nothing
    /// about what PDC receives. Constructing a `PluginClient` to check that
    /// needs a live subprocess, which is the integration suite's job, not this
    /// one's. An earlier comment here implied more coverage than that.
    ///
    /// It is kept because the two ways of getting this wrong are both silent
    /// and both a one-word edit away: the per-block `size` (unavailable to
    /// `route()`, and varying, so uncompensable) and `config.max_buffer_size`
    /// (8192 — 171 ms declared for a 1.33 ms pipeline). The bound below is the
    /// part that would catch the second one.
    #[test]
    fn declared_pipeline_latency_still_matches_the_block_size() {
        assert_eq!(PIPELINE_LATENCY_SAMPLES, BATCH_SIZE);
        // The failure mode worth naming: a max-buffer-sized declaration. Any
        // plausible block size is far below this; 8192 is far above it.
        assert!(
            PIPELINE_LATENCY_SAMPLES <= 1024,
            "a declared latency this large means `config.max_buffer_size` \
             (8192 = 171 ms at 48 kHz) reached PDC in place of the block size"
        );
    }
}
