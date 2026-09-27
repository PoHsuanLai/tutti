//! `InProcessVst2Client` — the graph node (`node.rs`) that drives a
//! VST2 plugin from the host audio thread.
//!
//! The instance lives behind `Arc<Mutex<tutti_vst2_host::Vst2Instance>>` shared
//! with the matching control backend. Audio thread acquires with
//! `try_lock`; on contention it falls back to silence and bumps
//! [`InProcessVst2Client::contention_count`].
//!
//! All per-block scratch — channel buffers, ref-vector storage, MIDI
//! drain — is sized in the node's `prepare`, to the graph's largest block.
//! A block is allocation-free in steady state (verified by the
//! `assert_no_alloc` regression test in `tests/`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use tutti_midi_types::ump::MidiEvent;
use tutti_vst2_host::{PluginInfo, RenderScratch, Vst2Instance, Vst2ProcessContext};

use crate::host::node::input_slot::InputSlot;
use crate::host::node::transport_source::PolledTransport;
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

/// An in-process VST2 plugin, as a graph node: MIDI in and out on event
/// ports, see `node.rs`.
pub struct InProcessVst2Client {
    pub(super) inner: Arc<Mutex<Vst2Instance>>,
    pub(super) metadata: PluginInfo,
    /// The block's MIDI in: its event input's.
    pub(super) midi: MidiEventVec,
    /// Per-block transport snapshot, gated on [`Features::TRANSPORT`]. The
    /// producer cell is shared across fundsp graph-commit clones (see
    /// [`InputSlot`]), so a `set_transport_source` on any clone reaches the one
    /// the audio thread runs.
    pub(super) transport: InputSlot<PolledTransport>,
    /// What the loader reported for this plugin, as the gate `transport` is
    /// drained against. Stored rather than passed in per block so the node's
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
            transport: InputSlot::new(Features::TRANSPORT),
            features,
            scratch,
            process_scratch,
            frames: 0,
            sample_rate,
            pending_sample_rate,
            contention_count,
        }
    }

    /// Install a transport reader so the plugin receives a live per-block
    /// [`TransportInfo`] (tempo, playhead, meter, bar, loop), which the VST2
    /// host turns into the `audioMasterGetTime` snapshot the plugin polls.
    ///
    /// Wrapped in a `PolledTransport` stamped with the current sample rate
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
        self.transport.install(Arc::new(PolledTransport::new(
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
    /// Called from [`set_rate`](Self::set_rate). The source holds its rate in
    /// a shared atomic, so this reaches the clone the audio thread runs;
    /// a no-op when no source is installed, since one installed later is stamped
    /// with `self.sample_rate` at that point.
    fn restamp_transport_rate(&self) {
        if let Some(src) = self.transport.source_ref().load().as_ref() {
            src.set_sample_rate(self.sample_rate);
        }
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
            // `InputSlot::clone` shares the producer cell rather than the
            // Option, so an install on any clone reaches the one the graph
            // runs.
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

    /// Take `sample_rate` as the rate blocks run at: restamp the transport
    /// source, and park the rate for the plugin (dispatched from the main
    /// thread, see `pending_sample_rate`). What the node's `prepare` does
    /// with its rate.
    pub(super) fn set_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.sample_rate = sample_rate;
        self.restamp_transport_rate();
        // Parked, not dispatched: `Vst2Instance::set_sample_rate` runs the
        // allocating `effMainsChanged` bracket, which must not race a block
        // the audio thread is rendering through the live plugin.
        queue_sample_rate(&self.pending_sample_rate, sample_rate);
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
    use tutti_core::meter::MeterMap;
    use tutti_core::transport::Transport;
    use tutti_core::RtPublish;

    /// The slot exactly as [`InProcessVst2Client::new`] builds it. Constructing
    /// the whole node needs a live `Vst2Instance` (a real plugin binary on
    /// disk), so the transport rail is exercised on its own — it is the piece
    /// that was missing, and it is a pure function of the declared features.
    fn slot() -> InputSlot<PolledTransport> {
        InputSlot::new(Features::TRANSPORT)
    }

    /// A rolling transport at `tempo`, plus the source the node installs for it.
    fn rolling(tempo: f64, rate: f64) -> (Transport, Arc<PolledTransport>) {
        let t = Transport::new(rate);
        t.settings.set_tempo(tempo);
        let _ = t.motion.try_send(tutti_core::MotionEvent::Play);
        t.motion.drain();
        let source = Arc::new(PolledTransport::new(
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
        transport
            .clock_links()
            .expect("the only playhead writer")
            .set_playhead(4.0);
        slot.install(source);
        let snapshot = *slot.drain(loader_features());

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
        transport
            .clock_links()
            .expect("the only playhead writer")
            .set_playhead(4.0);
        slot.install(source);

        let snapshot = slot.drain(loader_features());

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
        transport
            .clock_links()
            .expect("the only playhead writer")
            .set_playhead(4.0);
        slot.install(source);

        let snapshot = slot.drain(Features::empty());

        assert!(
            !is_live(snapshot),
            "an undeclared capability must not be fed"
        );
    }

    /// With no source installed the node still drains a usable default, so the
    /// `Vst2ProcessContext` is filled on every block rather than only once a host
    /// has wired a transport.
    #[test]
    fn an_uninstalled_slot_drains_the_default_snapshot() {
        let mut slot = slot();
        let snapshot = slot.drain(loader_features());
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
        transport
            .clock_links()
            .expect("the only playhead writer")
            .set_playhead(1.0);

        original.install(source);

        let snapshot = running.drain(loader_features());
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
        transport
            .clock_links()
            .expect("the only playhead writer")
            .set_playhead(2.0);
        slot.install(source);
        let snapshot = *slot.drain(declared);

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
