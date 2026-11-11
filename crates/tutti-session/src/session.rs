//! [`Session`]: the graph, its transport and engine side, and the order they
//! run in ([`PHASES`]).

use std::any::Any;
use std::sync::Arc;
use std::time::Instant;

use tutti_graph::NodeKey;
use tutti_graph::PrerollTable;
use tutti_graph::TapReader;
use tutti_nodes::ClickSettings;
use tutti_tempo::TempoMap;
use tutti_transport::{FadeOut, MotionEvent, QueueFull, ScheduleFull, Transport};
use tutti_types::{At, Beat, Bpm, ChannelLayout, ChannelTopology, Samples};

use crate::HeardTapError;
use crate::graph::{Committed, CrossfadeId, Graph, Retried};

/// One step of [`Session::update`]. [`PHASES`] is their order.
///
/// A host that runs the steps itself (a Bevy app runs each in its own
/// system) calls [`Session::run_phase`] once per phase, in this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Phase {
    /// Crossfades that waited out a re-prepare are retried, what bounces
    /// sent back is dropped (feature `export`; and a bounce in flight for
    /// `BOUNCE_STALL_WARNING` of `PhaseCx::now` warned about, once), then
    /// `Editor::collect` frees what the audio thread retired (here, on the
    /// main thread) and, mid re-prepare, sends the resumed plan.
    Collect,
    /// Hosted plugins' health, latency, parameter edits and state
    /// snapshots (feature `plugin`): a dead plugin's node is removed
    /// (`SessionEvent::PluginDied`), a moved latency handed to the graph
    /// (`SessionEvent::PluginLatencyChanged`), a due snapshot handed to the
    /// host as a job (`SessionEvent::PluginSnapshotDue`).
    Plugins,
    /// The plugins' render mode (features `plugin` and `export`): Offline
    /// while an export is in flight, Realtime otherwise, each plugin told on
    /// a change or when it was never told.
    RenderMode,
    /// Modulation's apply (feature `modulation`): the declarations (the
    /// host's, `PhaseCx::mods`, or the session's own, `Session::mods`)
    /// onto the graph, staging what is new for the commit. A refusal is
    /// `SessionEvent::ModulationRefused`.
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
    /// The device's faults, as `SessionEvent::DeviceFault` (feature `device`).
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
    /// ([`Graph::open_tap`](crate::Graph::open_tap),
    /// [`Graph::open_event_tap`](crate::Graph::open_event_tap)).
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
    /// `None`: the session's own ([`Session::mods`]). Read by the
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
    /// The audio backend reported a fault (a device unplugged mid-session):
    /// the most recent one since the last poll.
    #[cfg(feature = "device")]
    DeviceFault(tutti_cpal::StreamFault),
    /// A device is no longer listed by the host (a [`DeviceWatch`](crate::DeviceWatch)
    /// is running; design 018). The stream's own loss is
    /// [`DeviceFault`](Self::DeviceFault).
    #[cfg(feature = "device")]
    DeviceLost {
        /// Which way it played or captured.
        direction: tutti_cpal::Direction,
        /// The device.
        id: tutti_cpal::DeviceId,
    },
    /// A device is listed that was not (the first look lists them all).
    #[cfg(feature = "device")]
    DeviceAppeared {
        /// Which way it plays or captures.
        direction: tutti_cpal::Direction,
        /// The device.
        id: tutti_cpal::DeviceId,
    },
    /// The host's default device moved; `None`: there is none now.
    #[cfg(feature = "device")]
    DefaultChanged {
        /// Which direction's default.
        direction: tutti_cpal::Direction,
        /// The new default.
        id: Option<tutti_cpal::DeviceId>,
    },
    /// The device policy wants the stream restarted: run it with
    /// [`Session::restart_wanted`](crate::Session::restart_wanted) (a
    /// headless host) or the adapter's restart. The outcome is reported by
    /// the next update, and tells the policy, which waits out its backoff
    /// before it asks again.
    #[cfg(feature = "device")]
    RestartWanted(crate::DeviceRestart),
    /// A restart moved the stream. A host that holds a capture stream at the
    /// graph's rate (`MicIn`) reopens it when
    /// [`rate_changed`](crate::DeviceRestarted::rate_changed): the session
    /// does not hold it.
    #[cfg(feature = "device")]
    DeviceRestarted(crate::DeviceRestarted),
    /// The listing started with [`Session::list_outputs`] answered: the
    /// host's output devices, in enumeration order. Reported once.
    #[cfg(feature = "device")]
    OutputsListed(Vec<tutti_cpal::DeviceId>),
    /// The listing started with [`Session::list_outputs`] failed (the
    /// message is the error's). Reported once.
    #[cfg(feature = "device")]
    OutputsListFailed(String),
    /// The listing started with [`Session::list_outputs`] has not answered
    /// after [`LISTING_WARNING`](crate::LISTING_WARNING) of
    /// [`PhaseCx::now`]: the host's enumeration is stuck or slow. Reported
    /// once; the listing goes on waiting.
    #[cfg(feature = "device")]
    OutputsListSlow(std::time::Duration),
    /// A restart was refused or failed (the message is the error's). See
    /// [`Session::restart`](crate::Session::restart) for the stream's state.
    #[cfg(feature = "device")]
    RestartFailed(String),
    /// The modulation declarations were refused, in the host's keys
    /// (design 015 §4.7): reported once, when the refusal starts or
    /// changes; [`ModState::refusal`](crate::ModState::refusal) holds it
    /// while it lasts.
    #[cfg(feature = "modulation")]
    ModulationRefused(crate::ModRefusal),
    /// A modulation target has stayed unresolved for the session's warning
    /// time ([`UNRESOLVED_WARNING`](crate::UNRESOLVED_WARNING) by default)
    /// of [`PhaseCx::now`]: likely a key no node is ever bound to. Reported
    /// once per stretch it stays unresolved.
    #[cfg(feature = "modulation")]
    ModulationUnresolved {
        /// The target, in the host's key.
        target: tutti_modulation::TargetKey,
        /// How long it has been unresolved.
        waited: std::time::Duration,
    },
    /// The hardware out dropped events for want of an output: the total so
    /// far, reported when it rises, at most once per
    /// [`MIDI_OUT_DROP_REPORT`](crate::MIDI_OUT_DROP_REPORT) of
    /// [`PhaseCx::now`].
    #[cfg(feature = "midi")]
    MidiOutDropped(u64),
    /// The hardware devices' event inlets dropped events because they were
    /// full: the total so far, reported when it rises, at most once per
    /// [`MIDI_INPUT_DROP_REPORT`](crate::MIDI_INPUT_DROP_REPORT) of
    /// [`PhaseCx::now`]. An inlet's ring is emptied each block; it fills when
    /// blocks stop coming (a stalled callback) or the device sends faster than
    /// they empty it.
    #[cfg(feature = "midi-hardware")]
    MidiInputDropped(u64),
    /// A MIDI device was added, removed or changed, in the order the OS
    /// reported (design 017 §5). The session reads the devices in
    /// `Phase::Poll`; the list at any time is
    /// [`MidiSession::devices`](tutti_midi_hardware::MidiSession::devices).
    #[cfg(feature = "midi-hardware")]
    MidiDevice(tutti_midi_hardware::DeviceEvent),
    /// A device the session held open as an input vanished: its connection
    /// is closed and its port retired, after the [`MidiDevice`](Self::MidiDevice)
    /// `Removed` that said so. What the device had queued is discarded; notes
    /// it held are not released (design 017 §8.3 is not built).
    #[cfg(feature = "midi-hardware")]
    MidiInputLost(tutti_midi_hardware::DeviceInfo),
    /// A device the session holds open as an input has settled its UMP Stream
    /// negotiation (design 017 §5, M2-104 §7.1.6.1): the Endpoint it declared,
    /// or the OS's description of it where it did not answer, and the Protocol
    /// in use. Reported once per connection, from `Phase::Poll`; the state
    /// stays readable as
    /// [`MidiSession::stream`](tutti_midi_hardware::MidiSession::stream).
    #[cfg(feature = "midi-hardware")]
    MidiStream {
        /// The device.
        device: tutti_midi_hardware::DeviceId,
        /// What is known of it (boxed: an Endpoint is large).
        stream: Box<tutti_midi_hardware::DeviceStream>,
    },
    /// The device the session held open as its output vanished: the output is
    /// closed, and what the hardware out sends is counted as dropped until a
    /// host connects another.
    #[cfg(feature = "midi-hardware")]
    MidiOutputLost(tutti_midi_hardware::DeviceInfo),
    /// The OS watch for MIDI devices ended on its own (the reason is the
    /// message): the device list is frozen at what it last saw and no further
    /// hot-plug is reported. Reported once.
    #[cfg(feature = "midi-hardware")]
    MidiWatchFailed(tutti_midi_hardware::WatchFailure),
    /// A hosted plugin was declared dead (`tutti_plugin_host::Liveness`) with
    /// recovery off: its node was removed. Reported once. With recovery on
    /// the death is `PluginReloading`, and the node's removal
    /// `PluginGaveUp`.
    #[cfg(feature = "plugin")]
    PluginDied {
        /// The node it was at, now removed.
        node: NodeKey,
        /// The engine's latched reason.
        cause: String,
    },
    /// A hosted plugin's declared latency moved, and the graph took it: the
    /// next commit compensates it.
    #[cfg(feature = "plugin")]
    PluginLatencyChanged {
        /// The plugin's node.
        node: NodeKey,
        /// Its latency now.
        latency: Samples,
    },
    /// A healthy plugin's state is due a snapshot: run the job off the
    /// control thread and land it with
    /// [`Session::land_snapshot`](crate::Session::land_snapshot).
    #[cfg(feature = "plugin")]
    PluginSnapshotDue(crate::PluginSnapshot),
    /// A parameter mirror rebuilt by a crossfade to another plugin is due
    /// its read: run the job off the control thread and land it with
    /// [`Session::land_params`](crate::Session::land_params). Until then the
    /// mirror is not ready.
    #[cfg(all(feature = "plugin", feature = "modulation"))]
    PluginParamsDue(crate::PluginParamRead),
    /// A hosted plugin died or hung and recovery is on
    /// ([`RecoveryPolicy`](crate::RecoveryPolicy)): its node stays in the
    /// graph, silent, and a replacement loads after `delay`. Reported once
    /// per attempt.
    #[cfg(feature = "plugin")]
    PluginReloading {
        /// The plugin's node, kept.
        node: NodeKey,
        /// Why it needs a replacement: the engine's latched cause, or a
        /// failed attempt's reason.
        cause: String,
        /// Failures within the budget's window so far, this one included.
        attempt: u32,
        /// The wait before the load is handed out.
        delay: std::time::Duration,
    },
    /// A replacement plugin is due its load: run the job off the control
    /// thread and land it with
    /// [`Session::land_reload`](crate::Session::land_reload). A job dropped
    /// unrun counts as a failed attempt.
    #[cfg(feature = "plugin")]
    PluginReloadDue(crate::PluginReload),
    /// A replacement is in the graph at the same node, with the same wiring.
    #[cfg(feature = "plugin")]
    PluginRecovered {
        /// The plugin's node.
        node: NodeKey,
        /// Whether its state came back, and from how long before.
        state: crate::StateRestore,
        /// The node's latency before the failure and now. PDC follows at
        /// the next commit.
        latency: (Samples, Samples),
        /// Attempts it took.
        attempts: u32,
    },
    /// Recovery stopped: the failures spent the budget. The node is removed.
    #[cfg(feature = "plugin")]
    PluginGaveUp {
        /// The node it was at, now removed.
        node: NodeKey,
        /// Why.
        why: crate::GiveUp,
        /// The last state worth keeping, for a host that offers the user a
        /// manual reload: the snapshot a reload withheld, else the last one
        /// captured.
        snapshot: Option<Vec<u8>>,
    },
}

impl std::fmt::Debug for SessionEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
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
            Self::DeviceFault(fault) => f.debug_tuple("DeviceFault").field(fault).finish(),
            #[cfg(feature = "device")]
            Self::DeviceLost { direction, id } => f
                .debug_struct("DeviceLost")
                .field("direction", direction)
                .field("id", id)
                .finish(),
            #[cfg(feature = "device")]
            Self::DeviceAppeared { direction, id } => f
                .debug_struct("DeviceAppeared")
                .field("direction", direction)
                .field("id", id)
                .finish(),
            #[cfg(feature = "device")]
            Self::DefaultChanged { direction, id } => f
                .debug_struct("DefaultChanged")
                .field("direction", direction)
                .field("id", id)
                .finish(),
            #[cfg(feature = "device")]
            Self::RestartWanted(request) => f.debug_tuple("RestartWanted").field(request).finish(),
            #[cfg(feature = "device")]
            Self::DeviceRestarted(r) => f.debug_tuple("DeviceRestarted").field(r).finish(),
            #[cfg(feature = "device")]
            Self::RestartFailed(why) => f.debug_tuple("RestartFailed").field(why).finish(),
            #[cfg(feature = "modulation")]
            Self::ModulationRefused(refusal) => {
                f.debug_tuple("ModulationRefused").field(refusal).finish()
            }
            #[cfg(feature = "modulation")]
            Self::ModulationUnresolved { target, waited } => f
                .debug_struct("ModulationUnresolved")
                .field("target", target)
                .field("waited", waited)
                .finish(),
            #[cfg(feature = "midi")]
            Self::MidiOutDropped(count) => f.debug_tuple("MidiOutDropped").field(count).finish(),
            #[cfg(feature = "midi-hardware")]
            Self::MidiInputDropped(count) => {
                f.debug_tuple("MidiInputDropped").field(count).finish()
            }
            #[cfg(feature = "midi-hardware")]
            Self::MidiDevice(event) => f.debug_tuple("MidiDevice").field(event).finish(),
            #[cfg(feature = "midi-hardware")]
            Self::MidiInputLost(device) => f.debug_tuple("MidiInputLost").field(device).finish(),
            #[cfg(feature = "midi-hardware")]
            Self::MidiStream { device, stream } => f
                .debug_struct("MidiStream")
                .field("device", device)
                .field("stream", stream)
                .finish(),
            #[cfg(feature = "midi-hardware")]
            Self::MidiWatchFailed(why) => f.debug_tuple("MidiWatchFailed").field(why).finish(),
            #[cfg(feature = "midi-hardware")]
            Self::MidiOutputLost(device) => f.debug_tuple("MidiOutputLost").field(device).finish(),
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
            #[cfg(feature = "device")]
            Self::OutputsListed(ids) => f.debug_tuple("OutputsListed").field(ids).finish(),
            #[cfg(feature = "device")]
            Self::OutputsListFailed(why) => f.debug_tuple("OutputsListFailed").field(why).finish(),
            #[cfg(feature = "device")]
            Self::OutputsListSlow(waited) => {
                f.debug_tuple("OutputsListSlow").field(waited).finish()
            }
            Self::BlocksSilenced(count) => f.debug_tuple("BlocksSilenced").field(count).finish(),
            Self::EventsDroppedElsewhere(count) => f
                .debug_tuple("EventsDroppedElsewhere")
                .field(count)
                .finish(),
            #[cfg(feature = "plugin")]
            Self::PluginDied { node, cause } => f
                .debug_struct("PluginDied")
                .field("node", node)
                .field("cause", cause)
                .finish(),
            #[cfg(feature = "plugin")]
            Self::PluginLatencyChanged { node, latency } => f
                .debug_struct("PluginLatencyChanged")
                .field("node", node)
                .field("latency", latency)
                .finish(),
            #[cfg(feature = "plugin")]
            Self::PluginSnapshotDue(job) => f.debug_tuple("PluginSnapshotDue").field(job).finish(),
            #[cfg(all(feature = "plugin", feature = "modulation"))]
            Self::PluginParamsDue(job) => f.debug_tuple("PluginParamsDue").field(job).finish(),
            #[cfg(feature = "plugin")]
            Self::PluginReloading {
                node,
                cause,
                attempt,
                delay,
            } => f
                .debug_struct("PluginReloading")
                .field("node", node)
                .field("cause", cause)
                .field("attempt", attempt)
                .field("delay", delay)
                .finish(),
            #[cfg(feature = "plugin")]
            Self::PluginReloadDue(job) => f.debug_tuple("PluginReloadDue").field(job).finish(),
            #[cfg(feature = "plugin")]
            Self::PluginRecovered {
                node,
                state,
                latency,
                attempts,
            } => f
                .debug_struct("PluginRecovered")
                .field("node", node)
                .field("state", state)
                .field("latency", latency)
                .field("attempts", attempts)
                .finish(),
            #[cfg(feature = "plugin")]
            Self::PluginGaveUp {
                node,
                why,
                snapshot,
            } => f
                .debug_struct("PluginGaveUp")
                .field("node", node)
                .field("why", why)
                .field("snapshot_bytes", &snapshot.as_ref().map(Vec::len))
                .finish(),
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
    pub input_devices: usize,
    /// Its controls (the MPE mode).
    pub input_controls: tutti_midi_runtime::MidiIngestControls,
    /// The clock's node.
    pub clock: NodeKey,
    /// The clock master: enable it, pick MTC and its frame rate.
    pub clock_master: Arc<tutti_midi_runtime::ClockMaster>,
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

/// The tutti engine, Bevy-free: a [`Graph`], its transport, and the engine
/// side it renders on, in the order [`update`](Self::update) runs them.
///
/// Built with [`SessionBuilder`](crate::SessionBuilder): offline (the host
/// pulls blocks with [`render`](Self::render)), on a device (with a
/// `Device`, feature `device`), or detached (the executor stays on
/// this side: [`render_frame`](Self::render_frame)).
///
/// `Send`, not `Sync`: one control thread owns it. With `plugin` on, every
/// path that frees a retired unit checks the main thread (a `debug_assert!`
/// on `tutti_plugin_types::MainThread::claimed_elsewhere`, silent until a host
/// claims its main thread), because what it frees may be an in-process plugin
/// whose `Drop` tears down an editor. These paths take no `MainThread` token (a
/// headless session has no main thread to be handed); the entries that open an
/// in-process plugin do (`add_plugin`, `land_reload`). A unit is freed only
/// when the executor hands it back, at the editor's collect, so the check is
/// there: the [`Collect`](Phase::Collect) phase, a commit (on a detached
/// session, which applies and collects at once: [`commit`](Self::commit),
/// [`render_frame`](Self::render_frame), [`Graph::set_sample_rate`]), and
/// dropping the session. [`update`](Self::update) and the
/// [`Commit`](Phase::Commit) and [`AfterCommit`](Phase::AfterCommit) phases
/// check it too. Editing the graph (an insert, [`remove`](Self::remove), a
/// wire) frees nothing and may run on any thread that owns the session.
///
/// ```
/// use tutti_types::MaxBlock;
/// use tutti_types::{ChannelLayout, InterleavedMut, SampleRate, Samples};
/// use tutti_session::{GraphSource, Session};
/// use tutti_nodes::testing::Const;
///
/// // One global output: the mono device takes it as it is, unfolded.
/// let mut session = Session::builder()
///     .outputs_floor(1)
///     .build_offline(SampleRate(48_000.0), ChannelLayout::MONO, MaxBlock::of(Samples(64)))
///     .unwrap();
/// let (dc, _) = session.graph().insert(Const::mono(0.5));
/// session.graph().set_output_source(0, GraphSource::Node(dc, 0));
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
    /// The device's fault record, shared with the [`Device`](crate::Device),
    /// and the count the last poll saw.
    #[cfg(feature = "device")]
    pub(crate) faults: Option<(Arc<tutti_cpal::StreamFaults>, u64)>,
    /// The device watch (design 018), and what restarts reported since the
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
    /// A main-thread call (see the type docs).
    pub fn update(&mut self) -> Vec<SessionEvent> {
        crate::main_thread();
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
                crate::main_thread();
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
                crate::main_thread();
                let sent = self.graph.is_edited() && self.commit() == Committed::Sent;
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
                crate::main_thread();
                #[cfg(feature = "modulation")]
                {
                    // The re-apply's commit, as the update's own: published
                    // on `Sent` (what `commit_and_publish` did in Bevy).
                    if self.mods.reapply(&mut self.graph, cx.mods, cx.now, out) {
                        self.commit();
                    }
                    self.mods.resync(&mut self.graph);
                }
            }
            Phase::MidiOut => {
                #[cfg(feature = "midi-hardware")]
                self.midi_state.follow_output_config(self.midi_io.as_ref());
                #[cfg(feature = "midi")]
                self.midi_state.midi_out_phase(&mut self.graph, cx.now, out)
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
                    self.midi_io.as_ref(),
                    self.midi.as_ref(),
                    &mut self.graph,
                    cx.now,
                    out,
                ) {
                    self.commit();
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
    /// session's (`midi`, feature `midi`), that. A clip or live input
    /// feeding `node` stays, to be re-aimed (`Midi::retarget`) or removed.
    pub fn remove(&mut self, node: NodeKey) -> bool {
        #[cfg(feature = "modulation")]
        self.mods.own.forget_node(node);
        #[cfg(feature = "midi")]
        self.midi_state.forget_node(node);
        self.graph.remove(node)
    }

    /// The session's own modulation declarations, to edit (design 015
    /// §4.3, §5.1): the next update applies them. A session has one
    /// declarer: while these hold anything, a host's
    /// [`ModInput`] is refused.
    #[cfg(feature = "modulation")]
    pub fn mods(&mut self) -> crate::Mods<'_> {
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
    /// ([`Graph::take_audio_side`]).
    pub fn render_frame(&mut self, output: &mut [f32]) {
        self.render_frame_at(&tutti_graph::Transport::default(), output);
    }

    /// [`render_frame`](Self::render_frame) with the transport at
    /// `transport`: what plays a clip in a graph driven by hand.
    pub fn render_frame_at(&mut self, transport: &tutti_graph::Transport, output: &mut [f32]) {
        if self.graph.is_edited() {
            self.commit();
        }
        self.graph.render_local(transport, output);
    }

    /// The transport: its rate follows the device's.
    pub fn transport(&self) -> &Transport {
        &self.transport
    }

    /// Starts the transport rolling from where it stands, at the next block.
    /// A no-op while rolling.
    ///
    /// # Errors
    ///
    /// [`QueueFull`] when the transport's command queue is full; the event
    /// is handed back in it.
    pub fn play(&self) -> Result<(), QueueFull> {
        self.transport.motion().try_send(MotionEvent::Play)
    }

    /// Stops the transport where it stands, at the next block, after the
    /// [`FadeOut`] it names.
    ///
    /// # Errors
    ///
    /// [`QueueFull`], as [`play`](Self::play).
    pub fn stop(&self, fade: FadeOut) -> Result<(), QueueFull> {
        self.transport.motion().try_send(MotionEvent::Stop { fade })
    }

    /// Jumps to `beat` at the next block, keeping whatever the transport was
    /// doing (a locate while rolling keeps rolling), with a declick fade.
    ///
    /// # Errors
    ///
    /// [`QueueFull`], as [`play`](Self::play).
    pub fn locate(&self, beat: Beat) -> Result<(), QueueFull> {
        self.transport.motion().try_send(MotionEvent::locate(beat))
    }

    /// Sends a motion `event` to take effect at `at`: a frame, a beat, or the
    /// next block. The one timed verb: `schedule(At::Beat(b), MotionEvent::Play)`
    /// is play at beat `b`, so the session needs one method for the timed
    /// form where it would need three (`play_at`, `stop_at`, `locate_at`), and
    /// every [`MotionEvent`] (a locate that then rolls, a stop that returns
    /// to a beat) is timed the same way. The untimed verbs are `At::NextBlock`.
    ///
    /// # Errors
    ///
    /// [`ScheduleFull`] when the timed-command list is full.
    pub fn schedule(&self, at: At, event: MotionEvent) -> Result<(), ScheduleFull> {
        self.transport.motion().schedule(at, event)
    }

    /// Sets a constant project tempo (the project's [`TempoMap`], one step).
    /// The engine takes it at the next block. A tempo that is not finite and
    /// positive leaves the map as it was.
    pub fn set_tempo(&self, bpm: Bpm) {
        self.transport.settings().set_tempo(bpm);
    }

    /// Publishes `map` as the project's tempo (steps and ramps): the engine
    /// takes it at the next block. A map by value works: `map.into()`.
    pub fn set_tempo_map(&self, map: impl Into<Arc<TempoMap>>) {
        self.transport.settings().set_tempo_map(map);
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
    /// [`TapReader`] as a plan tap, and is the default way to record what the
    /// output sounds like. A transport stop fades in it as it does in the
    /// engine's output, so a take has no click there; [`Graph::open_output_tap`] is the
    /// pre-fader signal of the graph, aligned by the compiler, and steps at a
    /// stop for a transport-gated source.
    ///
    /// It is written by the engine, on every path that renders through it: a
    /// device callback, [`render`](Self::render), and a block the engine
    /// refuses (silence, whole frames, so a take keeps its timing). An export
    /// forks the graph and renders without the engine, so it never writes to
    /// it. There is one at a time; its ring holds `capacity` frames and what
    /// the reader does not drain in time is dropped and counted
    /// ([`TapReader::take_lost`]). It flows from the first
    /// render that begins after this returns, and is closed by
    /// [`close_heard_tap`](Self::close_heard_tap) or, once its reader is
    /// dropped, by the next [`Reclaim`](Phase::Reclaim) phase. The width is
    /// the device's when the session was built: a restart onto a device of
    /// another width counts the tap's blocks lost until it is opened again.
    ///
    /// # Errors
    ///
    /// [`HeardTapError::Detached`] on a session without an engine,
    /// [`HeardTapError::Busy`] while another reader is alive, and
    /// [`HeardTapError::Backpressure`] while the audio thread has not taken
    /// the last installs.
    pub fn open_heard_tap(&self, capacity: Samples) -> Result<TapReader, HeardTapError> {
        let (taps, layout) = self.heard.as_ref().ok_or(HeardTapError::Detached)?;
        taps.open(*layout, capacity).map_err(|e| match e {
            tutti_engine::HeardTapError::Backpressure => HeardTapError::Backpressure,
            _ => HeardTapError::Busy,
        })
    }

    /// Closes the tap [`open_heard_tap`](Self::open_heard_tap) opened. The
    /// reader keeps what it was written.
    pub fn close_heard_tap(&self) {
        if let Some((taps, _)) = &self.heard {
            taps.close();
        }
    }

    /// The master meter: the level of each graph output channel, one
    /// [`Level`](tutti_graph::Level) per channel, with the peak held since
    /// the last read. It is an op of the plan on the outputs (see
    /// [`Graph::master_meter`](crate::Graph::master_meter)), declared and on
    /// from the build, so every render feeds it: a device callback,
    /// [`render`](Self::render), and a detached session's
    /// [`render_frame`](Self::render_frame). It reads before the engine's fold
    /// to the device and its declick, aligned to the outputs' time. A block
    /// the engine refuses (rendered silent because the control side held the
    /// graph for that callback) runs no op, so it does not touch the meter:
    /// the meter holds what it held and counts no frames for it. `None` once
    /// a host has taken it.
    pub fn master_meter(&mut self) -> Option<&mut tutti_graph::MeterReader> {
        self.graph.master_meter()
    }

    /// Hands the master meter's reader to its one consumer; see
    /// [`master_meter`](Self::master_meter) and
    /// [`Graph::take_master_meter`](crate::Graph::take_master_meter).
    pub fn take_master_meter(&mut self) -> Option<tutti_graph::MeterReader> {
        self.graph.take_master_meter()
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

    /// The engine's MIDI nodes, when the builder inserted them.
    #[cfg(feature = "midi")]
    pub fn midi_nodes(&self) -> Option<&MidiNodes> {
        self.midi.as_ref()
    }

    /// The OS MIDI layer, when the builder opened it
    /// ([`SessionBuilder::midi_hardware`](crate::SessionBuilder::midi_hardware)).
    #[cfg(feature = "midi-hardware")]
    pub fn midi_io(&self) -> Option<&tutti_midi_hardware::MidiSession> {
        self.midi_io.as_ref()
    }

    /// Takes the disk streamer the builder made, for a host that holds it
    /// itself: a restart then re-rates it only through the host's `follow`
    /// hook.
    #[cfg(feature = "sampler")]
    pub fn take_disk_streamer(&mut self) -> Option<tutti_sampler::DiskStreamer> {
        self.disk_streamer.take()
    }

    /// The disk streamer the builder made, while the session holds it.
    #[cfg(feature = "sampler")]
    pub fn disk_streamer(&self) -> Option<&tutti_sampler::DiskStreamer> {
        self.disk_streamer.as_ref()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        crate::main_thread();
    }
}
