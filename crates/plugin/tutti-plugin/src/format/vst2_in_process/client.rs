//! `InProcessVst2Client`: the graph node (`node.rs`) that drives a VST2 plugin
//! from the host audio thread.
//!
//! The instance lives behind `Arc<Mutex<tutti_vst2_host::Vst2Instance>>` shared
//! with the matching control backend. Audio thread acquires with
//! `try_lock`; on contention it falls back to silence and bumps
//! [`InProcessVst2Client::contention_count`].
//!
//! All per-block scratch — channel buffers, ref-vector storage, MIDI
//! drain — is sized in the node's `prepare`, to the graph's largest block.
//! A block is allocation-free in steady state (verified by the
//! `assert_no_alloc` test in `tests/`).

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use tutti_core::{MeterMap, RtPublish, Samples};
use tutti_graph::{Cx, Offset};
use tutti_midi_types::ump::MidiEvent;
use tutti_vst2_host::{PluginInfo, RenderScratch, Vst2Instance, Vst2ProcessContext};

use crate::host::node::transport_source::{self, SteadyTime};
use crate::host::node::PluginMeter;
use crate::protocol::{Features, MidiEventVec, TransportInfo};

/// Per-channel f32 staging buffers.
///
/// The `vst2-host` API takes `&[&[f32]]` / `&mut [&mut [f32]]`, so caller
/// samples are staged into owned contiguous `Vec<f32>` arrays and
/// reborrow them as slice-of-slices each call. Sized in the node's
/// `prepare` to the graph's largest block (`frames`), never on the audio
/// thread.
pub(super) struct ProcessScratch {
    pub(super) f32_in: Vec<Vec<f32>>,
    pub(super) f32_out: Vec<Vec<f32>>,
}

impl ProcessScratch {
    fn new(num_inputs: usize, num_outputs: usize, frames: usize) -> Self {
        Self {
            f32_in: (0..num_inputs).map(|_| vec![0.0; frames]).collect(),
            f32_out: (0..num_outputs).map(|_| vec![0.0; frames]).collect(),
        }
    }
}

/// A VST2 plugin running inside the host process, as a graph node.
///
/// Created by [`in_process_vst2_client`](crate::in_process_vst2_client), or by
/// [`Plugin::open`](crate::catalog::Plugin::open) for a `.vst`/`.dll`/`.so`
/// file when the `vst2` feature is enabled. MIDI arrives on the node's event
/// input and the plugin's MIDI-out leaves on an event output (for a plugin that
/// declares [`Features::MIDI_OUT`]).
///
/// There is no process isolation: a plugin crash takes the host down. The
/// audio thread takes the instance lock with `try_lock` and outputs silence on
/// contention (counted by [`contention_count`](Self::contention_count)). Steady
/// state processing does not allocate.
///
/// A graph holding this node forks (`Editor::fork`, which an offline export
/// takes) **by state transfer**: a second instance of the plugin, from the same
/// library, restored from this one's saved state. See
/// [`fork_instance`](Self::fork_instance).
pub struct InProcessVst2Client {
    pub(super) inner: Arc<Mutex<Vst2Instance>>,
    pub(super) metadata: PluginInfo,
    /// The block's MIDI in: its event input's.
    pub(super) midi: MidiEventVec,
    /// The block's transport, read from the graph's [`Cx`] (live and in a
    /// fork alike), with the project meter installed through
    /// [`set_meter`](Self::set_meter).
    pub(super) transport: TransportFeed,
    /// What the loader reported for this plugin, as the gate `transport` is
    /// read against. Stored rather than passed in per block so the node's
    /// declared capability and its delivered behaviour read from one value.
    pub(super) features: Features,
    /// Per-clone audio scratch handed to `vst::AudioBuffer::from_raw`.
    pub(super) scratch: RenderScratch,
    /// Per-clone f32 staging arrays (sized in `prepare`, reused).
    pub(super) process_scratch: ProcessScratch,
    /// The block length `scratch` and `process_scratch` are sized for: the
    /// largest block the node was prepared for (zero before it is).
    pub(super) frames: usize,
    pub(super) sample_rate: f64,
    /// A rate the graph handed this node that the plugin has not been told
    /// about yet, or [`NO_PENDING_RATE`] when there is none.
    ///
    /// Telling a VST2 plugin its rate means bracketing `effSetSampleRate` in
    /// `effMainsChanged` — the pair plugins allocate and free their
    /// rate-dependent buffers in, and which must not race the audio
    /// thread's `process` on a plugin that is running. So the rate the node
    /// is prepared at is parked here by [`queue_sample_rate`] and dispatched
    /// from the main thread by [`drain_sample_rate`] (the editor idle pump),
    /// the same deferral the out-of-process client gets from its command
    /// queue.
    ///
    /// Shared across clones, so a rate reaching any clone is visible to
    /// whichever one the backend drains.
    pending_sample_rate: Arc<AtomicU64>,
    /// Bumped on every audio-thread `try_lock` failure. Shared across
    /// clones so the handle can read the global count.
    pub(super) contention_count: Arc<AtomicU64>,
    /// The file the plugin was loaded from: what a fork loads again.
    pub(super) origin: Arc<Path>,
    /// Who tells the plugin the rate the node is prepared at.
    pub(super) rate_delivery: RateDelivery,
}

/// Who tells the plugin the rate the node is prepared at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RateDelivery {
    /// Parked in `pending_sample_rate` for the main thread's editor pump
    /// (`InProcessVst2Backend::editor_idle`) to dispatch: the live instance,
    /// whose `effMainsChanged` bracket belongs on the main thread.
    Parked,
    /// Dispatched by `prepare` itself, under the instance lock: a fork. Nothing
    /// pumps a fork's editor (it has no handle), so a parked rate would never
    /// reach it; and its `prepare` runs on the control thread that forks it,
    /// with the unit out of any executor (`Node::prepare`'s contract), before
    /// its first block.
    InPrepare,
}

/// The transport a block hands the plugin: what `audioMasterGetTime`
/// answers while the plugin processes it.
///
/// One path for the live node and its fork: the transport the block's input
/// was rendered at ([`Cx::transport_at`] at its first frame, the node's
/// compiled arrival latency earlier), mapped the way the out-of-process
/// plugin node maps it ([`transport_source::from_cx`]), with the project
/// meter for the signature and bar. For an offline fork the `Cx` is the
/// render's. The snapshot is built in `process` before the plugin is
/// entered: `Vst2Instance::process_f32` publishes it to the cell the
/// callback reads, then calls the plugin.
#[derive(Clone)]
pub(super) struct TransportFeed {
    /// The project meter, read once per block. The cell is shared across the
    /// live node's clones, so a [`set_meter`](InProcessVst2Client::set_meter)
    /// on any clone reaches the one the audio thread runs; a fork has a cell
    /// of its own ([`forked`](Self::forked)).
    meter: PluginMeter,
    /// The meter read when none is installed (4/4 from bar 0), held so a
    /// block never builds one.
    default_meter: MeterMap,
    /// The free-running sample counter: frames this node rendered.
    steady: SteadyTime,
    /// The last snapshot handed out: what an empty block (no frame to read
    /// the transport at) repeats.
    last: TransportInfo,
}

impl TransportFeed {
    /// No meter installed, nothing rendered.
    pub(super) fn new() -> Self {
        Self {
            meter: PluginMeter::default(),
            default_meter: MeterMap::default(),
            steady: SteadyTime::default(),
            last: TransportInfo::default(),
        }
    }

    /// A fork's feed: the meter installed now, in a cell of its own (a meter
    /// installed on the live node afterwards does not reach the fork, as for
    /// the out-of-process node's fork), and a counter from zero.
    pub(super) fn forked(&self) -> Self {
        Self {
            meter: PluginMeter::holding(self.meter.load_full()),
            default_meter: MeterMap::default(),
            steady: SteadyTime::default(),
            last: TransportInfo::default(),
        }
    }

    /// The snapshot for a block of `frames`, then count them. A plugin that
    /// did not declare [`Features::TRANSPORT`] is handed the default
    /// (stopped) snapshot; an empty block, which has no frame to read the
    /// transport at, repeats the last one. Audio thread: two lock-free loads
    /// (the meter's cell, then its value), no allocation.
    pub(super) fn block(
        &mut self,
        cx: &Cx<'_>,
        frames: usize,
        features: Features,
    ) -> TransportInfo {
        if !features.contains(Features::TRANSPORT) {
            self.steady.advance(frames);
            return TransportInfo::default();
        }
        let Some(at) = Offset::new(0, Samples(frames)) else {
            return self.last;
        };
        let slot = self.meter.load();
        let meter = slot.as_ref().map(|m| m.read());
        self.last = transport_source::from_cx(
            cx,
            at,
            self.steady.at(0),
            meter.as_deref().unwrap_or(&self.default_meter),
        );
        self.steady.advance(frames);
        self.last
    }
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
        origin: Arc<Path>,
    ) -> Self {
        // Empty until the node is prepared: `prepare` sizes both to the
        // graph's largest block.
        let scratch = RenderScratch::new(metadata.num_inputs, metadata.num_outputs, 0);
        let process_scratch = ProcessScratch::new(
            metadata.num_inputs.count() as usize,
            metadata.num_outputs.count() as usize,
            0,
        );
        Self {
            inner,
            metadata,
            midi: MidiEventVec::new(),
            transport: TransportFeed::new(),
            features,
            scratch,
            process_scratch,
            frames: 0,
            sample_rate,
            pending_sample_rate,
            contention_count,
            origin,
            rate_delivery: RateDelivery::Parked,
        }
    }

    /// Gives the plugin the project meter, for the time signature and bar
    /// its `audioMasterGetTime` snapshot carries. Pass the cell the host
    /// publishes meter edits into, and an edit reaches the running plugin
    /// with nothing re-installed. Replaces any meter installed before, on
    /// every clone of this node.
    ///
    /// The transport itself needs no install: each block reads it from the
    /// graph (the transport the block's input was rendered at). The snapshot
    /// reaches plugins declaring [`Features::TRANSPORT`], which the
    /// in-process loader declares for every VST2.
    pub fn set_meter(&self, meter: Arc<RtPublish<MeterMap>>) {
        self.transport.meter.set_meter(meter);
    }

    /// Drops the meter; the plugin is told 4/4 from bar 0.
    pub fn clear_meter(&self) {
        self.transport.meter.clear_meter();
    }

    /// Whether a meter is installed.
    pub fn has_meter(&self) -> bool {
        self.transport.meter.has_meter()
    }

    /// The node's meter cell, as a handle of its own: the same cell as
    /// [`set_meter`](Self::set_meter), and what inserting the node hands
    /// back as its controls, so a host can install the meter after the
    /// node has moved into the graph.
    pub fn meter(&self) -> PluginMeter {
        self.transport.meter.clone()
    }

    /// Sets the render mode the plugin reads through
    /// `audioMasterGetCurrentProcessLevel`.
    ///
    /// Always returns `true`: VST2 carries this on a host callback the plugin
    /// polls, so there is nothing for a plugin to decline. Takes the instance
    /// lock, so call it from a control thread, not the audio thread.
    //
    // Takes the lock rather than caching the flag locally: the answer lives on
    // the `HostState` the plugin already holds, and a second copy could
    // disagree with it.
    pub fn set_render_mode(&self, mode: crate::protocol::RenderMode) -> bool {
        self.inner.lock().set_offline_render(mode.is_offline());
        true
    }

    /// Returns how many times the audio thread found the instance locked and
    /// output silence instead, since construction.
    ///
    /// Shared across clones; intended for diagnostics.
    pub fn contention_count(&self) -> u64 {
        self.contention_count.load(Ordering::Relaxed)
    }
}

impl Clone for InProcessVst2Client {
    fn clone(&self) -> Self {
        // Arc-clone the live plugin; allocate fresh scratch for this clone
        // (matches Batcher::clone in the subprocess client), at the frames
        // this one is sized for. Done at clone time, not on the audio thread.
        let scratch = RenderScratch::new(
            self.metadata.num_inputs,
            self.metadata.num_outputs,
            self.frames,
        );
        let process_scratch = ProcessScratch::new(
            self.metadata.num_inputs.count() as usize,
            self.metadata.num_outputs.count() as usize,
            self.frames,
        );
        Self {
            inner: Arc::clone(&self.inner),
            metadata: self.metadata.clone(),
            midi: MidiEventVec::new(),
            // Shares the meter cell, so a `set_meter` on any clone reaches
            // the one the graph runs.
            transport: self.transport.clone(),
            features: self.features,
            scratch,
            process_scratch,
            frames: self.frames,
            sample_rate: self.sample_rate,
            // Shared, not copied: a rate parked on one clone must be drainable
            // through another.
            pending_sample_rate: Arc::clone(&self.pending_sample_rate),
            contention_count: Arc::clone(&self.contention_count),
            origin: Arc::clone(&self.origin),
            rate_delivery: self.rate_delivery,
        }
    }
}

impl InProcessVst2Client {
    /// Size the scratch for blocks of up to `frames`: a no-op when it
    /// already is. Control thread (the node's `prepare`): it allocates.
    pub(super) fn ensure_scratch_size(&mut self, frames: usize) {
        if frames <= self.frames {
            return;
        }
        self.frames = frames;
        self.scratch =
            RenderScratch::new(self.metadata.num_inputs, self.metadata.num_outputs, frames);
        for ch in self
            .process_scratch
            .f32_in
            .iter_mut()
            .chain(self.process_scratch.f32_out.iter_mut())
        {
            ch.resize(frames, 0.0);
        }
    }

    /// Take `sample_rate` as the rate blocks run at, and hand it to the
    /// plugin as [`RateDelivery`] says: parked for the main thread (see
    /// `pending_sample_rate`), or, on a fork, dispatched here. What the
    /// node's `prepare` does with its rate. (The transport snapshot's rate is
    /// the block's own, from its `Env`.)
    pub(super) fn set_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        let changed = sample_rate != self.sample_rate;
        self.sample_rate = sample_rate;
        match self.rate_delivery {
            // Parked, not dispatched: `Vst2Instance::set_sample_rate` runs the
            // allocating `effMainsChanged` bracket, which must not race a
            // block the audio thread is rendering through the live plugin.
            RateDelivery::Parked => queue_sample_rate(&self.pending_sample_rate, sample_rate),
            // Only on a change: the fork was loaded at `self.sample_rate`, and
            // a redundant bracket makes a plugin reallocate its buffers.
            RateDelivery::InPrepare if changed => {
                self.inner.lock().set_sample_rate(sample_rate);
            }
            RateDelivery::InPrepare => {}
        }
    }
}

/// The per-block context handed to `vst2-host`, carrying this block's MIDI and
/// transport snapshot.
///
/// `transport` is always attached. The decision of *what* to attach happens one
/// level up, in [`TransportFeed::block`], which yields a default snapshot for a
/// plugin that did not declare [`Features::TRANSPORT`] — so there is no second
/// format check here.
///
/// Named rather than inlined at the two call sites so the shape of the context
/// the node builds is observable without a plugin binary on disk: whether
/// `ctx.transport` is populated at all is precisely what decides if
/// `audioMasterGetTime` has anything to serve.
pub(super) fn block_context<'a>(
    sample_rate: f64,
    midi_events: &'a [tutti_vst2_host::MidiEvent],
    transport: &'a TransportInfo,
) -> Vst2ProcessContext<'a> {
    Vst2ProcessContext::new(sample_rate)
        .midi(midi_events)
        .transport(transport)
}

/// Reborrow the staging arrays as slice-of-slices and call into
/// `vst2-host`. A free function so it can take disjoint borrows of the
/// fields on the caller side without a self-borrow conflict.
pub(super) fn drive_f32(
    inner: &Arc<Mutex<Vst2Instance>>,
    contention: &AtomicU64,
    midi: &mut MidiEventVec,
    midi_out: &mut dyn FnMut(&[MidiEvent]),
    transport: &TransportInfo,
    scratch: &mut RenderScratch,
    process_scratch: &mut ProcessScratch,
    num_inputs: usize,
    num_outputs: usize,
    size: usize,
    sample_rate: f64,
) -> bool {
    // Taken whether or not the plugin runs: a contended block drops its
    // MIDI rather than play it a block late. Sorted, stably: a queue may hold
    // it out of order.
    let mut midi_events = std::mem::take(midi);
    sort_midi(&mut midi_events);
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
                    let out = instance.process_f32(in_slice, out_slice, size, &ctx, scratch);
                    // The plugin's MIDI-out, each on its frame_offset.
                    midi_out(out);
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

/// Sort `events` by frame offset, stably, in place: an insertion sort, for a
/// short list, nearly sorted. Allocation-free.
fn sort_midi(events: &mut MidiEventVec) {
    for i in 1..events.len() {
        let mut j = i;
        while j > 0 && events[j - 1].frame_offset > events[j].frame_offset {
            events.swap(j - 1, j);
            j -= 1;
        }
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use tutti_core::TimeSignature;
    use tutti_core::{Beat, BeatsPerBar, Bpm, Frame, MeterChange, NoteValue, SampleRate};
    use tutti_graph::{Env, Transport, TransportChanges, TransportHistory};
    use tutti_types::Latency;

    /// A rolling block of 64 frames at 132 BPM from beat 4, at 48 kHz: 132,
    /// not the 120 the default snapshot carries, so a live read is told
    /// apart from the "nothing" answer.
    fn env() -> Env {
        Env {
            frame: Frame(0),
            sample_rate: SampleRate(48_000.0),
            block_len: Samples(64),
            transport: Transport::new(true, Bpm(132.0), Beat(4.0), None),
            changes: TransportChanges::NONE,
        }
    }

    fn cx(e: &Env) -> Cx<'_> {
        Cx {
            env: e,
            arrival: Latency::ZERO,
            history: TransportHistory::none(),
        }
    }

    fn seven_eight() -> TimeSignature {
        TimeSignature::new(BeatsPerBar::new(7), NoteValue::EIGHTH)
    }

    fn seven_eight_meter() -> Arc<RtPublish<MeterMap>> {
        Arc::new(RtPublish::new(MeterMap::new([MeterChange::new(
            Beat(0.0),
            seven_eight(),
        )])))
    }

    /// The feature set the in-process loader builds for a plugin reporting no
    /// optional capability: `TRANSPORT` is unconditional there.
    fn loader_features() -> Features {
        Features::TRANSPORT
    }

    /// The context the node hands `vst2-host` carries the block's transport
    /// from its `Cx`: the only thing that gives `audioMasterGetTime`
    /// something to serve. With no meter installed it is 4/4.
    ///
    /// Mutation (run): return `TransportInfo::default()` from
    /// `TransportFeed::block` for every plugin → 120 BPM, stopped → fails.
    #[test]
    fn the_block_context_carries_the_transport_from_cx() {
        let e = env();
        let mut feed = TransportFeed::new();
        let snapshot = feed.block(&cx(&e), 64, loader_features());
        let ctx = block_context(48_000.0, &[], &snapshot);
        let delivered = ctx.transport.expect("a transport snapshot");
        assert_eq!(delivered.timing.tempo, 132.0);
        assert!(delivered.state.playing);
        assert_eq!(delivered.position.quarters, 4.0);
        assert_eq!(delivered.sample_rate, 48_000.0);
        assert_eq!(
            delivered.timing.signature,
            TimeSignature::new(BeatsPerBar::new(4), NoteValue::QUARTER)
        );
    }

    /// The declared bit gates delivery: a plugin without `TRANSPORT` is
    /// handed the default (stopped, 120 BPM) snapshot, whatever the `Cx`.
    ///
    /// Mutation (run): drop the `features.contains(Features::TRANSPORT)`
    /// guard → the undeclared plugin reads 132 → fails.
    #[test]
    fn a_plugin_not_declaring_transport_is_handed_the_default() {
        let e = env();
        let mut feed = TransportFeed::new();
        let got = feed.block(&cx(&e), 64, Features::empty());
        let default = TransportInfo::default();
        assert_eq!(got.timing.tempo, default.timing.tempo);
        assert_eq!(got.state.playing, default.state.playing);
        assert_eq!(got.position.quarters, default.position.quarters);
    }

    /// A meter installed through one clone reaches the clone the graph runs
    /// (a host installs through whichever clone it holds), and the snapshot
    /// carries its signature.
    ///
    /// Mutation (run): read `self.default_meter` always in
    /// `TransportFeed::block` (drop the meter source) → 4/4 → fails. A
    /// `Clone` minting a fresh meter cell fails it the same way.
    #[test]
    fn a_meter_installed_on_one_clone_reaches_another() {
        let e = env();
        let held = TransportFeed::new();
        let mut running = held.clone();
        held.meter.set_meter(seven_eight_meter());
        let got = running.block(&cx(&e), 64, loader_features());
        assert_eq!(got.timing.signature, seven_eight());
    }

    /// A fork's feed holds the meter installed when it forked, and nothing
    /// installed on the live node afterwards; its counter starts at zero.
    ///
    /// Mutation (run, together with the counter's below): `forked` sharing
    /// the live cell (`self.meter.clone()`) and keeping `self.steady` →
    /// the later clear reaches the fork (4/4), its counter starts at 64 →
    /// fails.
    #[test]
    fn a_fork_has_the_meter_installed_when_it_forked() {
        let e = env();
        let mut live = TransportFeed::new();
        live.meter.set_meter(seven_eight_meter());
        let _ = live.block(&cx(&e), 64, loader_features());
        let mut fork = live.forked();
        live.meter.clear_meter();
        let got = fork.block(&cx(&e), 64, loader_features());
        assert_eq!(got.timing.signature, seven_eight());
        assert_eq!(got.position.continuous_samples, 0);
    }

    /// The free-running counter is the frames the node rendered, block after
    /// block.
    ///
    /// Mutation (run): drop `self.steady.advance(frames)` → the second block
    /// reads 0 → fails.
    #[test]
    fn the_counter_counts_the_frames_rendered() {
        let e = env();
        let mut feed = TransportFeed::new();
        let _ = feed.block(&cx(&e), 64, loader_features());
        let second = feed.block(&cx(&e), 64, loader_features());
        assert_eq!(second.position.continuous_samples, 64);
    }

    /// An empty block has no frame to read the transport at: the plugin is
    /// handed the last snapshot again, not the stopped default.
    ///
    /// Mutation (run): return `TransportInfo::default()` for an empty block
    /// → 120 BPM, stopped → fails.
    #[test]
    fn an_empty_block_repeats_the_last_snapshot() {
        let e = env();
        let mut feed = TransportFeed::new();
        let _ = feed.block(&cx(&e), 64, loader_features());
        let empty = feed.block(&cx(&e), 0, loader_features());
        assert_eq!(empty.timing.tempo, 132.0);
        assert!(empty.state.playing);
        assert_eq!(empty.position.continuous_samples, 0);
    }
}
