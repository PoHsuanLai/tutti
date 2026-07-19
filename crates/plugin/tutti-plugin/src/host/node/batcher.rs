//! [`Batcher`] — buffers one `BATCH_SIZE` block of per-channel audio
//! between the caller and the plugin-server bridge.
//!
//! `write()` accumulates one input sample per channel per call. Once
//! `should_flush()` is true, `flush()` ships the whole block across the
//! bridge, stages the server's reply into output storage, and resets
//! cursors so `read()` can replay the outputs sample-by-sample. `process()`
//! is the block-in/block-out variant that bypasses tick storage entirely.
//!
//! Dual f32/f64 support: fundsp drives a `PluginClient` through either
//! `AudioUnit<F32>` or `AudioUnit<F64>` (runtime choice). The wire format
//! is the one the plugin negotiated (independent of fundsp's choice), so
//! the tick scalar and wire scalar can differ — cross-format conversion
//! happens in the wire scratch. Each of `tick` and `wire` is a
//! format-tagged enum; only one variant exists per `Batcher` instance at
//! runtime.

use crate::host::ipc_client::audio::HarmonyInputs;
use crate::host::ipc_client::PluginBridge;
use crate::error::Result;
use crate::protocol::{
    MidiEventVec, NoteExpressionChanges, ParameterChanges, SampleFormat, TransportInfo,
};
use tutti_core::{BufferMut, BufferRef, Sample as FundspSample, F32, F64};

/// Matches fundsp's `MAX_BUFFER_SIZE` (`1 << 6`). Blocks of this size are
/// fundsp's natural unit and what the plugin server is sized for.
pub(super) const BATCH_SIZE: usize = 64;

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

    fn send_tick(
        batcher: &mut Batcher,
        bridge: &PluginBridge,
        ch: usize,
        size: usize,
    ) -> Result<()>;
    /// Zero-pad if the bridge returns fewer than `size` samples.
    fn recv_tick(batcher: &mut Batcher, bridge: &PluginBridge, ch: usize, size: usize);

    fn send_block(
        batcher: &mut Batcher,
        bridge: &PluginBridge,
        ch: usize,
        size: usize,
        input: &BufferRef<'_, Self::Marker>,
    ) -> Result<()>;
    fn recv_block(
        batcher: &mut Batcher,
        bridge: &PluginBridge,
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

/// One block of audio between fundsp callers and the plugin-server bridge.
pub(crate) struct Batcher {
    /// Total input ports (main + sidechain/aux input buses). Each port `ch` is
    /// written to flat slab channel `ch` (bus-ordered, base 0), so a fundsp
    /// `connect(src, 0, target, 1)` feeds the plugin's sidechain bus.
    pub(super) inputs: usize,
    pub(super) outputs: usize,
    /// Flat-channel base for the OUTPUT direction in the slab. Multi-bus slabs
    /// place outputs after the inputs (= total input channels) so the in-place
    /// output write never clobbers a sidechain input; 0 for single-bus legacy.
    output_base: usize,
    format: SampleFormat,

    tick: TickStorage,
    wire: WireStorage,

    // Shared across both tick paths — only one is ever live per instance.
    write_pos: usize,
    read_pos: usize,
    filled: usize,

    max_block: usize,
}

impl Clone for Batcher {
    fn clone(&self) -> Self {
        // Fresh buffers, reset cursors. Safe because fundsp clones on
        // graph commit before processing starts — no in-flight samples.
        Self::new(
            self.inputs,
            self.outputs,
            self.output_base,
            self.format,
            self.max_block,
        )
    }
}

impl Batcher {
    pub(super) fn new(
        inputs: usize,
        outputs: usize,
        output_base: usize,
        format: SampleFormat,
        max_block: usize,
    ) -> Self {
        Self {
            inputs,
            outputs,
            output_base,
            format,
            tick: TickStorage::Unset,
            wire: WireStorage::new(format, max_block),
            write_pos: 0,
            read_pos: 0,
            filled: 0,
            max_block,
        }
    }

    pub(super) fn reset(&mut self) {
        self.write_pos = 0;
        self.read_pos = 0;
        self.filled = 0;
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
    pub(super) fn flush<T: Scalar>(
        &mut self,
        bridge: &PluginBridge,
        midi: MidiEventVec,
        harmony: HarmonyInputs,
        transport: TransportInfo,
    ) {
        let size = self.write_pos;
        if size == 0 {
            return;
        }

        for ch in 0..self.inputs {
            if T::send_tick(self, bridge, ch, size).is_err() {
                T::silence_tick(self, size, self.outputs);
                self.drain_to(size);
                return;
            }
        }

        if !bridge.process(
            size,
            midi,
            ParameterChanges::new(),
            NoteExpressionChanges::new(),
            harmony,
            transport,
        ) {
            T::silence_tick(self, size, self.outputs);
            self.drain_to(size);
            return;
        }

        for ch in 0..self.outputs {
            T::recv_tick(self, bridge, ch, size);
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
        midi: MidiEventVec,
        harmony: HarmonyInputs,
        transport: TransportInfo,
    ) {
        for ch in 0..self.inputs {
            if T::send_block(self, bridge, ch, size, input).is_err() {
                T::silence_block(output, size, self.outputs);
                return;
            }
        }

        if !bridge.process(
            size,
            midi,
            ParameterChanges::new(),
            NoteExpressionChanges::new(),
            harmony,
            transport,
        ) {
            T::silence_block(output, size, self.outputs);
            return;
        }

        for ch in 0..self.outputs {
            T::recv_block(self, bridge, ch, size, output);
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

    fn send_tick(
        batcher: &mut Batcher,
        bridge: &PluginBridge,
        ch: usize,
        size: usize,
    ) -> Result<()> {
        let (input, _, wire) = batcher.tick_f32_and_wire();
        match wire {
            WireStorage::F32 { input: wire_in, .. } => {
                wire_in[..size].copy_from_slice(&input[ch][..size]);
                bridge.audio_buffer().write_channel(ch, &wire_in[..size])
            }
            WireStorage::F64 { input: wire_in, .. } => {
                for (d, &s) in wire_in[..size].iter_mut().zip(&input[ch][..size]) {
                    *d = s as f64;
                }
                bridge.audio_buffer().write_channel(ch, &wire_in[..size])
            }
        }
    }

    fn recv_tick(batcher: &mut Batcher, bridge: &PluginBridge, ch: usize, size: usize) {
        let slab_ch = batcher.output_base + ch;
        let (_, output, wire) = batcher.tick_f32_and_wire();
        let tick = &mut output[ch];
        match wire {
            WireStorage::F32 {
                output: wire_out, ..
            } => {
                let n = bridge
                    .audio_buffer()
                    .read_channel_into(slab_ch, &mut wire_out[..size])
                    .unwrap_or(0);
                tick[..n].copy_from_slice(&wire_out[..n]);
                tick[n..size].fill(0.0);
            }
            WireStorage::F64 {
                output: wire_out, ..
            } => {
                let n = bridge
                    .audio_buffer()
                    .read_channel_into(slab_ch, &mut wire_out[..size])
                    .unwrap_or(0);
                for (o, &v) in tick[..n].iter_mut().zip(&wire_out[..n]) {
                    *o = v as f32;
                }
                tick[n..size].fill(0.0);
            }
        }
    }

    fn send_block(
        batcher: &mut Batcher,
        bridge: &PluginBridge,
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
                bridge.audio_buffer().write_channel(ch, wire)
            }
            WireStorage::F64 { input: wire_in, .. } => {
                let wire = &mut wire_in[..size];
                for (i, slot) in wire.iter_mut().enumerate() {
                    *slot = input.at_f32(ch, i) as f64;
                }
                bridge.audio_buffer().write_channel(ch, wire)
            }
        }
    }

    fn recv_block(
        batcher: &mut Batcher,
        bridge: &PluginBridge,
        ch: usize,
        size: usize,
        output: &mut BufferMut<'_, F32>,
    ) {
        let slab_ch = batcher.output_base + ch;
        match &mut batcher.wire {
            WireStorage::F32 {
                output: wire_out, ..
            } => {
                let wire = &mut wire_out[..size];
                let n = bridge
                    .audio_buffer()
                    .read_channel_into(slab_ch, wire)
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
                    .read_channel_into(slab_ch, wire)
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

    fn send_tick(
        batcher: &mut Batcher,
        bridge: &PluginBridge,
        ch: usize,
        size: usize,
    ) -> Result<()> {
        let (input, _, wire) = batcher.tick_f64_and_wire();
        match wire {
            WireStorage::F64 { input: wire_in, .. } => {
                wire_in[..size].copy_from_slice(&input[ch][..size]);
                bridge.audio_buffer().write_channel(ch, &wire_in[..size])
            }
            WireStorage::F32 { input: wire_in, .. } => {
                for (d, &s) in wire_in[..size].iter_mut().zip(&input[ch][..size]) {
                    *d = s as f32;
                }
                bridge.audio_buffer().write_channel(ch, &wire_in[..size])
            }
        }
    }

    fn recv_tick(batcher: &mut Batcher, bridge: &PluginBridge, ch: usize, size: usize) {
        let slab_ch = batcher.output_base + ch;
        let (_, output, wire) = batcher.tick_f64_and_wire();
        let tick = &mut output[ch];
        match wire {
            WireStorage::F64 {
                output: wire_out, ..
            } => {
                let n = bridge
                    .audio_buffer()
                    .read_channel_into(slab_ch, &mut wire_out[..size])
                    .unwrap_or(0);
                tick[..n].copy_from_slice(&wire_out[..n]);
                tick[n..size].fill(0.0);
            }
            WireStorage::F32 {
                output: wire_out, ..
            } => {
                let n = bridge
                    .audio_buffer()
                    .read_channel_into(slab_ch, &mut wire_out[..size])
                    .unwrap_or(0);
                for (o, &v) in tick[..n].iter_mut().zip(&wire_out[..n]) {
                    *o = v as f64;
                }
                tick[n..size].fill(0.0);
            }
        }
    }

    fn send_block(
        batcher: &mut Batcher,
        bridge: &PluginBridge,
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
                bridge.audio_buffer().write_channel(ch, wire)
            }
            WireStorage::F32 { input: wire_in, .. } => {
                let wire = &mut wire_in[..size];
                for (i, slot) in wire.iter_mut().enumerate() {
                    *slot = input.at_scalar(ch, i) as f32;
                }
                bridge.audio_buffer().write_channel(ch, wire)
            }
        }
    }

    fn recv_block(
        batcher: &mut Batcher,
        bridge: &PluginBridge,
        ch: usize,
        size: usize,
        output: &mut BufferMut<'_, F64>,
    ) {
        let slab_ch = batcher.output_base + ch;
        match &mut batcher.wire {
            WireStorage::F64 {
                output: wire_out, ..
            } => {
                let wire = &mut wire_out[..size];
                let n = bridge
                    .audio_buffer()
                    .read_channel_into(slab_ch, wire)
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
                    .read_channel_into(slab_ch, wire)
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
        // inputs = main(2) + sidechain(1); output_base = 3 (output after input).
        let mut b = Batcher::new(3, 2, 3, SampleFormat::Float32, 64);
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

    /// `output_base` is preserved across the fundsp graph-commit clone (which
    /// rebuilds the batcher), so the cloned node still reads outputs from the
    /// slab's output range rather than aliasing the inputs.
    #[test]
    fn clone_preserves_output_base() {
        let b = Batcher::new(3, 2, 3, SampleFormat::Float32, 64);
        let c = b.clone();
        assert_eq!(c.inputs, 3);
        assert_eq!(c.outputs, 2);
        assert_eq!(c.output_base, 3);
    }

    /// Single-bus legacy: output_base 0 keeps inputs and outputs sharing the
    /// flat channel range in-place (today's behaviour).
    #[test]
    fn legacy_output_base_is_zero() {
        let b = Batcher::new(2, 2, 0, SampleFormat::Float32, 64);
        assert_eq!(b.output_base, 0);
    }
}
