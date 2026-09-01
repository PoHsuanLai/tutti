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

use crate::host::node::input_slot::{BlockCtx, InputSlot};
use crate::host::node::transport_source::TransportSource;
use crate::host::node::Midi;
use crate::protocol::{Features, TransportInfo};

/// Maximum block size the scratch buffers are pre-sized for. Matches fundsp's
/// `MAX_BUFFER_SIZE` so a single block lands in one `process()` call.
const BLOCK_SIZE: usize = 64;

/// Per-channel f32/f64 staging buffers + reusable ref vectors.
///
/// The `vst2-host` API takes `&[&[f32]]` / `&mut [&mut [f32]]`, so caller
/// samples are staged into owned contiguous `Vec<f32>` arrays and
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
    /// Per-block transport snapshot, gated on [`Features::TRANSPORT`]. The
    /// producer cell is shared across fundsp graph-commit clones (see
    /// [`InputSlot`]), so a `set_transport_source` on any clone reaches the one
    /// the audio thread runs.
    transport: InputSlot<TransportSource>,
    /// What the loader reported for this plugin, as the gate `transport` is
    /// drained against. Stored rather than passed in per block so the node's
    /// declared capability and its delivered behaviour read from one value.
    features: Features,
    /// Per-clone audio scratch handed to `vst::AudioBuffer::from_raw`.
    scratch: RenderScratch,
    /// Per-clone f32/f64 staging arrays (pre-allocated, reused).
    process_scratch: ProcessScratch,
    sample_rate: f64,
    /// A rate the graph handed this node that the plugin has not been told
    /// about yet, or [`NO_PENDING_RATE`] when there is none.
    ///
    /// `AudioUnit::set_sample_rate` arrives on the audio thread, and telling a
    /// VST2 plugin its rate means bracketing `effSetSampleRate` in
    /// `effMainsChanged` — the pair plugins allocate and free their
    /// rate-dependent buffers in. So the rate is parked here by
    /// [`queue_sample_rate`] and dispatched from the main thread by
    /// [`drain_sample_rate`], the same deferral the out-of-process client gets
    /// from its command queue.
    ///
    /// Shared across the fundsp graph-commit clones, so a rate reaching any
    /// clone is visible to whichever one the backend drains.
    pending_sample_rate: Arc<AtomicU64>,
    /// Bumped on every audio-thread `try_lock` failure. Shared across
    /// clones so the handle can read the global count.
    contention_count: Arc<AtomicU64>,
}

/// [`InProcessVst2Client::pending_sample_rate`] sentinel: nothing is waiting.
///
/// A rate is stored as `f64::to_bits`, so the sentinel must be a bit pattern no
/// rate produces. Zero is not one — `0.0f64.to_bits() == 0`, and a parked `0.0`
/// would then read as "nothing waiting". `u64::MAX` is a NaN payload, and a
/// rate that round-trips to NaN is not a rate.
pub(super) const NO_PENDING_RATE: u64 = u64::MAX;

/// Park `rate` for the main thread to dispatch. Audio-thread safe: one
/// `Relaxed` store, no lock and no allocation.
///
/// Last write wins. A rate superseded before anyone drained it never reached
/// the plugin, so dropping it loses nothing — the plugin only needs to hear the
/// rate it is about to run at.
pub(super) fn queue_sample_rate(cell: &AtomicU64, rate: f64) {
    cell.store(rate.to_bits(), Ordering::Relaxed);
}

/// Dispatch a parked rate, if any, and report whether one reached the plugin.
///
/// Main thread only: `Vst2Instance::set_sample_rate` runs the `effMainsChanged`
/// bracket, which allocates. This is the drain half of the deferral
/// [`queue_sample_rate`] opens.
///
/// The rate is claimed out of the cell before the lock is tried and put back if
/// the lock is unavailable — but only if nothing newer arrived meanwhile, so a
/// stale rate cannot overwrite a fresh one.
pub(super) fn drain_sample_rate(cell: &AtomicU64, inner: &Mutex<Vst2Instance>) -> bool {
    let bits = cell.swap(NO_PENDING_RATE, Ordering::Relaxed);
    if bits == NO_PENDING_RATE {
        return false;
    }
    match inner.try_lock() {
        Some(mut instance) => {
            instance.set_sample_rate(f64::from_bits(bits));
            true
        }
        None => {
            let _ =
                cell.compare_exchange(NO_PENDING_RATE, bits, Ordering::Relaxed, Ordering::Relaxed);
            false
        }
    }
}

impl InProcessVst2Client {
    pub(crate) fn new(
        inner: Arc<Mutex<Vst2Instance>>,
        metadata: PluginInfo,
        features: Features,
        sample_rate: f64,
        pending_sample_rate: Arc<AtomicU64>,
        contention_count: Arc<AtomicU64>,
    ) -> Self {
        let scratch = RenderScratch::new(metadata.num_inputs, metadata.num_outputs, BLOCK_SIZE);
        let process_scratch = ProcessScratch::new(
            metadata.num_inputs.count() as usize,
            metadata.num_outputs.count() as usize,
        );
        Self {
            inner,
            metadata,
            midi: Midi::new(),
            transport: InputSlot::new(Features::TRANSPORT),
            features,
            scratch,
            process_scratch,
            sample_rate,
            pending_sample_rate,
            contention_count,
        }
    }

    /// Producer handle for this plugin's MIDI inbox. Cheap to clone.
    pub fn midi_sender(&self) -> MidiSender {
        self.midi.sender()
    }

    /// Layer a transport-aware MIDI source over the live inbox, polled once per
    /// block. The clip-playback path, where [`midi_sender`](Self::midi_sender)
    /// is the live one; the port drains both.
    ///
    /// Held in an `Arc` so the same source survives the unit-clone fundsp
    /// performs on each `commit()`.
    pub fn set_midi_source(&mut self, source: Arc<dyn tutti_midi_types::MidiUnitIn>) {
        self.midi.set_source(source);
    }

    /// Drop a previously-installed source override; subsequent blocks poll the
    /// live `MidiReceiver` again.
    pub fn clear_midi_source(&mut self) {
        self.midi.clear_source();
    }

    /// Install a transport reader so the plugin receives a live per-block
    /// [`TransportInfo`] (tempo, playhead, meter, bar, loop), which the VST2
    /// host turns into the `audioMasterGetTime` snapshot the plugin polls.
    ///
    /// Wrapped in a `TransportSource` stamped with the current sample rate
    /// (updated live on a device change). The snapshot only reaches plugins
    /// advertising [`Features::TRANSPORT`]; others always drain a default.
    ///
    /// `meter` is a separate handle rather than something read off the
    /// transport: meter is a layer over the timeline, not transport state.
    /// Passing the same handle the host publishes elsewhere means a meter edit
    /// reaches running plugins without re-installing anything.
    pub fn set_transport_source(
        &mut self,
        reader: tutti_core::transport::Transport,
        meter: Arc<tutti_core::RtPublish<tutti_core::meter::MeterMap>>,
    ) {
        self.transport.install(Arc::new(TransportSource::new(
            Arc::new(reader),
            meter,
            self.sample_rate,
        )));
    }

    /// Drop a previously-installed transport reader; subsequent blocks feed the
    /// plugin a default (stopped) snapshot.
    pub fn clear_transport_source(&mut self) {
        self.transport.clear();
    }

    /// Restamp the installed transport source with a new sample rate.
    ///
    /// Called from both `AudioUnit::set_sample_rate` impls. The source holds its
    /// rate in a shared atomic, so this reaches the clone the audio thread runs;
    /// a no-op when no source is installed, since one installed later is stamped
    /// with `self.sample_rate` at that point.
    fn restamp_transport_rate(&self) {
        if let Some(src) = self.transport.source_ref().load().as_ref() {
            src.set_sample_rate(self.sample_rate);
        }
    }

    /// Install the outbound routing target so this plugin's MIDI-out re-enters
    /// the graph. See [`Midi::set_out`]. Off-RT; call once at wiring time.
    pub fn set_midi_out(&self, sink: Arc<tutti_midi_runtime::MidiOutSink>) {
        self.midi.set_out(sink);
    }

    /// Drop the outbound routing target; subsequent blocks discard MIDI-out.
    pub fn clear_midi_out(&self) {
        self.midi.clear_out();
    }

    /// Set the level reported through `audioMasterGetCurrentProcessLevel`.
    ///
    /// Always `true`: VST2 carries this on a host callback the plugin polls, so
    /// there is no query for a plugin to decline. Takes the lock rather than
    /// caching the flag locally — the answer lives on the `HostState` the
    /// plugin already holds, and a second copy here could disagree with it.
    pub fn set_render_mode(&self, mode: crate::protocol::RenderMode) -> bool {
        self.inner.lock().set_offline_render(mode.is_offline());
        true
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
        let process_scratch = ProcessScratch::new(
            self.metadata.num_inputs.count() as usize,
            self.metadata.num_outputs.count() as usize,
        );
        Self {
            inner: Arc::clone(&self.inner),
            metadata: self.metadata.clone(),
            midi: self.midi.clone(),
            // `InputSlot::clone` shares the producer cell rather than the
            // Option, so an install on any clone reaches the one fundsp runs.
            transport: self.transport.clone(),
            features: self.features,
            scratch,
            process_scratch,
            sample_rate: self.sample_rate,
            // Shared, not copied: fundsp clones the unit on every graph commit,
            // so a rate parked on one clone must be drainable through another.
            pending_sample_rate: Arc::clone(&self.pending_sample_rate),
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
        // Nothing reaches the plugin from here, and nothing can. VST 2.4 has no
        // opcode that clears DSP state on its own: the only two that touch it
        // are `effMainsChanged`, where plugins allocate and free their
        // rate-dependent buffers, and the `effStartProcess`/`effStopProcess`
        // pair, which announces an interruption rather than a clear and is only
        // legal while resumed. This call is on the audio thread, so neither is
        // available. `Vst2Instance::reset_processing_state` is that cycle, on
        // the main thread, for a host that wants it on a locate or a loop wrap.
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.sample_rate = sample_rate;
        self.restamp_transport_rate();
        // Parked, not dispatched: `Vst2Instance::set_sample_rate` runs the same
        // allocating `effMainsChanged` bracket, and this is the audio thread.
        queue_sample_rate(&self.pending_sample_rate, sample_rate);
    }

    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        // Single-sample tick reuses process() with size=1.
        for (ch, &sample) in input
            .iter()
            .enumerate()
            .take(self.metadata.num_inputs.count() as usize)
        {
            self.process_scratch.f32_in[ch][0] = sample;
        }
        let transport = *self
            .transport
            .drain(BlockCtx { block_size: 1 }, self.features);
        let processed = drive_f32(
            &self.inner,
            &self.contention_count,
            &mut self.midi,
            &transport,
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

        let transport = *self
            .transport
            .drain(BlockCtx { block_size: size }, self.features);
        let processed = drive_f32(
            &self.inner,
            &self.contention_count,
            &mut self.midi,
            &transport,
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
            self.metadata.latency_samples.get() as f64,
            input,
        )
    }

    /// The tail `tutti-vst2-host` decoded at load, from `effGetTailSize`.
    ///
    /// Read off the metadata rather than dispatched here: this runs on the
    /// audio thread, and the figure cannot change without a reload. `Unknown`
    /// stays the answer for a plugin that declines the opcode — VST2's raw `0`
    /// means "no information", not "no tail".
    fn tail(&mut self) -> tutti_plugin_types::PluginTail {
        self.metadata.tail
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
        // See the f32 impl: VST2 offers nothing here the audio thread may
        // dispatch.
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.sample_rate = sample_rate;
        self.restamp_transport_rate();
        queue_sample_rate(&self.pending_sample_rate, sample_rate);
    }

    fn tick(&mut self, input: &[f64], output: &mut [f64]) {
        for (ch, &sample) in input
            .iter()
            .enumerate()
            .take(self.metadata.num_inputs.count() as usize)
        {
            self.process_scratch.f64_in[ch][0] = sample;
        }
        let transport = *self
            .transport
            .drain(BlockCtx { block_size: 1 }, self.features);
        let processed = drive_f64(
            &self.inner,
            &self.contention_count,
            &mut self.midi,
            &transport,
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

        let transport = *self
            .transport
            .drain(BlockCtx { block_size: size }, self.features);
        let processed = drive_f64(
            &self.inner,
            &self.contention_count,
            &mut self.midi,
            &transport,
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
            self.metadata.latency_samples.get() as f64,
            input,
        )
    }

    /// The tail `tutti-vst2-host` decoded at load, from `effGetTailSize`.
    ///
    /// Read off the metadata rather than dispatched here: this runs on the
    /// audio thread, and the figure cannot change without a reload. `Unknown`
    /// stays the answer for a plugin that declines the opcode — VST2's raw `0`
    /// means "no information", not "no tail".
    fn tail(&mut self) -> tutti_plugin_types::PluginTail {
        self.metadata.tail
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

/// The per-block context handed to `vst2-host`, carrying this block's MIDI and
/// transport snapshot.
///
/// `transport` is always attached. The decision of *what* to attach happens one
/// level up, in [`InputSlot::drain`], which yields a default snapshot for a
/// plugin that did not declare [`Features::TRANSPORT`] or has no source
/// installed — so there is no second format check here.
///
/// Named rather than inlined at the two call sites so the shape of the context
/// the node builds is observable without a plugin binary on disk: whether
/// `ctx.transport` is populated at all is precisely what decides if
/// `audioMasterGetTime` has anything to serve.
fn block_context<'a>(
    sample_rate: f64,
    midi_events: &'a [tutti_vst2_host::MidiEvent],
    transport: &'a TransportInfo,
) -> ProcessContext<'a> {
    ProcessContext::new(sample_rate)
        .midi(midi_events)
        .transport(transport)
}

/// Reborrow the staging arrays as slice-of-slices and call into
/// `vst2-host`. A free function so it can take disjoint borrows of the
/// fields on the caller side without a self-borrow conflict.
fn drive_f32(
    inner: &Arc<Mutex<Vst2Instance>>,
    contention: &AtomicU64,
    midi: &mut Midi,
    transport: &TransportInfo,
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
            #[allow(
                clippy::needless_range_loop,
                reason = "`ch` indexes two parallel arrays; a zip would hide the \
                          MAX_CHANNELS bound the debug_asserts above pin"
            )]
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
                    let ctx = block_context(sample_rate, &midi_events, transport);
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

fn drive_f64(
    inner: &Arc<Mutex<Vst2Instance>>,
    contention: &AtomicU64,
    midi: &mut Midi,
    transport: &TransportInfo,
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
            #[allow(
                clippy::needless_range_loop,
                reason = "`ch` indexes two parallel arrays; a zip would hide the \
                          MAX_CHANNELS bound the debug_asserts above pin"
            )]
            for ch in 0..num_inputs.min(MAX_CHANNELS) {
                in_refs[ch] = &process_scratch.f64_in[ch][..size];
            }
            let in_slice = &in_refs[..num_inputs.min(MAX_CHANNELS)];

            run_with_mut_channels_f64(
                &mut process_scratch.f64_out[..num_outputs.min(MAX_CHANNELS)],
                size,
                |out_slice| {
                    let ctx = block_context(sample_rate, &midi_events, transport);
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


#[cfg(test)]
mod transport_tests {
    use super::*;
    use tutti_core::meter::MeterMap;
    use tutti_core::transport::Transport;
    use tutti_core::RtPublish;

    /// The slot exactly as [`InProcessVst2Client::new`] builds it. Constructing
    /// the whole node needs a live `Vst2Instance` (a real plugin binary on
    /// disk), so the transport rail is exercised on its own — it is the piece
    /// that was missing, and it is a pure function of the declared features.
    fn slot() -> InputSlot<TransportSource> {
        InputSlot::new(Features::TRANSPORT)
    }

    /// A rolling transport at `tempo`, plus the source the node installs for it.
    fn rolling(tempo: f64, rate: f64) -> (Transport, Arc<TransportSource>) {
        let t = Transport::new(rate);
        t.settings.set_tempo(tempo);
        let _ = t.motion.try_send(tutti_core::MotionEvent::Play);
        t.motion.drain();
        let source = Arc::new(TransportSource::new(
            Arc::new(t.clone()),
            Arc::new(RtPublish::new(MeterMap::default())),
            rate,
        ));
        (t, source)
    }

    /// The feature set the in-process loader builds, for a plugin reporting no
    /// optional capability of its own. `TRANSPORT` is unconditional there, so
    /// this is the minimum any in-process VST2 declares.
    fn loader_features() -> Features {
        let mut f = Features::empty();
        f.insert(Features::TRANSPORT);
        f
    }

    /// Whether a drained snapshot carries transport data rather than the
    /// "nothing installed" default.
    ///
    /// Compared field-wise against [`TransportInfo::default`] because the wire
    /// type has no `PartialEq`, and against the *default* rather than against
    /// zero because that default is a stopped transport at **120 BPM, 4/4** —
    /// not a zeroed struct. A predicate reading `tempo != 0.0` is therefore true
    /// for the default too, and would pass whether or not the snapshot was ever
    /// filled. Every fixture here runs at a tempo other than 120 so a live read
    /// is distinguishable from that default.
    fn is_live(info: &TransportInfo) -> bool {
        let default = TransportInfo::default();
        info.state.playing != default.state.playing
            || info.timing.tempo != default.timing.tempo
            || info.position.quarters != default.position.quarters
            || info.sample_rate != default.sample_rate
    }

    /// The context the node hands `vst2-host` each block carries a transport
    /// snapshot, which is the only thing that gives `audioMasterGetTime`
    /// something to serve.
    ///
    /// This is the claim [`Features::TRANSPORT`] makes. Before the node had a
    /// transport rail, `ctx.transport` was left `None` on every block, so
    /// `update_transport` never ran and the plugin's `get_time_info` callback
    /// kept reading an unpublished cell while the capability bit read true.
    #[test]
    fn the_block_context_carries_a_transport_snapshot() {
        let mut slot = slot();
        let (transport, source) = rolling(132.0, 48_000.0);
        transport.settings.set_beat(4.0);
        slot.install(source);
        let snapshot = *slot.drain(BlockCtx { block_size: 64 }, loader_features());

        let ctx = block_context(48_000.0, &[], &snapshot);

        let delivered = ctx
            .transport
            .expect("the block context must carry a transport snapshot");
        // 132, not the 120 the default carries — an assertion against 120 would
        // hold for an unfilled snapshot.
        assert!(
            (delivered.timing.tempo - 132.0).abs() < 1e-9,
            "the running tempo must reach the plugin, got {}",
            delivered.timing.tempo
        );
        assert!(
            delivered.state.playing,
            "a rolling transport must read as playing"
        );
        assert!(
            (delivered.position.quarters - 4.0).abs() < 1e-9,
            "the playhead must reach the plugin, got {}",
            delivered.position.quarters
        );
    }

    /// A node declaring `TRANSPORT` drains the live snapshot, not a default.
    #[test]
    fn a_node_declaring_transport_is_handed_the_live_snapshot() {
        let mut slot = slot();
        let (transport, source) = rolling(132.0, 48_000.0);
        transport.settings.set_beat(4.0);
        slot.install(source);

        let snapshot = slot.drain(BlockCtx { block_size: 64 }, loader_features());

        assert!(
            (snapshot.timing.tempo - 132.0).abs() < 1e-9,
            "declared TRANSPORT must deliver the running tempo, got {}",
            snapshot.timing.tempo
        );
        assert!(
            is_live(snapshot),
            "the snapshot must differ from the no-transport default"
        );
    }

    /// The declared bit is what gates delivery, so a node that does not declare
    /// `TRANSPORT` drains the default even with a source installed.
    ///
    /// Pins the gate as the reason delivery happens, rather than the mere
    /// presence of an installed source.
    #[test]
    fn a_node_not_declaring_transport_drains_the_default() {
        let mut slot = slot();
        let (transport, source) = rolling(132.0, 48_000.0);
        transport.settings.set_beat(4.0);
        slot.install(source);

        let snapshot = slot.drain(BlockCtx { block_size: 64 }, Features::empty());

        assert!(
            !is_live(snapshot),
            "an undeclared capability must not be fed"
        );
    }

    /// With no source installed the node still drains a usable default, so the
    /// `ProcessContext` is filled on every block rather than only once a host
    /// has wired a transport.
    #[test]
    fn an_uninstalled_slot_drains_the_default_snapshot() {
        let mut slot = slot();
        let snapshot = slot.drain(BlockCtx { block_size: 64 }, loader_features());
        assert!(!is_live(snapshot));
    }

    /// Installing on one clone reaches the clone the audio thread runs.
    ///
    /// fundsp clones the unit on every graph commit, and a host installs the
    /// transport through whichever clone it holds. Sharing the producer cell
    /// rather than the `Option` is what makes the install visible; the opposite
    /// is the shared-cell bug this rail already carries a regression guard for.
    #[test]
    fn a_transport_installed_on_one_clone_reaches_another() {
        let original = slot();
        let mut running = original.clone();
        let (transport, source) = rolling(90.0, 44_100.0);
        transport.settings.set_beat(1.0);

        original.install(source);

        let snapshot = running.drain(BlockCtx { block_size: 64 }, loader_features());
        assert!(
            (snapshot.timing.tempo - 90.0).abs() < 1e-9,
            "an install on a sibling clone must reach the running node"
        );
    }

    /// The declared capability and the delivered behaviour are the same value.
    ///
    /// The node gates its per-block send on `loaded.features`, the field the
    /// handle reports, so a reader cannot see `TRANSPORT` on the handle while
    /// the audio path silently withholds it.
    #[test]
    fn the_declared_transport_bit_is_the_one_the_send_is_gated_on() {
        let declared = loader_features();
        assert!(
            declared.contains(Features::TRANSPORT),
            "the in-process loader declares TRANSPORT unconditionally"
        );

        let mut slot = slot();
        let (transport, source) = rolling(96.0, 48_000.0);
        transport.settings.set_beat(2.0);
        slot.install(source);
        let snapshot = *slot.drain(BlockCtx { block_size: 64 }, declared);

        // Through `block_context`, so this reads the value the plugin is
        // actually handed rather than only the slot's output.
        let delivered = block_context(48_000.0, &[], &snapshot)
            .transport
            .is_some_and(is_live);
        assert_eq!(
            declared.contains(Features::TRANSPORT),
            delivered,
            "a declared transport capability must be a delivered one"
        );
    }
}