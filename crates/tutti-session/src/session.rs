//! [`Session`]: the graph, its transport and engine side, and the order they
//! run in ([`PHASES`]).

use std::any::Any;
#[cfg(feature = "midi")]
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use tutti_graph::NodeKey;
use tutti_graph::PrerollTable;
use tutti_nodes::ClickSettings;
use tutti_tempo::TempoMap;
use tutti_transport::{FadeOut, LoopEdit, MotionEvent, QueueFull, ScheduleFull, Then, Transport};
use tutti_types::{At, Beat, Bpm, ChannelLayout, ChannelTopology, LoopRange, Samples};

use crate::graph::{Committed, CrossfadeId, Graph, Retried};
use crate::{HeardOpen, HeardTapError, Playhead, RemoveError, ScheduleError, TempoError};

/// One step of [`Session::update`]. [`PHASES`] is their order.
///
/// A host that runs the steps itself calls [`Session::run_phase`] once per phase, in this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Phase {
    /// What the session's jobs sent lands first (`SessionBuilder::spawn_with`:
    /// each message, then each end, in arrival order). Then crossfades that
    /// waited out a re-prepare are retried, what bounces
    /// sent back is dropped (feature `export`; and a bounce in flight for
    /// `BOUNCE_STALL_WARNING` of `PhaseCx::now` warned about, once), then
    /// `Editor::collect` frees what the audio thread retired (here, on the
    /// main thread) and, mid re-prepare, sends the resumed plan.
    Collect,
    /// Hosted plugins' health, latency, parameter edits and state
    /// snapshots (feature `plugin`): a dead plugin's node is removed
    /// (`PluginChange::Died`), a moved latency handed to the graph
    /// (`PluginChange::LatencyChanged`), a due snapshot, a parameter
    /// read and a reload submitted to the session's spawner (their results land in
    /// the next `Phase::Collect`).
    Plugins,
    /// The plugins' render mode (features `plugin` and `export`): Offline
    /// while an export is in flight, Realtime otherwise, each plugin told on
    /// a change or when it was never told.
    RenderMode,
    /// Modulation's apply (feature `modulation`): the declarations (the
    /// host's, `PhaseCx::mods`, or the session's own, `Session::mods_mut`)
    /// onto the graph, staging what is new for the commit. A refusal is
    /// `SessionEvent::Modulation`.
    Modulation,
    /// The commit, iff the graph was edited since the last one, and the
    /// compensation of the plan it sent (or of a re-prepare's resumed plan)
    /// published.
    Commit,
    /// Modulation's re-apply and resyncs (feature `modulation`): while a
    /// target is unresolved (its node was new before the commit), the
    /// declarations apply again and what that built is committed, so the
    /// lane plays from the update its node does; then the note-envelope
    /// resyncs the apply owes are sent.
    AfterCommit,
    /// The hardware out (an event tap on the clock)
    /// drained to the output (feature `midi`; `Midi::set_hardware_out`).
    MidiOut,
    /// The device's faults, as `DeviceEvent::Fault` (feature `device`).
    Poll,
    /// Frees, here, what the audio thread let go of in every cell the
    /// session publishes to (`ArcLend::reclaim`): the project meter and the
    /// plugin slots following it, the modulation
    /// lanes' programs, the session's clips, the hardware input's MPE
    /// request and port set. A value a block still held when it was
    /// replaced stays retired until a scan after the block ends; a cell
    /// that is not published again gets that scan here. A lock and a
    /// length check per cell when nothing is retired. It also closes the
    /// graph's taps whose readers were dropped
    /// ([`Taps::open`](crate::Taps::open),
    /// [`Taps::open_event`](crate::Taps::open_event)).
    Reclaim,
}

/// Every [`Phase`], in the order [`Session::update`] runs them.
pub const PHASES: &[Phase] = &[
    Phase::Collect,
    Phase::Plugins,
    Phase::RenderMode,
    Phase::Modulation,
    Phase::Commit,
    Phase::AfterCommit,
    Phase::MidiOut,
    Phase::Poll,
    Phase::Reclaim,
];

/// What one phase reads besides the session.
#[derive(Clone, Copy, Debug)]
pub struct PhaseCx<'a> {
    /// The time thresholds are measured on (a test passes its own).
    pub now: Instant,
    /// The host's modulation declarations and the resolver for its keys;
    /// `None`: the session's own ([`Session::mods_mut`]). Read by the
    /// [`Modulation`](Phase::Modulation) and
    /// [`AfterCommit`](Phase::AfterCommit) phases.
    #[cfg(feature = "modulation")]
    pub mods: Option<ModInput<'a>>,
    _borrow: std::marker::PhantomData<&'a ()>,
}

impl PhaseCx<'_> {
    /// A context at `now`, with the session's own declarations.
    pub fn at(now: Instant) -> Self {
        Self {
            now,
            #[cfg(feature = "modulation")]
            mods: None,
            _borrow: std::marker::PhantomData,
        }
    }
}

#[cfg(feature = "modulation")]
impl<'a> PhaseCx<'a> {
    /// The same context with the host's declarations.
    #[must_use]
    pub fn with_mods(mut self, mods: ModInput<'a>) -> Self {
        self.mods = Some(mods);
        self
    }
}

/// A host's modulation declarations, borrowed for one phase, and how its
/// keys resolve to nodes.
#[cfg(feature = "modulation")]
#[derive(Clone, Copy)]
pub struct ModInput<'a> {
    /// The declarations, as `tutti_modulation` takes them.
    pub declarations: tutti_modulation::Declarations<'a>,
    /// The host's keys, resolved.
    pub keys: &'a dyn crate::ModKeys,
}

#[cfg(feature = "modulation")]
impl std::fmt::Debug for ModInput<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModInput").finish_non_exhaustive()
    }
}

/// Something an [`update`](Session::update) found for the host.
#[non_exhaustive]
pub enum SessionEvent {
    /// The graph's edits could not be committed: the plan sent last keeps
    /// playing. Each update tries again; a refusal is reported once, until a
    /// commit is sent or it fails another way.
    CommitRefused(tutti_graph::CommitError),
    /// A crossfade that waited out a re-prepare
    /// ([`Crossfade::Waiting`](crate::Crossfade::Waiting)) swapped its node
    /// in: `controls` are the incoming node's `IntoNode::Controls`, boxed
    /// (`()` for a plugin), its params already addressed.
    CrossfadeLanded {
        /// The node it swapped behind.
        node: NodeKey,
        /// The id [`Graph::crossfade`] returned.
        id: CrossfadeId,
        /// The incoming node's controls.
        controls: Box<dyn Any + Send>,
    },
    /// A crossfade that waited out a re-prepare was refused when it was
    /// retried (the graph is poisoned, or the node went): the incoming node
    /// was dropped.
    CrossfadeFailed {
        /// The node it was to swap behind.
        node: NodeKey,
        /// The id [`Graph::crossfade`] returned.
        id: CrossfadeId,
        /// Why.
        refused: crate::CrossfadeRefused,
    },
    /// A node's event output port lost events since the last update: they
    /// were past its capacity (a note-off only past the room set aside for
    /// note-offs), a PDC or feedback delay it feeds was full, or the node
    /// left out an event because the one it pairs with was refused (a clip's
    /// note-off of a note-on the port refused). One per port per update, the
    /// sum over the blocks of the update. A note-off refused and sent again
    /// later counts as refused.
    ///
    /// `port` is `None` for a loss no output port owns. The graph's total is
    /// [`LoadReading::dropped_events`](tutti_metering::LoadReading::dropped_events), exact
    /// where this is per port: a block that loses at more than 64 places, or
    /// a host that does not run `Collect` for long enough to fill a ring of
    /// 256 entries, leaves some out here and not there.
    EventsDropped {
        /// The node.
        node: NodeKey,
        /// Its event output port, `None` for a loss the node had that no
        /// output port owns: events a plugin took in and did not deliver, a
        /// scheduled command that did not fit.
        port: Option<u16>,
        /// How many events were refused or withheld.
        count: u64,
        /// How many of them a node says it then wrote (a note-off it sent
        /// again): `count - recovered` never arrived, summed over updates.
        recovered: u64,
    },
    /// Events the graph lost that no [`EventsDropped`](Self::EventsDropped)
    /// names: a merge's, and those a block that lost at more than 64 places
    /// or a full report ring left out. The count since the last update.
    EventsDroppedElsewhere(u64),
    /// The engine rendered silence for blocks it could not render: an
    /// **audible dropout** (the listener hears a gap, and a heard tap or
    /// recording takes the silence too). The count is the blocks silenced
    /// since the last update that reported, one event per update that saw
    /// any; it is derived on the control side from
    /// [`LoadReading::silenced`](tutti_metering::LoadReading::silenced), so
    /// the audio thread does nothing more than count.
    ///
    /// A block is silenced when the graph was held by the control side as
    /// the callback came (an edit applying while the device asked for
    /// audio), when the output buffer's width is one the engine has no fold
    /// for ([`Engine::support_output`](tutti_engine::Engine::support_output)),
    /// when the graph has no output channels, or when the graph's root width
    /// cannot be placed on the device's. Blocks that merely ran past their
    /// budget are not silenced and have no event: that is the polled
    /// [`LoadReading::over_budget`](tutti_metering::LoadReading::over_budget).
    /// A [`LoadMeter::reset`](tutti_metering::LoadMeter::reset) between
    /// updates restarts the count without an event.
    BlocksSilenced(u64),
    /// The sound card or its devices: a fault, a device that came or went, a
    /// restart wanted, done or failed ([`DeviceEvent`](crate::DeviceEvent)).
    #[cfg(feature = "device")]
    Device(crate::DeviceEvent),
    /// A take of the session's: its first frame arrived, it ended, or it
    /// failed ([`TakeEvent`](crate::TakeEvent)).
    #[cfg(all(feature = "device", feature = "audio-io"))]
    Take(crate::TakeEvent),
    /// A modulation problem (design 015 §4.7), the one type
    /// [`ModState::problems`](crate::ModState::problems) lists. Reported
    /// once: a refusal when it starts or changes (the problems list holds it
    /// while it lasts), a target that has stayed unresolved for the session's
    /// warning time ([`UNRESOLVED_WARNING`](crate::UNRESOLVED_WARNING) by
    /// default) of [`PhaseCx::now`], likely a key no node is ever bound to,
    /// once per stretch it stays unresolved.
    #[cfg(feature = "modulation")]
    Modulation(crate::Problem),
    /// A MIDI event of the session's, by area (design 030, convention 1):
    /// a device came or went, a held input or the output was lost, a
    /// negotiation settled, the device watch ended, or events were dropped.
    /// See [`MidiEvent`](crate::MidiEvent).
    #[cfg(feature = "midi")]
    Midi(crate::MidiEvent),
    /// A stream an [`open_stream`](Session::open_stream) started opened or
    /// failed to. See [`SamplerEvent`](crate::SamplerEvent).
    #[cfg(feature = "sampler")]
    Sampler(crate::SamplerEvent),
    /// A hosted plugin's event, by area (design 029 section 3.3, design 030
    /// convention 1): what the plugin reported (its own edit of a parameter, a
    /// new tail) and every transition of its life (died, reloading, recovered,
    /// gave up). See [`PluginEvent`](crate::PluginEvent).
    #[cfg(feature = "plugin")]
    Plugin(crate::PluginEvent),
}

impl std::fmt::Debug for SessionEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CommitRefused(e) => f.debug_tuple("CommitRefused").field(e).finish(),
            Self::CrossfadeLanded { node, id, .. } => f
                .debug_struct("CrossfadeLanded")
                .field("node", node)
                .field("id", id)
                .finish_non_exhaustive(),
            Self::CrossfadeFailed { node, id, refused } => f
                .debug_struct("CrossfadeFailed")
                .field("node", node)
                .field("id", id)
                .field("refused", refused)
                .finish(),
            #[cfg(feature = "device")]
            Self::Device(event) => f.debug_tuple("Device").field(event).finish(),
            #[cfg(all(feature = "device", feature = "audio-io"))]
            Self::Take(event) => f.debug_tuple("Take").field(event).finish(),
            #[cfg(feature = "modulation")]
            Self::Modulation(problem) => f.debug_tuple("Modulation").field(problem).finish(),
            #[cfg(feature = "midi")]
            Self::Midi(event) => f.debug_tuple("Midi").field(event).finish(),
            #[cfg(feature = "sampler")]
            Self::Sampler(event) => f.debug_tuple("Sampler").field(event).finish(),
            Self::EventsDropped {
                node,
                port,
                count,
                recovered,
            } => f
                .debug_struct("EventsDropped")
                .field("node", node)
                .field("port", port)
                .field("count", count)
                .field("recovered", recovered)
                .finish(),
            Self::BlocksSilenced(count) => f.debug_tuple("BlocksSilenced").field(count).finish(),
            Self::EventsDroppedElsewhere(count) => f
                .debug_tuple("EventsDroppedElsewhere")
                .field(count)
                .finish(),
            #[cfg(feature = "plugin")]
            Self::Plugin(event) => f.debug_tuple("Plugin").field(event).finish(),
        }
    }
}

/// The engine's MIDI, as graph nodes the builder inserts: the hardware
/// input's ingest and the clock (Beat Clock and MTC, off until a host enables it),
/// with their controls. The hardware out is not a node: the session reads an
/// event tap on the clock's output ([`Midi::set_hardware_out`](crate::Midi::set_hardware_out)).
#[cfg(feature = "midi")]
#[derive(Clone, Debug)]
pub struct MidiNodes {
    /// The hardware input's ingest node: its ports feed what a route rule names.
    pub input: NodeKey,
    /// Which of its event outputs carries which Group and channel
    /// ([`SessionBuilder::midi_groups`](crate::SessionBuilder::midi_groups)).
    pub input_ports: tutti_midi_runtime::MidiInputPorts,
    /// How many hardware devices the input takes at once: it has that many
    /// event inputs, one for each
    /// ([`SessionBuilder::midi_devices`](crate::SessionBuilder::midi_devices)).
    pub input_devices: NonZeroUsize,
    /// Its controls (the MPE mode).
    pub input_controls: tutti_midi_runtime::MidiIngestControls,
    /// The clock's node.
    pub clock: NodeKey,
    /// The clock master: enable it, pick MTC and its frame rate.
    pub clock_master: tutti_midi_runtime::ClockControls,
}

/// Whether the root is as it was asked to be
/// ([`Session::root_topology_status`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootTopologyStatus {
    /// In force: the root is what was declared (or nothing was).
    Applied,
    /// Not in force, and why. The previous state stands.
    Refused(tutti_types::FoldError),
}

/// The tutti engine: a [`Graph`], its transport, and the engine
/// side it renders on, in the order [`update`](Self::update) runs them.
///
/// Built with [`SessionBuilder`](crate::SessionBuilder): offline (the host
/// pulls blocks with [`render`](Self::render)), on a device (with a
/// `Device`, feature `device`), or detached (the executor stays on
/// this side: [`render_frame`](Self::render_frame)).
///
/// `Send`, not `Sync`: one control thread owns it, whichever thread that is.
/// Nothing the session frees needs the main thread: a plugin a removal, a
/// replacement or a crossfade retires is parked in a graveyard, which the main
/// thread empties (`MainSession::update`, with `plugin`), and a format closes an
/// in-process plugin that is released off the main thread there
/// (`tutti_plugin_types::defer_to_main`). A host that never claims a main
/// thread keeps the parked plugins until it does, and the session says so once
/// (`Session::retired_plugins`). What does need the main thread (an editor,
/// loading an in-process plugin, restoring state) takes the proof
/// (`MainThread`), or is the view's (`Session::main`).
///
/// ```
/// use tutti_types::MaxBlock;
/// use tutti_types::{ChannelLayout, InterleavedMut, SampleRate, Samples};
/// use tutti_graph::ForkByClone;
/// use tutti_session::{OutPort, Root, Session};
/// use tutti_nodes::testing::Const;
///
/// // One global output: the mono device takes it as it is, unfolded.
/// let mut session = Session::builder()
///     .outputs_floor(ChannelLayout::MONO)
///     .build_offline(SampleRate::SR_48K, ChannelLayout::MONO, MaxBlock::of(Samples(64)))
///     .unwrap();
/// let (dc, _) = session.graph().add(ForkByClone(Const::mono(0.5)));
/// session.graph().set_sources_hard(Root::output(0), [OutPort::new(dc, 0)]).unwrap();
/// session.update();
///
/// let mut block = [0.0f32; 64];
/// session.render(&mut InterleavedMut::new(&mut block, ChannelLayout::MONO));
/// assert!(block.iter().all(|&s| s == 0.5));
/// ```
#[derive(Debug)]
pub struct Session {
    pub(crate) graph: Graph,
    pub(crate) transport: Transport,
    /// The engine, for an offline session (the host pulls blocks); `None` on
    /// a device (the callback holds it) or detached.
    pub(crate) engine: Option<tutti_engine::Engine>,
    /// The engine's load meter, offline or on a device; `None` detached.
    pub(crate) load: Option<tutti_metering::LoadMeter>,
    /// The engine's tap after the fader and the device width it was built
    /// for; `None` detached.
    pub(crate) heard: Option<(tutti_engine::HeardTaps, ChannelLayout)>,
    /// The engine's block clock, for [`playhead`](Self::playhead); `None`
    /// detached, where nothing renders.
    pub(crate) clock: Option<tutti_engine::BlockClockReader>,
    /// The root topology last asked for
    /// ([`declare_root_topology`](Self::declare_root_topology)), applied or not.
    pub(crate) root_request: Option<ChannelTopology>,
    /// Why `root_request` is not in force, if it is not.
    pub(crate) root_refusal: Option<tutti_types::FoldError>,
    pub(crate) click: Option<NodeKey>,
    pub(crate) click_settings: Arc<ClickSettings>,
    /// The per-channel pre-roll of the plan sent last, for sources outside
    /// the graph (a disk stream), republished with every plan.
    pub(crate) compensation: Arc<PrerollTable>,
    /// The total latency of the plan sent last.
    pub(crate) latency: Samples,
    /// A re-prepare resumed in this update's collect: its plan's figures
    /// are published at the commit.
    pub(crate) resumed: bool,
    /// [`LoadReading::silenced`](tutti_metering::LoadReading::silenced) as
    /// the last update read it: the next update reports the rise
    /// ([`SessionEvent::BlocksSilenced`]).
    pub(crate) silenced_seen: u64,
    /// Whether the last motion this session sent for the next block left the
    /// transport rolling (a play, a rolling locate) or stopped. The control
    /// side's own word, which the playhead (one block old) cannot give.
    pub(crate) rolling_sent: std::sync::atomic::AtomicBool,
    #[cfg(feature = "midi")]
    pub(crate) midi: Option<MidiNodes>,
    /// Clips, live inputs, routes and the hardware out's output.
    #[cfg(feature = "midi")]
    pub(crate) midi_state: crate::midi::MidiState,
    /// What modulation applied, and the session's own declarations.
    #[cfg(feature = "modulation")]
    pub(crate) mods: crate::modulation::ModSession,
    #[cfg(feature = "midi-hardware")]
    pub(crate) midi_io: Option<tutti_midi_hardware::MidiSession>,
    #[cfg(feature = "sampler")]
    pub(crate) disk_streamer: Option<tutti_sampler::DiskStreamer>,
    /// The last [`SamplerLoad`](crate::SamplerLoad) minted.
    #[cfg(feature = "sampler")]
    pub(crate) sampler_opens: u64,
    /// The device's fault record, shared with the [`Device`](crate::Device),
    /// and the count the last poll saw.
    #[cfg(feature = "device")]
    pub(crate) faults: Option<(Arc<tutti_cpal::StreamFaults>, u64)>,
    /// The device watch, and what restarts reported since the
    /// last poll.
    #[cfg(feature = "device")]
    pub(crate) device_watch: Option<crate::DeviceWatch>,
    #[cfg(feature = "device")]
    pub(crate) device_events: Vec<SessionEvent>,
    /// The one-shot listing of the output devices, until it answers.
    #[cfg(feature = "device")]
    pub(crate) output_listing: Option<crate::watch::OutputListing>,
    /// A listing was started by [`Session::list_outputs_once`].
    #[cfg(feature = "device")]
    pub(crate) outputs_listed: bool,
    /// The capture inputs added ([`Session::add_input`]) and the host they open on.
    #[cfg(all(feature = "device", feature = "audio-io"))]
    pub(crate) inputs: crate::input::Inputs,
    /// How often a healthy plugin's state is captured.
    #[cfg(feature = "plugin")]
    pub(crate) snapshot_interval: std::time::Duration,
    /// What a hosted plugin's death or hang does; `None`: it is removed.
    #[cfg(feature = "plugin")]
    pub(crate) recovery: Option<crate::RecoveryPolicy>,
    /// The `now` of the last `Phase::Plugins`: the time a landing reload is
    /// measured at.
    #[cfg(feature = "plugin")]
    pub(crate) plugins_now: Instant,
    /// The render mode last announced.
    #[cfg(feature = "plugin")]
    pub(crate) render_mode: crate::plugin::RenderModeState,
    /// The bounce token and the leftovers' channel.
    #[cfg(feature = "export")]
    pub(crate) bounce: crate::bounce::BounceState,
    /// The blocking work running on the spawner, landed in `Phase::Collect`.
    pub(crate) jobs: crate::jobs::Jobs,
}

impl Session {
    /// A builder with the defaults.
    pub fn builder() -> crate::SessionBuilder {
        crate::SessionBuilder::default()
    }

    /// The graph, to edit. An edit marks it edited; the next
    /// [`update`](Self::update) commits every edit since the last at once.
    pub fn graph(&mut self) -> &mut Graph {
        &mut self.graph
    }

    /// The graph, to read.
    pub fn graph_ref(&self) -> &Graph {
        &self.graph
    }

    /// Declares what the graph root's channels are: the session's output as the
    /// engine and an export place it on the device and in a file.
    /// `Some(`[`ChannelTopology::unassigned`]`)` is independent buses, kept by
    /// index (a 4-wide one on a 6-wide device stays at 0..4, where a bare
    /// 4 is quad and its rears land at 4 and 5); a named topology is speaker
    /// feeds, folded by position. `None` takes the declaration back, and the
    /// root is the engine's order for its width again. **Nothing changes
    /// until one is declared.**
    ///
    /// The declaration states the root, so it **sets the root's width**: the
    /// graph's outputs become exactly as many as `root` has, widened or
    /// narrowed (channels past it are dropped), with no floor of the device's
    /// ([`root_width`](crate::root_width)). Afterwards the width and the
    /// declaration cannot disagree: [`Graph::widen_outputs`] refuses to widen a
    /// declared root. Declare again to change both. An export of the master
    /// places the file from the declaration (`tutti_export::Setup::with_root`, set by the export's fork).
    ///
    /// The device a declaration must place on is the one the session renders
    /// to (`build_offline`'s layout, or the open device's, and the new one on a
    /// restart); a detached session has none, and the file is checked at the
    /// export.
    ///
    /// # Errors
    ///
    /// [`Error::Output`](crate::Error::Output) when the declaration is empty, wider than the engine
    /// renders ([`MAX_ROOT_CHANNELS`](tutti_graph::MAX_ROOT_CHANNELS)), or the
    /// device cannot place it (an unassigned root wider than the device, a channel
    /// no speaker names). Nothing is changed.
    pub fn declare_root_topology(&mut self, root: Option<ChannelTopology>) -> crate::Result<()> {
        // The same request again, in force, is nothing to do.
        if root == self.root_request && self.root_refusal.is_none() {
            return Ok(());
        }
        let device = self.heard.as_ref().map(|(_, layout)| *layout);
        self.root_request = root.clone();
        match self.graph.declare_root(root, device) {
            Ok(()) => {
                self.root_refusal = None;
                Ok(())
            }
            Err(e) => {
                self.root_refusal = Some(e);
                Err(e.into())
            }
        }
    }

    /// The topology last asked for with
    /// [`declare_root_topology`](Self::declare_root_topology) (or
    /// [`SessionBuilder::root_topology`](crate::SessionBuilder::root_topology)),
    /// whether or not it is in force: see
    /// [`root_topology_status`](Self::root_topology_status).
    pub fn requested_root_topology(&self) -> Option<&ChannelTopology> {
        self.root_request.as_ref()
    }

    /// Whether the root is as it was asked to be. A refusal is a value here,
    /// never only a log line.
    ///
    /// - [`RootTopologyStatus::Refused`] with the declaration's own
    ///   [`FoldError`](tutti_types::FoldError) when the last request could not
    ///   be placed on the device. The previous declaration (the one
    ///   [`root_topology`](Self::root_topology) reads) stays in force, and a
    ///   **device restart re-attempts the request**: it is
    ///   [`Applied`](RootTopologyStatus::Applied) as soon as the new device
    ///   places it.
    /// - Otherwise `Refused` with [`Graph::widen_outputs`]'s last refusal: a
    ///   widening the declared root could not take
    ///   ([`FoldError::RootWidth`](tutti_types::FoldError::RootWidth)), or
    ///   channels past `MAX_ROOT_CHANNELS`
    ///   ([`FoldError::Unplaced`](tutti_types::FoldError::Unplaced)); that
    ///   is what a host's `MasterSources` longer than the root comes to.
    /// - Otherwise `Applied`.
    pub fn root_topology_status(&self) -> RootTopologyStatus {
        match self.root_refusal.or(self.graph.widen_refusal()) {
            Some(e) => RootTopologyStatus::Refused(e),
            None => RootTopologyStatus::Applied,
        }
    }

    /// The topology the root was declared with
    /// ([`declare_root_topology`](Self::declare_root_topology)), if one was.
    pub fn root_topology(&self) -> Option<&ChannelTopology> {
        self.graph.root_topology()
    }

    /// Runs every phase in [`PHASES`] order at `Instant::now()` with the
    /// session's own declarations, and returns what it found.
    ///
    /// With `plugin`, a host that has the main thread calls
    /// `MainSession::update` instead, which also drops the retired plugins and
    /// services the editors (see the type docs).
    #[must_use = "the events carry faults and dropouts (a refused commit, a plugin that died, a lost block): handle them, or bind `let _events` with a reason"]
    pub fn update(&mut self) -> Vec<SessionEvent> {
        let mut out = Vec::new();
        let cx = PhaseCx::at(Instant::now());
        for &phase in PHASES {
            self.run_phase(phase, cx, &mut out);
        }
        out
    }

    /// Runs one phase, pushing what it found onto `out`. A host running the
    /// phases itself runs them in [`PHASES`] order, once each per update.
    pub fn run_phase(&mut self, phase: Phase, cx: PhaseCx<'_>, out: &mut Vec<SessionEvent>) {
        #[cfg(not(feature = "modulation"))]
        let _ = cx;
        match phase {
            Phase::Collect => {
                // First, so what a job landed is in the graph this update commits.
                self.land_jobs(out);
                // Before the collect, as a frame's spawns run before its
                // commit: a crossfade lands on the update that first finds
                // the re-prepare resumed.
                for retried in self.graph.retry_crossfades() {
                    out.push(match retried {
                        Retried::Landed { node, id, controls } => {
                            SessionEvent::CrossfadeLanded { node, id, controls }
                        }
                        Retried::Failed { node, id, refused } => {
                            SessionEvent::CrossfadeFailed { node, id, refused }
                        }
                    });
                }
                // What an export left behind (a render's fork, an edit
                // that never ran) drops here, on the main thread.
                #[cfg(feature = "export")]
                {
                    self.bounce.drop_leftovers();
                    self.bounce.watch_stall(cx.now);
                }
                let repreparing = self.graph.is_repreparing();
                self.graph.collect();
                // Closes a plugin that was dropped off its own thread (a pool thread at an
                // export's end, say): its owner handed the close to the main thread, which is
                // this tick. `MainThread::current` is `Some` only on the claiming thread.
                #[cfg(feature = "plugin")]
                if let Some(main) = tutti_plugin_types::MainThread::current() {
                    tutti_plugin_types::run_deferred(&main);
                }
                self.report_event_losses(out);
                self.report_silenced(out);
                // A re-prepare resumed in that collect: it sent the resumed
                // plan, whose shapes are new (a lookahead is a time, so a
                // rate change moves latencies).
                self.resumed |= repreparing && !self.graph.is_repreparing();
            }
            Phase::Commit => {
                let sent = self.graph.is_edited() && self.commit_reporting(out);
                if !sent && self.resumed {
                    self.publish_sent();
                }
                self.resumed = false;
            }
            Phase::Modulation =>
            {
                #[cfg(feature = "modulation")]
                self.mods
                    .modulation_phase(&mut self.graph, cx.mods, cx.now, out)
            }
            Phase::AfterCommit => {
                #[cfg(feature = "modulation")]
                {
                    // The re-apply's commit, as the update's own: published
                    // on `Sent`.
                    if self.mods.reapply(&mut self.graph, cx.mods, cx.now, out) {
                        self.commit_reporting(out);
                    }
                    self.mods.resync(&mut self.graph);
                }
            }
            Phase::MidiOut => {
                #[cfg(feature = "midi")]
                {
                    let playhead = self.playhead();
                    self.midi_state
                        .midi_out_phase(&mut self.graph, &playhead, cx.now, out)
                }
            }
            Phase::Plugins =>
            {
                #[cfg(feature = "plugin")]
                self.plugins_phase(cx, out)
            }
            Phase::RenderMode =>
            {
                #[cfg(feature = "plugin")]
                self.render_mode_phase()
            }
            Phase::Poll => {
                // A device that connected is heard from the plan that holds
                // its inlet: committed now, not an update later.
                #[cfg(feature = "midi-hardware")]
                if self.midi_state.midi_input_phase(
                    self.midi_io.as_mut(),
                    self.midi.as_ref(),
                    &mut self.graph,
                    cx.now,
                    out,
                ) {
                    self.commit();
                }
                // The tempo the session follows is read after the devices:
                // what a device sent in the block before is in its tap.
                #[cfg(feature = "midi")]
                {
                    let follow = crate::midi_follow::FollowCx {
                        settings: self.transport.settings(),
                        meter: &self.click_settings.project_meter(),
                        playhead: self.playhead(),
                    };
                    let tempo_edited = self.midi_state.follow_phase(&follow, &mut self.graph, out);
                    let (actions, clock_edited) =
                        self.midi_state.clock_phase(&follow, &mut self.graph, out);
                    let (machine_actions, machine_edited) =
                        self.midi_state.machine_phase(&follow, &mut self.graph, out);
                    if tempo_edited || clock_edited || machine_edited {
                        self.commit();
                    }
                    let refused: u64 = actions
                        .into_iter()
                        .chain(machine_actions)
                        .map(|action| u64::from(!self.send_clock_action(action)))
                        .sum();
                    if refused > 0 {
                        out.push(crate::MidiEvent::ClockCommandsRefused(refused).into());
                    }
                }
                #[cfg(feature = "device")]
                self.device_poll(cx.now, out)
            }
            Phase::Reclaim => {
                // Any thread that owns the session: what it frees is
                // tables, meters, programs and clips, never a unit. A tap
                // or meter whose reader was dropped goes out of the next plan.
                self.graph.reap_taps();
                self.graph.reap_event_taps();
                self.graph.reap_meters();
                if let Some((taps, _)) = &self.heard {
                    taps.reclaim();
                }
                self.reclaim();
            }
        }
    }

    /// The blocks the engine silenced since the last update, as one
    /// [`SessionEvent::BlocksSilenced`]. The meter's count is cumulative
    /// (and zeroed by a reset), so the rise is read against the last
    /// reading; a reset restarts from the new count.
    fn report_silenced(&mut self, out: &mut Vec<SessionEvent>) {
        let Some(load) = &self.load else { return };
        let now = load.peek().silenced;
        let rise = now.saturating_sub(self.silenced_seen);
        self.silenced_seen = now;
        if rise > 0 {
            out.push(SessionEvent::BlocksSilenced(rise));
        }
    }

    /// The events the graph's ports lost since the last update, as
    /// [`SessionEvent::EventsDropped`], one per port.
    fn report_event_losses(&mut self, out: &mut Vec<SessionEvent>) {
        let mut sums: std::collections::BTreeMap<(tutti_graph::NodeKey, Option<u16>), (u64, u64)> =
            std::collections::BTreeMap::new();
        for loss in self.graph.take_event_losses() {
            let sum = sums.entry((loss.node, loss.port)).or_default();
            sum.0 += u64::from(loss.count);
            sum.1 += u64::from(loss.recovered);
        }
        out.extend(
            sums.into_iter()
                .filter(|(_, (count, recovered))| *count > 0 || *recovered > 0)
                .map(
                    |((node, port), (count, recovered))| SessionEvent::EventsDropped {
                        node,
                        port,
                        count,
                        recovered,
                    },
                ),
        );
        let elsewhere = self.graph.take_unreported_event_losses();
        if elsewhere > 0 {
            out.push(SessionEvent::EventsDroppedElsewhere(elsewhere));
        }
    }

    /// The [`Reclaim`](Phase::Reclaim) phase: every cell the session
    /// publishes to, scanned and what no block holds freed. Returns how many
    /// retired values are still held (by a block in progress).
    ///
    /// The nodes are the graph's table: each node's reclaimer, kept by node
    /// as it went in ([`SessionNode::reclaimer`](crate::SessionNode::reclaimer)),
    /// and each hosted plugin's meter. Beside it, the cells the session
    /// publishes to itself: the project meter (and
    /// every slot following it), the transport's tempo map, the modulation lanes (inserted by the
    /// reconciler, not through `add`), the hardware inlets' clock and an
    /// offline engine's output folds.
    pub(crate) fn reclaim(&self) -> usize {
        let mut pinned = self.graph.reclaim();
        pinned += self.click_settings.project_meter().reclaim();
        pinned += self.transport.settings().reclaim_tempo();
        // The engine's output folds are swept by the graph (`Graph::reclaim`),
        // offline or on a device, through the `OutputControl` it kept.
        #[cfg(feature = "modulation")]
        {
            pinned += self.mods.state.reclaim();
        }
        #[cfg(feature = "midi-hardware")]
        if let Some(io) = &self.midi_io {
            pinned += io.ports().inlets().reclaim();
        }
        pinned
    }

    /// Commits every edit since the last commit, now, and on `Sent`
    /// publishes the plan's compensation. For a host that must send an edit
    /// before the next update; [`update`](Self::update) commits iff the
    /// graph was edited.
    pub fn commit(&mut self) -> Committed {
        let committed = self.graph.commit();
        if committed == Committed::Sent {
            self.publish_sent();
        }
        committed
    }

    /// [`commit`](Self::commit), a refusal pushed onto `out`: whether it sent.
    fn commit_reporting(&mut self, out: &mut Vec<SessionEvent>) -> bool {
        match self.commit() {
            Committed::Sent => true,
            Committed::Refused(e) => {
                if self.graph.newly_refused(&e) {
                    out.push(SessionEvent::CommitRefused(e));
                }
                false
            }
            Committed::Retry => false,
        }
    }

    /// Publish the compensation of the plan sent last: the per-channel
    /// pre-roll to [`compensation`](Self::compensation), the total to
    /// [`latency`](Self::latency). Nothing before the first plan.
    pub(crate) fn publish_sent(&mut self) {
        if let Some(sent) = self.graph.sent_compensation() {
            self.latency = sent.total();
            self.compensation.set(sent.channels());
        }
    }

    /// Takes `node` out of the graph ([`Graph::remove`]). Any thread that
    /// owns the session: the removal frees nothing itself. The unit is freed
    /// when the executor hands it back, at a collect, which is where the
    /// main-thread guard is (see the type docs).
    ///
    /// The session's own declarations on `node` go with it: its modulation
    /// ranges, automation and the routes into it (`mods`, feature
    /// `modulation`), and, when it is a clip or live input of the
    /// session's (`midi`, feature `midi`), that; a clip's MPE transform goes
    /// with the clip. A clip or live input
    /// feeding `node` stays, to be re-aimed (`Midi::retarget`) or removed.
    ///
    /// # Errors
    ///
    /// [`RemoveError::NoSuchNode`] when `node` is not in the graph (removed
    /// already, or never added); nothing changes, the session's declarations
    /// on it included.
    pub fn remove(&mut self, node: impl Into<NodeKey>) -> Result<(), RemoveError> {
        let node = node.into();
        self.graph.remove(node)?;
        #[cfg(feature = "modulation")]
        self.mods.own.forget_node(node);
        #[cfg(feature = "midi")]
        if let Some(pair) = self.midi_state.forget_node(node) {
            // A clip and its transform are one wiring: the other goes too (it
            // is in the graph unless the caller removed it first).
            let _ = self.graph.remove(pair);
        }
        Ok(())
    }

    /// The session's own modulation declarations, read: its routes and
    /// sources ([`ModsView::routes`](crate::ModsView::routes),
    /// [`routes_to`](crate::ModsView::routes_to),
    /// [`sources`](crate::ModsView::sources)). Reading marks nothing
    /// edited; [`mods_mut`](Self::mods_mut) edits.
    #[cfg(feature = "modulation")]
    #[must_use]
    pub fn mods(&self) -> crate::ModsView<'_> {
        crate::Mods::view_of(&self.mods.own)
    }

    /// The session's own modulation declarations, to edit (design 015
    /// §4.3, §5.1): the next update applies them. A session has one
    /// declarer: while these hold anything, a host's
    /// [`ModInput`] is refused.
    #[cfg(feature = "modulation")]
    pub fn mods_mut(&mut self) -> crate::Mods<'_> {
        // Whatever the handle edits is built into the next apply's
        // declarations.
        self.mods.own.edited();
        crate::Mods {
            own: &mut self.mods.own,
        }
    }

    /// The modulation as applied: the lanes, what is unresolved, the
    /// refusal held.
    #[cfg(feature = "modulation")]
    pub fn mod_state(&self) -> &crate::ModState {
        &self.mods.state
    }

    /// The session's MIDI: live inputs, clips, the hardware input's routes
    /// and the hardware out.
    #[cfg(feature = "midi")]
    pub fn midi(&mut self) -> crate::Midi<'_> {
        crate::Midi {
            graph: &mut self.graph,
            state: &mut self.midi_state,
            nodes: self.midi.as_ref(),
            jobs: &mut self.jobs,
            settings: self.transport.settings(),
            meter: self.click_settings.project_meter(),
            #[cfg(feature = "midi-hardware")]
            io: self.midi_io.as_mut(),
        }
    }

    /// Renders one block into `output`, as a device callback would: what an
    /// offline session's host pulls. Every update's commit lands at the
    /// start of the next block.
    ///
    /// # Panics
    ///
    /// On a session not built with
    /// [`build_offline`](crate::SessionBuilder::build_offline): its engine
    /// is a device's callback, or there is none.
    pub fn render(&self, output: &mut tutti_types::InterleavedMut<'_>) {
        self.engine
            .as_ref()
            .expect("render needs an offline session (SessionBuilder::build_offline)")
            .process(output);
    }

    /// Renders one frame on the calling thread under a stopped transport:
    /// no inputs, `output` one sample per global output. Commits any edit
    /// first.
    ///
    /// # Panics
    ///
    /// On a session whose audio side an engine holds, or was taken
    /// (`Graph::take_audio_side`, behind the `testing` feature).
    pub fn render_frame(&mut self, output: &mut [f32]) {
        self.render_frame_at(&tutti_graph::TransportPosition::default(), output);
    }

    /// [`render_frame`](Self::render_frame) with the transport at
    /// `transport`: what plays a clip in a graph driven by hand.
    pub fn render_frame_at(
        &mut self,
        transport: &tutti_graph::TransportPosition,
        output: &mut [f32],
    ) {
        if self.graph.is_edited() {
            self.commit();
        }
        self.graph.render_local(transport, output);
    }

    /// The transport: its rate follows the device's.
    pub fn transport(&self) -> &Transport {
        &self.transport
    }

    /// The playhead, in one read: its beat, the graph frame, the transport
    /// time, whether it is rolling, and the OS clock reading they were taken
    /// at ([`Playhead`]). The audio thread writes it once per render call
    /// through a sequence lock, so the fields are one instant, never a beat
    /// of one call beside the play state of another.
    ///
    /// It moves in block steps; [`beat_now`](Self::beat_now) (or
    /// [`Playhead::beat_now`] on a snapshot already held) is the beat
    /// extrapolated to the moment a UI draws. A session with no engine
    /// rendering (detached, or before the first block) reads
    /// [`Playhead::default`]: stopped at the origin, with no clock reading
    /// ([`Playhead::at`] is `None`). An offline bounce renders a fork on
    /// its own timeline and never moves this one.
    #[must_use]
    pub fn playhead(&self) -> Playhead {
        self.clock
            .as_ref()
            .map_or_else(Playhead::default, tutti_engine::BlockClockReader::playhead)
    }

    /// The playhead's beat now, extrapolated from the last block by
    /// [`Playhead::beat_now`] on [`tutti_rt::monotonic_ns`], the clock the
    /// block was stamped on.
    #[must_use]
    pub fn beat_now(&self) -> Beat {
        self.playhead().beat_now(tutti_rt::monotonic_ns())
    }

    /// Starts the transport rolling from where it stands, at the next block.
    /// A no-op while rolling.
    ///
    /// # Errors
    ///
    /// [`QueueFull`] when the transport's command queue is full; the event
    /// is handed back in it.
    pub fn play(&self) -> Result<(), QueueFull> {
        self.transport.motion().try_send(MotionEvent::Play)?;
        self.rolling_sent.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Stops the transport where it stands, at the next block, after the
    /// [`FadeOut`] it names.
    ///
    /// # Errors
    ///
    /// [`QueueFull`], as [`play`](Self::play).
    pub fn stop(&self, fade: FadeOut) -> Result<(), QueueFull> {
        self.transport
            .motion()
            .try_send(MotionEvent::Stop { fade })?;
        self.rolling_sent.store(false, Ordering::Relaxed);
        Ok(())
    }

    /// Jumps to `beat` at the next block, keeping whatever the transport was
    /// doing (a locate while rolling keeps rolling), with a declick fade.
    ///
    /// # Errors
    ///
    /// [`QueueFull`], as [`play`](Self::play).
    pub fn locate(&self, beat: Beat) -> Result<(), QueueFull> {
        self.transport
            .motion()
            .try_send(MotionEvent::locate(beat))?;
        Ok(())
    }

    /// Sends a motion `event` to take effect at `at`: a frame, a beat, or the
    /// next block (`At`, or a `Beat` or `Frame` as it stands). The one timed verb: `schedule(b, MotionEvent::Play)`
    /// is play at beat `b`, so the session needs one method for the timed
    /// form where it would need three (`play_at`, `stop_at`, `locate_at`), and
    /// every [`MotionEvent`] (a locate that then rolls, a stop that returns
    /// to a beat) is timed the same way. The untimed verbs are `At::NextBlock`.
    ///
    /// A beat is reached by playback: it has a frame only while the transport
    /// rolls. A command that starts the transport rolling ([`MotionEvent::Play`],
    /// or a locate that then rolls) timed at a beat while the transport is
    /// stopped could only fire after the playback it starts, so it is refused
    /// where it is made. Stopped is the control side's word: the last motion
    /// this session sent (a `play` just before is rolling), not the playhead,
    /// which is a block old. A command that does not start playback (a stop, a
    /// plain locate) waits for the playback to reach its beat, as any beat
    /// command does.
    ///
    /// # Errors
    ///
    /// [`ScheduleError::Full`] when the timed-command list is full, and
    /// [`ScheduleError::BeatWhileStopped`] for the command above.
    pub fn schedule(&self, at: impl Into<At>, event: MotionEvent) -> Result<(), ScheduleError> {
        let at = at.into();
        let starts = matches!(
            event,
            MotionEvent::Play
                | MotionEvent::Locate {
                    then: Then::Roll,
                    ..
                }
        );
        // Rolling as far as the control side knows: the last motion this
        // session sent for the next block, or what the engine last rendered
        // (a transport started through `transport()` directly).
        let rolling = self.rolling_sent.load(Ordering::Relaxed) || self.playhead().playing();
        if let At::Beat(beat) = at
            && starts
            && !rolling
        {
            return Err(ScheduleError::BeatWhileStopped { beat, event });
        }
        self.transport.motion().schedule(at, event)?;
        if at == At::NextBlock {
            match event {
                MotionEvent::Play
                | MotionEvent::Locate {
                    then: Then::Roll, ..
                } => self.rolling_sent.store(true, Ordering::Relaxed),
                MotionEvent::Stop { .. } => self.rolling_sent.store(false, Ordering::Relaxed),
                _ => {}
            }
        }
        Ok(())
    }

    /// Sends what a followed MIDI clock asked of the transport, at the next
    /// block, and says whether the transport took it. A command the transport's
    /// list has no room for is dropped and logged, and the caller counts it
    /// ([`MidiEvent::ClockCommandsRefused`](crate::MidiEvent::ClockCommandsRefused)):
    /// the source says its position again with the next message.
    #[cfg(feature = "midi")]
    fn send_clock_action(&self, action: crate::midi_follow::ClockAction) -> bool {
        use crate::midi_follow::ClockAction;
        let event = match action {
            ClockAction::Play => MotionEvent::Play,
            ClockAction::Stop => MotionEvent::stop(),
            ClockAction::Locate(beat) => MotionEvent::locate(beat),
            ClockAction::PlayFrom(beat) => MotionEvent::play_from(beat),
        };
        match self.schedule(At::NextBlock, event) {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!("midi: a followed clock's {event:?} was not sent: {error}");
                false
            }
        }
    }

    /// Loops the transport over `range` from the next block, and arms it: the
    /// engine owns the loop region and takes this as one edit that lands whole
    /// ([`LoopEdit::over`]). Build the range with `LoopRange::try_from(a..b)`,
    /// which names why a region is refused.
    ///
    /// A playhead already at or past the loop's end does not jump: the loop takes
    /// effect once the playhead is inside it.
    ///
    /// # Errors
    ///
    /// [`ScheduleFull`] when the timed-command list is full; the edit is handed
    /// back in it.
    pub fn set_loop(&self, range: LoopRange) -> Result<(), ScheduleFull> {
        self.transport
            .motion()
            .schedule(At::NextBlock, LoopEdit::over(range))
    }

    /// Stops looping from the next block. The loop's bounds are kept, so
    /// [`set_loop`](Self::set_loop) with them (or arming through the transport)
    /// restores it ([`LoopEdit::armed`]).
    ///
    /// # Errors
    ///
    /// [`ScheduleFull`], as [`set_loop`](Self::set_loop).
    pub fn clear_loop(&self) -> Result<(), ScheduleFull> {
        self.transport
            .motion()
            .schedule(At::NextBlock, LoopEdit::armed(false))
    }

    /// Sets a constant project tempo (the project's [`TempoMap`], one step).
    /// The engine takes it at the next block. A tempo that is not finite and
    /// positive leaves the map as it was.
    ///
    /// # Errors
    ///
    /// [`TempoError::Following`] while the session follows a live tempo:
    /// nothing is published.
    pub fn set_tempo(&self, bpm: Bpm) -> std::result::Result<(), TempoError> {
        self.tempo_is_ours()?;
        self.transport.settings().set_tempo(bpm);
        Ok(())
    }

    /// Publishes `map` as the project's tempo (steps and ramps): the engine
    /// takes it at the next block. A map by value works: `map.into()`.
    ///
    /// # Errors
    ///
    /// [`TempoError::Following`] while the session follows a live tempo:
    /// nothing is published.
    pub fn set_tempo_map(
        &self,
        map: impl Into<Arc<TempoMap>>,
    ) -> std::result::Result<(), TempoError> {
        self.tempo_is_ours()?;
        self.transport.settings().set_tempo_map(map);
        Ok(())
    }

    /// Refuses a tempo the app sets while a live tempo owns the map.
    fn tempo_is_ours(&self) -> std::result::Result<(), TempoError> {
        #[cfg(feature = "midi")]
        if self.midi_state.is_following() {
            return Err(TempoError::Following);
        }
        Ok(())
    }

    /// The per-channel compensation of the plan sent last, by output channel:
    /// what the compiler delays each output by, for a host that displays it.
    /// Republished with every plan; empty until the first. A node in the graph
    /// (a disk voice included) is aligned by the compiler, not by this table.
    ///
    /// Shared, not copied: a clone of the `Arc` reads every later plan's table. Read a channel with
    /// [`PrerollTable::preroll`], `None` past the table's end. No audio
    /// thread reads it, so it is a plain lock, not a published cell.
    pub fn compensation(&self) -> &Arc<PrerollTable> {
        &self.compensation
    }

    /// The total latency of the plan sent last: what a DAW displays.
    pub fn latency(&self) -> Samples {
        self.latency
    }

    /// The metronome's node (unwired: the host declares where it lands),
    /// when the builder inserted one.
    pub fn click_node(&self) -> Option<NodeKey> {
        self.click
    }

    /// The metronome's settings.
    pub fn click_settings(&self) -> &Arc<ClickSettings> {
        &self.click_settings
    }

    /// Opens the tap on **what the engine renders**: every output channel at
    /// the device's width, after the declick fader. It is the engine's f32
    /// output buffer, not the device's bytes: the device layer clamps a long
    /// callback to its `MAX_FRAMES` and converts to the device's sample type
    /// after the engine has written the tap. It is read through the same
    /// [`TapReader`](tutti_graph::TapReader) as a plan tap, and is the default way to record what the
    /// output sounds like. A transport stop fades in it as it does in the
    /// engine's output, so a take has no click there; [`Taps::open_output`](crate::Taps::open_output) is the
    /// pre-fader signal of the graph, aligned by the compiler, and steps at a
    /// stop for a transport-gated source.
    ///
    /// It is written by the engine, on every path that renders through it: a
    /// device callback, [`render`](Self::render), and a block the engine
    /// refuses (silence, whole frames, so a take keeps its timing). An export
    /// forks the graph and renders without the engine, so it never writes to
    /// it. There is one at a time; its ring holds `capacity` frames and what
    /// the reader does not drain in time is dropped and counted
    /// ([`TapReader::take_lost`](tutti_graph::TapReader::take_lost)). It flows from the first
    /// render that begins after this returns, and is closed by
    /// [`close_heard_tap`](Self::close_heard_tap) or, once its reader is
    /// dropped, by the next [`Reclaim`](Phase::Reclaim) phase. The width is
    /// the device's when the session was built: a restart onto a device of
    /// another width counts the tap's blocks lost until it is opened again.
    ///
    /// # Errors
    ///
    /// [`HeardTapError::Detached`] on a session without an engine,
    /// and [`HeardTapError::Busy`] while another reader is alive. While the
    /// audio thread has not taken the last installs it is not an error: the
    /// call returns [`HeardOpen::Retry`] and opens nothing.
    pub fn open_heard_tap(&self, capacity: Samples) -> Result<HeardOpen, HeardTapError> {
        let (taps, layout) = self.heard.as_ref().ok_or(HeardTapError::Detached)?;
        taps.open(*layout, capacity)
            .map_err(|_| HeardTapError::Busy)
    }

    /// Closes the tap [`open_heard_tap`](Self::open_heard_tap) opened, now
    /// rather than at the [`Reclaim`](Phase::Reclaim) phase that follows a
    /// dropped reader. The reader keeps what it was written.
    ///
    /// # Errors
    ///
    /// [`HeardTapError::Detached`] on a session without an engine;
    /// [`HeardTapError::NotOpen`] when no heard tap is open (never opened, or
    /// closed already); nothing changes.
    pub fn close_heard_tap(&self) -> Result<(), HeardTapError> {
        let (taps, _) = self.heard.as_ref().ok_or(HeardTapError::Detached)?;
        taps.close().map_err(|e| match e {
            tutti_engine::HeardTapError::NotOpen => HeardTapError::NotOpen,
            _ => HeardTapError::Busy,
        })
    }

    /// The master meter: the level of each graph output channel, one
    /// [`Level`](tutti_graph::Level) per channel, with the peak held since
    /// the last read. It is an op of the plan on the outputs (see
    /// [`Meters::master`](crate::Meters::master)), declared and on
    /// from the build, so every render feeds it: a device callback,
    /// [`render`](Self::render), and a detached session's
    /// [`render_frame`](Self::render_frame). It reads before the engine's fold
    /// to the device and its declick, aligned to the outputs' time. A block
    /// the engine refuses (rendered silent because the control side held the
    /// graph for that callback) runs no op, so it does not touch the meter:
    /// the meter holds what it held and counts no frames for it. `None` once
    /// a host has taken it.
    pub fn master_meter(&mut self) -> Option<&mut tutti_graph::MeterReader> {
        self.graph.meters().master()
    }

    /// Hands the master meter's reader to its one consumer; see
    /// [`master_meter`](Self::master_meter) and
    /// [`Meters::take_master`](crate::Meters::take_master).
    pub fn take_master_meter(&mut self) -> Option<tutti_graph::MeterReader> {
        self.graph.meters().take_master()
    }

    /// The engine's DSP-load meter: each block's render time against its
    /// real-time budget, the smoothed mean, the peak since the last
    /// [`take`](tutti_metering::LoadMeter::take) and the blocks over budget.
    /// Fed by every block the device callback (or, offline,
    /// [`render`](Self::render)) renders; `None` on a detached session,
    /// which has no engine.
    pub fn load_meter(&self) -> Option<&tutti_metering::LoadMeter> {
        self.load.as_ref()
    }

    /// The same meter, mutably: [`take`](tutti_metering::LoadMeter::take)
    /// reads the peak since the last take, so it is the one reader's (the
    /// session holds it; there is no second).
    pub fn load_meter_mut(&mut self) -> Option<&mut tutti_metering::LoadMeter> {
        self.load.as_mut()
    }

    /// The engine's MIDI nodes, when the builder inserted them.
    #[cfg(feature = "midi")]
    pub fn midi_nodes(&self) -> Option<&MidiNodes> {
        self.midi.as_ref()
    }

    /// The raw OS MIDI layer, when the builder opened it
    /// ([`SessionBuilder::midi_hardware`](crate::SessionBuilder::midi_hardware)):
    /// for a test that drives the layer under the session (a fake backend's
    /// hot-plug, a device's ports and negotiation). A host reaches devices
    /// through [`Midi`](crate::Midi) (`s.midi().connect_input(..)`) and the
    /// events of `Session::update`.
    #[cfg(all(feature = "midi-hardware", feature = "testing"))]
    pub fn testing_midi_io(&self) -> Option<&tutti_midi_hardware::MidiSession> {
        self.midi_io.as_ref()
    }
}
