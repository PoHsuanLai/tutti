//! `InProcClient`: the graph node (`node.rs`) that drives an in-process
//! plugin of any hosting ([`Loaded`]: an [`InProcess`](tutti_plugin_types::InProcess)
//! format, or a served plugin, `tutti-plugin-ipc`'s `Ipc`) from the host audio thread.
//!
//! The audio thread renders through the Processor the node's realtime end
//! ([`tutti_rt::Rt`]) holds, and takes no lock. The plugin's control side
//! (`super::instance::Instance`: its `Lifecycle`) is behind an `Arc<Mutex<_>>`
//! that only control threads take: the handle, the node's `shape` and
//! `prepare`, a fork. A block with no Processor in hand (it was recalled for a
//! rate change, or the plugin refused its rate) renders silence, counted by
//! [`InProcClient::unlent_blocks`]; a block the Processor failed renders
//! silence too, counted by [`InProcClient::failed_blocks`].
//!
//! A block is one [`Processor::process`]: the graph's channel slices as an
//! [`AudioBufferMut`], the block's MIDI, parameter chunk and transport as a
//! [`ProcessContext`], and a [`ProcessOutput`] the client owns and clears each
//! block. A block is allocation-free in steady state (verified by the
//! `assert_no_alloc` test in `tests/`).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tutti_graph::UmpAt;
use tutti_plugin_types::{
    AudioBuffer, AudioBufferMut, Loaded, MainThread, Origin, PluginDescriptor, PluginIo,
    PluginTail, ProcessContext, ProcessOutput, Processor,
};
use tutti_rt::Rt;

use arrayvec::ArrayVec;
use parking_lot::Mutex;
use tutti_graph::{Bus, Buses, Cx, Offset, Shape};
use tutti_plugin_types::{LoadedPlugin, PluginBus};
use tutti_tempo::{Cursor, TempoMap};
use tutti_transport::TempoReader;
use tutti_types::{BusTopology, ChannelLayout, Samples};

use super::instance::Instance;
use crate::{BlockTransport, TransportSource, TransportWalk};
use crate::{ParamDelivery, PluginControls, PluginMeter, SegmentPoints};
use tutti_plugin_types::MidiIntake;
use tutti_plugin_types::{
    ChordChanges, ExpressiveContext, Features, MAX_BLOCK_CHANNELS, MIDI_STACK_CAPACITY,
    MidiEventVec, ParamId, ParameterChanges, ScaleChanges, TransportChange, TransportChanges,
    TransportInfo,
};

/// A plugin running inside the host process, as a graph node, whatever its
/// format `F`.
///
/// Created by [`load_client`](crate::load_client), or by
/// `tutti-plugin`'s `Plugin::open` for a `.vst`/`.dll`/`.so`
/// file when the `vst2` feature is enabled. MIDI arrives on the node's event
/// input and the plugin's MIDI-out leaves on an event output (for a plugin that
/// declares [`Features::MIDI_OUT`]).
///
/// There is no process isolation: a plugin crash takes the host down. The
/// audio thread renders through the Processor it was lent and takes no lock;
/// while it holds none it outputs silence (counted by
/// [`unlent_blocks`](Self::unlent_blocks)). Steady state processing does not
/// allocate.
///
/// A graph holding this node forks (`Editor::fork`, which an offline export
/// takes) **by state transfer**: a second instance of the plugin, from the same
/// library, restored from this one's saved state. See
/// [`PluginNode::fork_instance`]. The client becomes a graph node by
/// [`bind`](Self::bind)ing it to the project's tempo map.
pub struct InProcClient<F: Loaded> {
    /// The plugin's control side. Control threads only: the audio thread
    /// renders through `rt`.
    pub(super) inner: Arc<Mutex<Instance<F>>>,
    /// The realtime end the plugin's Processor is lent to.
    pub(super) rt: Rt<F::Processor>,
    pub(super) descriptor: PluginDescriptor,
    /// Where the plugin came from, for an in-process one: what a fork loads
    /// again. `None` for a hosting that reopens another way.
    pub(super) origin: Option<Origin>,
    /// What the Processor adds to the plugin's own latency
    /// ([`Processor::latency`]), as the last `prepare` read it: the served
    /// pipeline's chunk.
    pub(super) pipeline: Samples,
    /// A served plugin's host-side controls (`PluginControls`): its latency and
    /// tail are re-reported by the bridge's thread into cells the controls
    /// share, so `shape` reads the figures from them, `retune` writes the
    /// pipeline's chunk and the rate into them, and the meter, the delivery
    /// counters and the chunk counters are the node's own. `None` for an
    /// in-process plugin, whose figures are the control side's.
    pub(super) controls: Option<PluginControls>,
    /// The pin counts, latency and tail the plugin declared when the node was
    /// built: what `shape` and `process` size by while the instance lock is
    /// held elsewhere. Moves are reported through the handle
    /// (`PluginInvalidation`), not here.
    pub(super) declared: Declared,
    /// The block's MIDI in: its event input's.
    pub(super) midi: MidiEventVec,
    /// Takes the event input's MIDI into `midi`, carrying across blocks
    /// which SysEx lost a packet to its cap.
    pub(super) intake: MidiIntake,
    /// What the plugin emits in a block: owned here and cleared at the head of
    /// each block, so its storage is the one allocation, made at construction.
    pub(super) out: ProcessOutput,
    /// The block's transport, read from the graph's [`Cx`] (live and in a
    /// fork alike), with the project meter installed through
    /// [`NodeControls::set_meter`](crate::NodeControls::set_meter).
    pub(super) transport: TransportFeed,
    /// What the loader reported for this plugin, as the gate `transport` is
    /// read against. Stored rather than passed in per block so the node's
    /// declared capability and its delivered behaviour read from one value.
    pub(super) features: Features,
    pub(super) sample_rate: f64,
    /// A rate the graph handed this node that the plugin has not been told
    /// about yet, or [`NO_PENDING_RATE`] when there is none.
    ///
    /// Telling a plugin its rate is a deactivation and an activation, which
    /// needs the main thread. A `prepare` on it runs the change itself; one off
    /// it parks the rate here ([`queue_sample_rate`]) for the main thread's
    /// pump ([`drain_sample_rate`], the editor idle pump), the same deferral
    /// the out-of-process client gets from its command queue.
    ///
    /// Shared with the control backend, which drains it.
    pending_sample_rate: Arc<AtomicU64>,
    /// Blocks rendered silent because the realtime end held no Processor.
    /// Shared with the loader's handle side.
    pub(super) unlent: Arc<AtomicU64>,
    /// Blocks rendered silent because the Processor failed them
    /// ([`Processor::process`] returned an error).
    pub(super) failed: Arc<AtomicU64>,
    /// Who tells the plugin the rate the node is prepared at.
    pub(super) rate_delivery: RateDelivery,
    /// A fork's rate could not be told to the plugin (its `prepare` ran off the
    /// main thread), so it is at a rate other than the node's: the node renders
    /// silence until a `prepare` on the main thread tells it.
    pub(super) rate_refused: bool,
    /// The parameter events on the event input, as changes only: a slot per
    /// parameter the plugin declared at load and no spares (`closed`: the
    /// node addresses only its declared parameters), the value last
    /// delivered to the instance in each (`host::node`'s `param_delivery`,
    /// the out-of-process node's own). The block is its chunk.
    ///
    /// The node is not `Clone`: a clone would drive the same instance, so
    /// two of them in one graph write it in an unordered order (the
    /// parallel executor), each with its own record of what it sent, and a
    /// `ForkByClone` of it would render an export through the live
    /// instance. A fork is a second instance (`fork.rs`).
    pub(super) params: ParamDelivery,
    /// The curve segments in flight on the event input, sent as points until
    /// each lands. Slots sized at construction, one per parameter slot.
    pub(super) segments: SegmentPoints,
    /// Frames into the Processor's chunk, from `prepare` and every `reset`: where
    /// a block of a chunked Processor ([`Processor::chunk`]) is cut. Always
    /// zero for one that is not.
    pub(super) phase: Samples,
    /// The walk of the transport across the chunk being filled, between pieces.
    pub(super) walk: TransportWalk,
    /// The transport at the chunk's first frame (final at the piece that
    /// completes it), and the chunk's changes so far, on chunk frames: what
    /// every piece of a chunked Processor is handed.
    pub(super) chunk_info: TransportInfo,
    pub(super) changes: TransportChanges,
    /// MIDI the chunked Processor holds for the chunk in flight (taken by the
    /// pieces so far), which a reset or a re-prepare drops.
    pub(super) chunk_midi: u64,
    /// Dropped MIDI the next block reports to the executor (`Io::count_lost`):
    /// a reset has no `Io`.
    pub(super) owed_lost: u64,
    /// The chords and scales in the piece being rendered, for a plugin that
    /// takes sequencer context.
    pub(super) chords: ChordChanges,
    pub(super) scales: ScaleChanges,
    /// The MIDI-out of the pieces rendered so far in this block, on the
    /// block's frames.
    pub(super) out_events: MidiEventVec,
}

/// Omits `inner`: formatting must not wait on the control side's lock.
impl<F: Loaded> std::fmt::Debug for InProcClient<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcClient")
            .field("descriptor", &self.descriptor)
            .field("sample_rate", &self.sample_rate)
            .finish_non_exhaustive()
    }
}

/// What the plugin declared when the node was built.
#[derive(Clone, Copy, Debug)]
pub(super) struct Declared {
    /// The plugin's channels as the node's ports: its buses as the node's
    /// buses, in bus order, and the channels past them as ports.
    pub(super) ports: Shape,
    pub(super) latency: Samples,
    pub(super) tail: PluginTail,
}

impl Declared {
    /// What the plugin reported at load.
    pub(super) fn of(loaded: &LoadedPlugin) -> Self {
        Self {
            ports: Shape::buses(buses_of(&loaded.inputs), buses_of(&loaded.outputs)).with_ports(
                ChannelLayout::clamped(loaded.total_inputs()),
                ChannelLayout::clamped(loaded.total_outputs()),
            ),
            latency: loaded.latency_samples,
            tail: loaded.tail,
        }
    }
}

/// The node's buses for a plugin's, in bus order, each with the role the
/// format gave it.
///
/// A bus without a [`placement`](PluginBus::placement) (unknown, or one that
/// does not have its width's channels) is `Unassigned` at that width: a count is
/// not a placement. The list stops at the first bus past
/// [`MAX_BUSES`](tutti_graph::MAX_BUSES) or wider than a bus holds; the
/// plugin's remaining channels are ports the node still has.
fn buses_of(plugin: &[PluginBus]) -> Buses {
    let mut buses = Buses::NONE;
    for bus in plugin {
        let placed = bus.placement().and_then(|t| BusTopology::try_from(t).ok());
        let Some(topology) = placed.or_else(|| BusTopology::unassigned(bus.channels)) else {
            break;
        };
        match buses.with(Bus::new(topology, bus.role)) {
            Some(more) => buses = more,
            None => break,
        }
    }
    buses
}

/// Who tells the plugin the rate the node is prepared at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RateDelivery {
    /// Off the main thread, parked in `pending_sample_rate` for the main
    /// thread's editor pump (`Backend::editor_idle`) to dispatch: the live
    /// instance.
    Parked,
    /// Off the main thread, refused (silence until a `prepare` on it): a fork.
    /// Nothing pumps a fork's editor (it has no handle), so a parked rate
    /// would never reach it.
    InPrepare,
}

/// The transport a block hands the plugin: what `audioMasterGetTime`
/// answers while the plugin processes it.
///
/// One path for the live node and its fork: the transport the block's input
/// was rendered at ([`Cx::transport_at`] at its first frame, the node's
/// compiled arrival latency earlier), mapped the way the out-of-process
/// plugin node maps it (`from_cx`), with the project
/// meter for the signature and bar. For an offline fork the `Cx` is the
/// render's. The snapshot is built in `process` before the plugin is
/// entered: the Processor publishes it where the plugin reads it (VST2:
/// the cell `audioMasterGetTime` reads), then calls the plugin.
///
/// A clone shares the meter slot and reads it through a reader of its own
/// (`Reader::clone` registers one on the same cell), and carries the counter
/// and the last snapshot as they stand. Only a test clones a feed.
#[derive(Clone, Debug)]
pub(super) struct TransportFeed {
    /// The node's meter slot, shared with its controls
    /// ([`InProcClient::controls`]), so a meter installed through them
    /// reaches the node the audio thread runs; a fork has a slot of its own
    /// ([`forked`](Self::forked)). Control side only: the block reads
    /// through `source`.
    meter: PluginMeter,
    /// The slot's reader and the clocks (`TransportSource`), the out-of-process
    /// node's own.
    source: TransportSource,
    /// The last snapshot handed out: what an empty block (no frame to read
    /// the transport at) repeats.
    last: TransportInfo,
}

impl TransportFeed {
    /// A handle on the node's meter cell: what its controls install through.
    pub(super) fn meter(&self) -> PluginMeter {
        self.meter.clone()
    }

    /// No meter installed, nothing rendered.
    pub(super) fn new() -> Self {
        Self::over(PluginMeter::default())
    }

    /// A feed reading `meter`'s cell through a reader registered here,
    /// nothing rendered, on the host clock. Control thread:
    /// registering may allocate.
    fn over(meter: PluginMeter) -> Self {
        Self {
            source: TransportSource::new(meter.reader()),
            meter,
            last: TransportInfo::default(),
        }
    }

    /// A fork's feed: the meter installed now, in a slot of its own (a meter
    /// installed on the live node afterwards does not reach the fork, as for
    /// the out-of-process node's fork), a counter from zero, and, for an
    /// `offline` render, no host clock.
    pub(super) fn forked(&self, offline: bool) -> Self {
        let mut feed = Self::over(PluginMeter::holding(self.meter.installed()));
        if offline {
            feed.source.go_offline();
        }
        feed
    }

    /// No host clock from here on: the render runs at its own speed, so
    /// `nanoSeconds` must not be claimed ([`TransportSource::go_offline`]).
    pub(super) fn go_offline(&mut self) {
        self.source.go_offline();
    }

    /// What one block reads of the transport, for a node that reads it at
    /// several frames ([`TransportSource::block`]); count the block's frames
    /// after it with [`advance`](Self::advance).
    pub(super) fn lend(&mut self, features: Features) -> BlockTransport<'_> {
        self.source.block(features)
    }

    /// Count `frames` rendered ([`TransportSource::advance`]).
    pub(super) fn advance(&mut self, frames: usize) {
        self.source.advance(frames);
    }

    /// The snapshot for a block of `frames`, then count them. A plugin that
    /// did not declare [`Features::TRANSPORT`] is handed the default
    /// (stopped) snapshot; an empty block, which has no frame to read the
    /// transport at, repeats the last one. Audio thread: one wait-free
    /// borrow (the node's meter cell), no allocation and no free.
    pub(super) fn block(
        &mut self,
        cx: &Cx<'_>,
        frames: usize,
        features: Features,
        tempo: &TempoMap,
        cursor: &mut Cursor,
    ) -> TransportInfo {
        if !features.contains(Features::TRANSPORT) {
            self.source.advance(frames);
            return TransportInfo::default();
        }
        let Some(at) = Offset::new(0, Samples(frames)) else {
            return self.last;
        };
        self.last = self
            .source
            .block(features)
            .at(cx, at, frames, tempo, cursor);
        self.source.advance(frames);
        self.last
    }
}

/// An in-process client bound to the project's tempo map
/// ([`InProcClient::bind`]): the graph node. It owns the reader, lends
/// the map once per block and keeps a cursor into it. Only this type is a
/// [`Node`](tutti_graph::Node) and an [`IntoNode`](tutti_graph::IntoNode);
/// the loaded client is neither. It derefs to the client for its control
/// surface (`controls`, `set_meter`, ...).
pub struct PluginNode<F: Loaded> {
    pub(super) client: InProcClient<F>,
    pub(super) tempo: TempoReader,
    pub(super) cursor: Cursor,
}

impl<F: Loaded> std::fmt::Debug for PluginNode<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginNode").finish_non_exhaustive()
    }
}

impl<F: Loaded> std::ops::Deref for PluginNode<F> {
    type Target = InProcClient<F>;
    fn deref(&self) -> &InProcClient<F> {
        &self.client
    }
}

impl<F: Loaded> std::ops::DerefMut for PluginNode<F> {
    fn deref_mut(&mut self) -> &mut InProcClient<F> {
        &mut self.client
    }
}

/// [`InProcClient::pending_sample_rate`] sentinel: nothing is waiting.
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
/// Main thread only: `Instance::set_sample_rate` deactivates and activates the
/// plugin, which allocates. This is the drain half of the deferral
/// [`queue_sample_rate`] opens. The Processor is on the audio thread, so the
/// change starts with a recall and finishes at a later pump, once the audio
/// thread has handed it back.
pub(super) fn drain_sample_rate<F: Loaded>(
    main: &MainThread,
    cell: &AtomicU64,
    inner: &Mutex<Instance<F>>,
) -> bool {
    let bits = cell.swap(NO_PENDING_RATE, Ordering::Relaxed);
    if bits == NO_PENDING_RATE {
        return false;
    }
    // A refusal is kept in the instance and reported by the control side's next
    // pump (`take_rate_report`), so the result is not read here.
    let _ = inner
        .lock()
        .set_sample_rate(main, f64::from_bits(bits), None);
    true
}

impl<F: Loaded> InProcClient<F> {
    pub(super) fn new(
        inner: Arc<Mutex<Instance<F>>>,
        rt: Rt<F::Processor>,
        descriptor: PluginDescriptor,
        origin: Option<Origin>,
        declared: Declared,
        features: Features,
        sample_rate: f64,
        pending_sample_rate: Arc<AtomicU64>,
        unlent: Arc<AtomicU64>,
        parameters: impl IntoIterator<Item = ParamId>,
        spares: usize,
        controls: Option<PluginControls>,
    ) -> Self {
        // A slot per declared parameter: every parameter can move in one
        // block. Its point room is sized in `prepare`. `spares` is how many
        // addresses the plugin did not list the delivery may learn (none:
        // the node addresses only its declared parameters).
        let mut rt = rt;
        let pipeline = rt
            .poll(Processor::stop)
            .map_or(Samples(0), |processor| processor.latency());
        if let Some(controls) = &controls {
            controls.set_pipeline(pipeline);
        }
        let counters = controls
            .as_ref()
            .map_or_else(Arc::default, PluginControls::delivery_counters);
        let params = ParamDelivery::with_spares(parameters, counters, spares);
        let transport = controls
            .as_ref()
            .map_or_else(TransportFeed::new, |c| TransportFeed::over(c.meter()));
        let mut segments = SegmentPoints::default();
        segments.prepare(params.room());
        Self {
            inner,
            rt,
            descriptor,
            origin,
            pipeline,
            declared,
            midi: MidiEventVec::new(),
            intake: MidiIntake::default(),
            out: ProcessOutput::default(),
            transport,
            controls,
            features,
            sample_rate,
            pending_sample_rate,
            unlent,
            failed: Arc::default(),
            rate_delivery: RateDelivery::Parked,
            rate_refused: false,
            params,
            segments,
            phase: Samples(0),
            walk: TransportWalk::default(),
            chunk_info: TransportInfo::default(),
            changes: TransportChanges::default(),
            chunk_midi: 0,
            owed_lost: 0,
            chords: ChordChanges::default(),
            scales: ScaleChanges::default(),
            out_events: MidiEventVec::new(),
        }
    }

    /// Binds the client to the reader of the project's tempo map it tells the
    /// plugin the tempo from, lent once per
    /// block: the step from a loaded client to a graph node, as
    /// `PluginClient::bind` (`tutti-plugin`'s) is for
    /// the out-of-process one. `Session` hands every plugin it inserts a
    /// reader of its own; a host with no session builds one over a fixed map
    /// ([`TempoReader::fixed`]). A fork clones the reader. Control thread.
    #[must_use]
    pub fn bind(self, tempo: TempoReader) -> PluginNode<F> {
        PluginNode {
            client: self,
            tempo,
            cursor: Cursor::default(),
        }
    }

    /// Sets the render mode the plugin runs in ([`Control::set_render_mode`](tutti_plugin_types::Control::set_render_mode),
    /// and a reconfiguration through the lifecycle for a format that cannot
    /// take it live: the next activation carries the mode).
    ///
    /// `false` off the main thread, where the plugin cannot be told, and when
    /// the plugin refuses the reconfiguration. Takes the instance lock, so
    /// call it from a control thread, not the audio thread.
    pub fn set_render_mode(&self, mode: tutti_plugin_types::RenderMode) -> bool {
        match MainThread::current() {
            Some(main) => self.inner.lock().set_render_mode(&main, mode),
            None => false,
        }
    }

    /// Seeds the parameter delivery with what a live node had delivered
    /// ([`PluginControls::delivered`]): a value the live node sent is the
    /// fork's too, so a host value held still is not sent to a fork over the
    /// state it was loaded with.
    pub fn seed_delivery(&mut self, delivered: &[(ParamId, f64)]) {
        self.params.seed(delivered);
    }

    /// Drops the host clock the transport tells the plugin
    /// (`TransportInfo::system_time_ns`): for an offline render, which has none.
    /// A fork of the in-process node does this itself; a served plugin's fork
    /// (`tutti-plugin`) does it through this.
    pub fn go_offline(&mut self) {
        self.transport.go_offline();
    }

    /// Returns how many blocks the node rendered silent because its realtime
    /// end held no Processor (recalled for a rate change, or a plugin that
    /// refused its rate), since construction.
    ///
    /// Intended for diagnostics.
    pub fn unlent_blocks(&self) -> u64 {
        self.unlent.load(Ordering::Relaxed)
    }

    /// Returns how many blocks the node rendered silent because the plugin
    /// failed them ([`Processor::process`] returned an error), since
    /// construction. Such a block loses its MIDI, counted with the rest.
    ///
    /// Intended for diagnostics.
    pub fn failed_blocks(&self) -> u64 {
        self.failed.load(Ordering::Relaxed)
    }
}

impl<F: Loaded> InProcClient<F> {
    /// The chunk in flight is dropped (a reset, a re-prepare): the walk of its
    /// transport ends, and the MIDI the Processor held for it is lost, counted
    /// now in the delivery's dropped MIDI and in the executor's at the next
    /// block.
    pub(super) fn drop_chunk(&mut self) {
        self.walk.clear();
        let lost = std::mem::take(&mut self.chunk_midi);
        self.params.count_midi_dropped(lost);
        self.owed_lost += lost;
    }

    /// Offers `p`'s rate, block ceiling and quantum to the Processor
    /// ([`Processor::retune`]) and reads the latency it adds afterwards.
    /// `true` when it took them live: the plugin is told no restart, needs no
    /// main thread, and [`set_rate`](Self::set_rate) is not run. `false` when
    /// it has none in hand or does not follow the graph (an in-process one).
    pub(super) fn retune(&mut self, p: &tutti_graph::Prepare) -> bool {
        let rate = p.sample_rate();
        let max_block = Samples(p.max_block().get());
        let Some(processor) = self.rt.poll(Processor::stop) else {
            return false;
        };
        if !processor.retune(rate, max_block, p.quantum()) {
            return false;
        }
        self.pipeline = processor.latency();
        if let Some(controls) = &self.controls {
            // What a host holding the controls reads as the declared latency
            // and stamps a source with.
            controls.set_pipeline(self.pipeline);
            controls.restamp(rate);
        }
        self.sample_rate = rate.get();
        self.rate_refused = false;
        self.inner.lock().retuned(rate);
        true
    }

    /// Take `sample_rate` as the rate blocks run at, and hand it to the
    /// plugin as [`RateDelivery`] says: parked for the main thread (see
    /// `pending_sample_rate`), or, on a fork, dispatched here. What the
    /// node's `prepare` does with its rate. (The transport snapshot's rate is
    /// the block's own, from its `Env`.)
    pub(super) fn set_rate(&mut self, sample_rate: tutti_types::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        let changed = sample_rate != self.sample_rate;
        self.sample_rate = sample_rate;
        // Only on a change: a redundant bracket makes a plugin reallocate its
        // buffers.
        if !changed && !self.rate_refused {
            return;
        }
        // `prepare` has no token to be handed and cannot fail, so it asks for
        // the proof. On the main thread the change runs here, to the end: the
        // node is out of any executor (`Node::prepare`'s contract), so its
        // realtime end hands the Processor back and takes the new one in this
        // call.
        if let Some(main) = MainThread::current() {
            let outcome = self
                .inner
                .lock()
                .set_sample_rate(&main, sample_rate, Some(&mut self.rt));
            // A refusal renders silence (no Processor is lent) and is reported
            // through the handle (`take_rate_report`).
            self.rate_refused = outcome.is_err();
            if let Err(reason) = outcome {
                tracing::error!("a plugin refused its rate: {reason}");
            }
            return;
        }
        match self.rate_delivery {
            // Off the main thread the bracket cannot run: parked for the main
            // thread's pump.
            RateDelivery::Parked => queue_sample_rate(&self.pending_sample_rate, sample_rate),
            // A fork has no pump: the refusal is recorded (`rate_refused`), the
            // node renders silence rather than audio at the wrong rate, and the
            // next `prepare` on the main thread clears it.
            RateDelivery::InPrepare => {
                self.rate_refused = true;
                tracing::error!(
                    "a forked plugin was prepared off the main thread; \
                     it renders silence until it is prepared on it"
                );
            }
        }
    }
}

/// The per-block context handed to the plugin, carrying this block's MIDI,
/// parameter chunk and transport snapshot.
///
/// `transport` is always attached. The decision of *what* to attach happens one
/// level up, in [`TransportFeed::block`], which yields a default snapshot for a
/// plugin that did not declare [`Features::TRANSPORT`] — so there is no second
/// format check here. The parameter chunk is always attached too: it carries
/// the changes only, and a Processor that takes parameters through a coarse
/// write (VST2) reads it as well as one that takes curves.
///
/// Named rather than inlined so the shape of the context the node builds is
/// observable without a plugin binary on disk: whether `ctx.transport` is
/// populated at all is precisely what decides if the plugin has a transport
/// to read.
pub(super) fn block_context<'a>(
    midi_events: &'a [UmpAt],
    params: &'a ParameterChanges,
    transport: &'a TransportInfo,
) -> ProcessContext<'a> {
    ProcessContext::new()
        .midi(midi_events)
        .params(params)
        .transport(transport)
}

/// What a block did to the plugin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Rendered {
    /// The plugin ran the block; its MIDI-out is in the client's `out`.
    Ran,
    /// No Processor in hand: the block was not rendered.
    Unlent,
    /// The Processor failed the block.
    Failed,
}

/// What a block, or the piece of one a chunked Processor is handed, carries
/// beside its MIDI and parameter changes.
pub(super) struct Inputs<'a> {
    /// The transport at the first frame.
    pub(super) transport: &'a TransportInfo,
    /// The transport changes inside, each on a frame from the first.
    pub(super) changes: &'a [TransportChange],
    /// Sequencer context, for a plugin that takes it.
    pub(super) expressive: Option<ExpressiveContext<'a>>,
}

/// Render one block through the plugin, from `inputs` into `outputs`: the
/// graph's own channel buffers, each `size` frames, through the Processor the
/// node took from its realtime end (`None`: it holds none). A free function so
/// it can take disjoint borrows of the fields on the caller side without a
/// self-borrow conflict. A chunked Processor's node calls it once per piece of
/// a block.
///
/// On anything but [`Rendered::Ran`] `outputs` is untouched: the caller writes
/// the silence.
pub(super) fn render_with<P: Processor>(
    processor: Option<&mut P>,
    counters: (&AtomicU64, &AtomicU64),
    params: &ParamDelivery,
    midi: &mut MidiEventVec,
    out: &mut ProcessOutput,
    with: Inputs<'_>,
    inputs: &[&[f32]],
    outputs: &mut [&mut [f32]],
    size: usize,
    sample_rate: f64,
) -> Rendered {
    let (unlent, failed) = counters;
    // Taken whether or not the plugin runs: a block with no Processor drops
    // its MIDI rather than play it a block late. Sorted, stably: a queue may
    // hold it out of order.
    let mut midi_events = std::mem::take(midi);
    tutti_plugin_types::insertion_sort_by_key(&mut midi_events, |e| e.offset);
    // The block's MIDI as the graph's `(Offset, Ump)`: every offset came from
    // an `Offset` of this block (the node's intake), so none is clamped here;
    // the `min` only keeps a stray one inside the block.
    let last = size.saturating_sub(1);
    let block = Samples(size);
    let timed: ArrayVec<UmpAt, MIDI_STACK_CAPACITY> = midi_events
        .iter()
        .filter_map(|e| Some((Offset::new((e.offset as usize).min(last), block)?, e.ump)))
        .collect();
    let Some(processor) = processor else {
        unlent.fetch_add(1, Ordering::Relaxed);
        return Rendered::Unlent;
    };
    out.clear();
    let mut ctx =
        block_context(&timed, params.chunk(), with.transport).transport_changes(with.changes);
    if let Some(expressive) = with.expressive {
        ctx = ctx.expressive(expressive);
    }
    // `PluginIo` borrows the tables and the data for one lifetime, and a
    // `&mut [&mut _]` is invariant in the data's: the same slices again, in a
    // table whose borrow is this call's own. An array, not an `ArrayVec`: a
    // type with a `Drop` would need its data to outlive the borrow of the
    // table.
    let width = outputs.len().min(MAX_BLOCK_CHANNELS);
    let mut rest = outputs.iter_mut();
    let mut table: [&mut [f32]; MAX_BLOCK_CHANNELS] =
        std::array::from_fn(|_| rest.next().map_or(&mut [][..], |o| &mut **o));
    let mut io = PluginIo {
        buffer: AudioBufferMut::F32(AudioBuffer {
            inputs,
            outputs: &mut table[..width],
            num_samples: size,
            sample_rate,
        }),
        ctx: &ctx,
        out,
    };
    match processor.process(&mut io) {
        Ok(()) => Rendered::Ran,
        Err(_) => {
            failed.fetch_add(1, Ordering::Relaxed);
            Rendered::Failed
        }
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use tutti_graph::{Env, Transport, TransportChanges, TransportHistory};
    use tutti_transport::ProjectMeter;
    use tutti_types::Latency;
    use tutti_types::TimeSignature;
    use tutti_types::{
        Beat, BeatsPerBar, Bpm, Frame, MeterChange, MeterMap, NoteValue, SampleRate,
    };

    /// A rolling block of 64 frames at 132 BPM from beat 4, at 48 kHz: 132,
    /// not the 120 the default snapshot carries, so a live read is told
    /// apart from the "nothing" answer.
    fn env() -> Env {
        Env {
            frame: Frame(0),
            sample_rate: SampleRate(48_000.0),
            block_len: Samples(64),
            transport: Transport::new(true, Bpm::of(132.0), Beat(4.0), None),
            changes: TransportChanges::NONE,
            play: tutti_graph::PlayClock::ZERO,
        }
    }

    /// The project's tempo map for these blocks: the constant 132 BPM
    /// `env()`'s transport carries, so a block reports the tempo the test's
    /// transport has.
    fn map() -> TempoMap {
        TempoMap::constant(Bpm::of(132.0)).expect("a tempo")
    }

    fn feed() -> TransportFeed {
        TransportFeed::new()
    }

    fn cx(e: &Env) -> Cx<'_> {
        Cx {
            env: e,
            arrival: Latency::ZERO,
            history: TransportHistory::none(),
        }
    }

    fn seven_eight() -> TimeSignature {
        TimeSignature::new(BeatsPerBar::of(7), NoteValue::EIGHTH)
    }

    fn seven_eight_meter() -> ProjectMeter {
        ProjectMeter::new(MeterMap::new([MeterChange::new(Beat(0.0), seven_eight())]))
    }

    /// The feature set the in-process loader builds for a plugin reporting no
    /// optional capability: `TRANSPORT` is unconditional there.
    fn loader_features() -> Features {
        Features::TRANSPORT
    }

    /// The context the node hands the plugin carries the block's transport
    /// from its `Cx`: the only thing that gives `audioMasterGetTime`
    /// something to read. With no meter installed it is 4/4.
    ///
    /// Mutation (run): return `TransportInfo::default()` from
    /// `TransportFeed::block` for every plugin → 120 BPM, stopped → fails.
    #[test]
    fn the_block_context_carries_the_transport_from_cx() {
        let e = env();
        let mut feed = feed();
        let snapshot = feed.block(
            &cx(&e),
            64,
            loader_features(),
            &map(),
            &mut Cursor::default(),
        );
        let params = ParameterChanges::default();
        let ctx = block_context(&[], &params, &snapshot);
        let delivered = ctx.transport.expect("a transport snapshot");
        assert_eq!(delivered.timing.tempo, 132.0);
        assert!(delivered.state.playing);
        assert_eq!(delivered.position.quarters, 4.0);
        assert_eq!(delivered.sample_rate, 48_000.0);
        assert_eq!(
            delivered.timing.signature,
            TimeSignature::new(BeatsPerBar::of(4), NoteValue::QUARTER)
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
        let mut feed = feed();
        let got = feed.block(
            &cx(&e),
            64,
            Features::empty(),
            &map(),
            &mut Cursor::default(),
        );
        let default = TransportInfo::default();
        assert_eq!(got.timing.tempo, default.timing.tempo);
        assert_eq!(got.state.playing, default.state.playing);
        assert_eq!(got.position.quarters, default.position.quarters);
    }

    /// A meter installed through one handle on the feed's cell reaches the
    /// feed the node runs (a host installs through the node's controls), and
    /// the snapshot carries its signature.
    ///
    /// Mutation (run): read `self.default_meter` always in
    /// `TransportFeed::block` (drop the meter source) → 4/4 → fails. A
    /// `Clone` minting a fresh meter cell fails it the same way.
    #[test]
    fn a_meter_installed_on_one_clone_reaches_another() {
        let e = env();
        let held = feed();
        let mut running = held.clone();
        held.meter.set_meter(seven_eight_meter());
        let got = running.block(
            &cx(&e),
            64,
            loader_features(),
            &map(),
            &mut Cursor::default(),
        );
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
        let mut live = feed();
        live.meter.set_meter(seven_eight_meter());
        let _ = live.block(
            &cx(&e),
            64,
            loader_features(),
            &map(),
            &mut Cursor::default(),
        );
        let mut fork = live.forked(false);
        live.meter.clear_meter();
        let got = fork.block(
            &cx(&e),
            64,
            loader_features(),
            &map(),
            &mut Cursor::default(),
        );
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
        let mut feed = feed();
        let _ = feed.block(
            &cx(&e),
            64,
            loader_features(),
            &map(),
            &mut Cursor::default(),
        );
        let second = feed.block(
            &cx(&e),
            64,
            loader_features(),
            &map(),
            &mut Cursor::default(),
        );
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
        let mut feed = feed();
        let _ = feed.block(
            &cx(&e),
            64,
            loader_features(),
            &map(),
            &mut Cursor::default(),
        );
        let empty = feed.block(
            &cx(&e),
            0,
            loader_features(),
            &map(),
            &mut Cursor::default(),
        );
        assert_eq!(empty.timing.tempo, 132.0);
        assert!(empty.state.playing);
        assert_eq!(empty.position.continuous_samples, 0);
    }

    /// **A live feed tells the plugin the host clock; an offline fork's
    /// does not.** No clock relates to a render that runs at its own speed,
    /// so `nanoSeconds` must not be claimed there. A live fork keeps it.
    ///
    /// Mutation (run): `forked` ignoring `offline` → the offline fork reports
    /// a host time → fails. Mutation (run): `over` with `host_clock: None` →
    /// the live feed reports none → fails.
    #[test]
    fn only_a_live_feed_carries_the_host_clock() {
        let e = env();
        let mut live = feed();
        assert!(
            live.block(
                &cx(&e),
                64,
                loader_features(),
                &map(),
                &mut Cursor::default()
            )
            .system_time_ns
            .is_some()
        );
        let mut live_fork = live.forked(false);
        assert!(
            live_fork
                .block(
                    &cx(&e),
                    64,
                    loader_features(),
                    &map(),
                    &mut Cursor::default()
                )
                .system_time_ns
                .is_some()
        );
        let mut offline = live.forked(true);
        assert_eq!(
            offline
                .block(
                    &cx(&e),
                    64,
                    loader_features(),
                    &map(),
                    &mut Cursor::default()
                )
                .system_time_ns,
            None
        );
    }

    /// The tempo a block reports is the project map's at the playhead, not
    /// the transport's own (`Env`'s 132).
    ///
    /// Mutation (run): drop `Snapshot::follow` from `from_cx` -> the block
    /// reads 132 -> fails.
    #[test]
    fn a_block_reports_the_maps_tempo() {
        let e = env();
        let mut live = feed();
        let map = TempoMap::constant(Bpm::of(100.0)).expect("a tempo");
        let got = live.block(&cx(&e), 64, loader_features(), &map, &mut Cursor::default());
        assert_eq!(got.timing.tempo, 100.0);
    }
}

/// `render_with`, over a Processor of the test's own: what a block does to the
/// counters and to the plugin's output, for any format.
#[cfg(test)]
mod render_tests {
    use super::*;
    use tutti_plugin_types::{PluginError, PluginResult, WireMidi};
    use tutti_ump::Ump;

    /// A Processor that fails every block it is told to, and emits one MIDI
    /// event in each block it is told to.
    struct Scripted {
        fail: bool,
        emit: bool,
    }

    impl Processor for Scripted {
        fn process(&mut self, io: &mut PluginIo<'_>) -> PluginResult<()> {
            if self.fail {
                return Err(PluginError::Block("scripted failure"));
            }
            if self.emit {
                io.out.emit_midi([WireMidi {
                    offset: 3,
                    ump: Ump([0x2090_3C40, 0, 0, 0]),
                }]);
                self.emit = false;
            }
            Ok(())
        }

        fn stop(&mut self) {}

        fn reset(&mut self) {}
    }

    /// One block of 8 frames through `render_with`, the rest of its arguments at
    /// their quiet values.
    fn block(
        rt: &mut Rt<Scripted>,
        counters: &(AtomicU64, AtomicU64),
        out: &mut ProcessOutput,
    ) -> Rendered {
        let params = ParamDelivery::closed([], Arc::default());
        let mut midi = MidiEventVec::new();
        let mut channel = [0.0f32; 8];
        render_with(
            rt.poll(Processor::stop),
            (&counters.0, &counters.1),
            &params,
            &mut midi,
            out,
            Inputs {
                transport: &TransportInfo::default(),
                changes: &[],
                expressive: None,
            },
            &[],
            &mut [&mut channel[..]],
            8,
            48_000.0,
        )
    }

    /// **A block the Processor fails is counted as failed, and a block with no
    /// Processor as unlent, each apart from the other**; the caller renders
    /// silence for both. A failure does not take the Processor away: the next
    /// block runs.
    ///
    /// Mutation (run): the `failed.fetch_add` in `render_with` removed -> the
    /// failed count stays 0 -> fails. Mutation (run): `Err(_)` answering
    /// `Rendered::Unlent` -> the first assertion fails.
    #[test]
    fn a_failed_block_is_counted_apart_from_an_unlent_one() {
        let counters = (AtomicU64::new(0), AtomicU64::new(0));
        let mut out = ProcessOutput::default();
        let (mut hand, mut rt) = tutti_rt::handoff();
        assert_eq!(block(&mut rt, &counters, &mut out), Rendered::Unlent);
        assert_eq!(counters.0.load(Ordering::Relaxed), 1, "unlent");
        assert_eq!(counters.1.load(Ordering::Relaxed), 0, "failed");

        assert!(
            hand.lend(Scripted {
                fail: true,
                emit: false
            })
            .is_ok()
        );
        assert_eq!(block(&mut rt, &counters, &mut out), Rendered::Failed);
        assert_eq!(counters.0.load(Ordering::Relaxed), 1, "unlent");
        assert_eq!(counters.1.load(Ordering::Relaxed), 1, "failed");

        rt.held().expect("still in hand").fail = false;
        assert_eq!(block(&mut rt, &counters, &mut out), Rendered::Ran);
        assert_eq!(counters.1.load(Ordering::Relaxed), 1, "failed, once");
    }

    /// **The plugin's MIDI out is what it emitted in this block**: the block
    /// after an emitting one reads none of it, because `out` is cleared at the
    /// head of every block.
    ///
    /// Mutation (run): the `out.clear()` in `render` removed -> the second
    /// block still holds the first's event -> fails.
    #[test]
    fn a_blocks_midi_out_is_what_the_plugin_emitted_in_it() {
        let counters = (AtomicU64::new(0), AtomicU64::new(0));
        let mut out = ProcessOutput::default();
        let (mut hand, mut rt) = tutti_rt::handoff();
        assert!(
            hand.lend(Scripted {
                fail: false,
                emit: true
            })
            .is_ok()
        );
        assert_eq!(block(&mut rt, &counters, &mut out), Rendered::Ran);
        assert_eq!(out.midi_events.len(), 1);
        assert_eq!(out.midi_events[0].offset, 3);
        assert_eq!(block(&mut rt, &counters, &mut out), Rendered::Ran);
        assert!(
            out.midi_events.is_empty(),
            "last block's MIDI is not this one's"
        );
    }
}

#[cfg(test)]
mod declared_buses {
    use super::*;
    use tutti_graph::BusRole;
    use tutti_plugin_types::{ChannelTopology, Speaker};

    fn width(n: u16) -> ChannelLayout {
        ChannelLayout::from(n)
    }

    fn stereo() -> ChannelTopology {
        ChannelTopology::new([Speaker::FrontLeft, Speaker::FrontRight])
    }

    /// A plugin's buses are the node's, in order, each with the plugin's role
    /// and placement: a stereo main, a mono sidechain and an aux. A bus
    /// without a placement (or with one that disagrees with its width) is
    /// `Unassigned`, and the shape's ports are every channel.
    ///
    /// Mutations (run), each failing this test: `PluginBus::placement`'s
    /// width filter removed (the disagreeing bus keeps its two speakers); the
    /// role taken as `Main` for the first bus and `Aux` for the rest (the
    /// sidechain is lost); `buses_of` reading `outputs` for the input side.
    #[test]
    fn a_plugins_buses_are_the_nodes_in_order_placed_and_with_their_roles() {
        let loaded = LoadedPlugin {
            inputs: vec![
                PluginBus::new(width(2), BusRole::Main).placed(Some(stereo())),
                PluginBus::new(width(1), BusRole::Sidechain),
                // Declares two speakers for a three-channel bus.
                PluginBus::new(width(3), BusRole::Aux).placed(Some(stereo())),
            ],
            outputs: vec![PluginBus::main(width(2)).placed(Some(stereo()))],
            ..LoadedPlugin::default()
        };
        let shape = Declared::of(&loaded).ports;
        assert_eq!(shape.audio_in(), width(6));
        let ins: Vec<_> = shape
            .buses_in()
            .iter()
            .map(|b| (b.role, b.topology.positions().to_vec()))
            .collect();
        assert_eq!(
            ins,
            [
                (BusRole::Main, vec![Speaker::FrontLeft, Speaker::FrontRight]),
                (BusRole::Sidechain, vec![Speaker::Unassigned]),
                (BusRole::Aux, vec![Speaker::Unassigned; 3]),
            ]
        );
        assert_eq!(shape.buses_in().first_port(2), Some(3));
        assert_eq!(
            shape.buses_out().get(0).unwrap().topology.positions(),
            &[Speaker::FrontLeft, Speaker::FrontRight]
        );
    }

    /// A plugin with no bus list has no buses; one with more than the node
    /// holds, or a bus wider than a bus holds, keeps the first ones and every
    /// channel as a port.
    ///
    /// Mutation (run): the wide bus's `else` arm returning `Buses::NONE` for
    /// the whole side fails this test. (The `break` on a refused `with` is
    /// equivalent to `continue`: a full list refuses every later bus.)
    #[test]
    fn what_does_not_fit_stays_a_port() {
        let none = Declared::of(&LoadedPlugin::default()).ports;
        assert!(none.buses_in().is_empty() && none.buses_out().is_empty());

        let many = LoadedPlugin {
            inputs: vec![PluginBus::main(width(1)); 5],
            outputs: [2, 20, 2].map(|n| PluginBus::main(width(n))).to_vec(),
            ..LoadedPlugin::default()
        };
        let shape = Declared::of(&many).ports;
        assert_eq!(shape.audio_in(), width(5));
        assert_eq!(shape.buses_in().len(), tutti_graph::MAX_BUSES);
        // The 20-wide bus is past a bus; the one after it is not reached.
        assert_eq!(shape.audio_out(), width(24));
        assert_eq!(shape.buses_out().len(), 1);
        assert_eq!(shape.buses_out().width(), 2);
    }
}
