//! [`Session`]: the graph, its transport and engine side, and the order they
//! run in ([`PHASES`]).

use std::any::Any;
use std::sync::Arc;
use std::time::Instant;

use tutti_core::{AudioNode, AudioTap, ClickSettings, MasterMeter, RtPublish, Samples, Transport};

use crate::graph::{Committed, CrossfadeId, Graph, Retried};

/// One step of [`Session::update`]. [`PHASES`] is their order.
///
/// A host that runs the steps itself (a Bevy app runs each in its own
/// system) calls [`Session::run_phase`] once per phase, in this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Phase {
    /// Crossfades that waited out a re-prepare are retried, then
    /// `Editor::collect` frees what the audio thread retired (here, on the
    /// main thread) and, mid re-prepare, sends the resumed plan.
    Collect,
    /// Hosted plugins' health, latency and parameter edits. Empty until the
    /// session owns the plugins (design 015 PR 6).
    Plugins,
    /// The plugins' render mode (realtime or offline). Empty until the
    /// session owns exports (design 015 PR 6).
    RenderMode,
    /// Modulation's collect and apply. Empty until the session owns the
    /// modulation loop (design 015 PR 5): a host applies its declarations
    /// with [`Graph::modulate`] until then.
    Modulation,
    /// The commit, iff the graph was edited since the last one, and the
    /// compensation of the plan it sent (or of a re-prepare's resumed plan)
    /// published.
    Commit,
    /// Modulation's re-apply and resyncs. Empty until design 015 PR 5.
    AfterCommit,
    /// Hardware MIDI out and the clock. Empty until design 015 PR 5.
    MidiOut,
    /// The device's faults, as [`SessionEvent::DeviceFault`].
    Poll,
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
];

/// What one phase reads besides the session.
#[derive(Clone, Copy)]
pub struct PhaseCx<'a> {
    /// The time thresholds are measured on (a test passes its own).
    pub now: Instant,
    /// The host's modulation declarations and the resolver for its keys;
    /// `None`: the session's own.
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
    /// The declarations, as `tutti_mod` takes them.
    pub declarations: tutti_mod::Declarations<'a>,
    /// The host's keys, resolved.
    pub keys: &'a dyn crate::ModKeys,
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
        node: AudioNode,
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
        node: AudioNode,
        /// The id [`Graph::crossfade`] returned.
        id: CrossfadeId,
        /// Why.
        why: String,
    },
    /// The audio backend reported a fault (a device unplugged mid-session):
    /// the most recent one since the last poll.
    #[cfg(feature = "device")]
    DeviceFault(tutti_cpal::StreamFault),
}

impl std::fmt::Debug for SessionEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CrossfadeLanded { node, id, .. } => f
                .debug_struct("CrossfadeLanded")
                .field("node", node)
                .field("id", id)
                .finish_non_exhaustive(),
            Self::CrossfadeFailed { node, id, why } => f
                .debug_struct("CrossfadeFailed")
                .field("node", node)
                .field("id", id)
                .field("why", why)
                .finish(),
            #[cfg(feature = "device")]
            Self::DeviceFault(fault) => f.debug_tuple("DeviceFault").field(fault).finish(),
        }
    }
}

/// The engine's MIDI, as graph nodes the builder inserts: the hardware
/// input, the clock (Beat Clock and MTC, off until a host enables it) and the
/// hardware out the clock feeds, with their controls.
#[cfg(feature = "midi")]
#[derive(Clone)]
pub struct MidiNodes {
    /// The hardware input's node: its ports feed what a route rule names.
    pub input: AudioNode,
    /// Its controls (the MPE mode).
    pub input_controls: tutti_midi_runtime::MidiInputControls,
    /// The clock's node.
    pub clock: AudioNode,
    /// The clock master: enable it, pick MTC and its frame rate.
    pub clock_master: Arc<tutti_midi_runtime::ClockMaster>,
    /// The hardware out's node, fed by the clock (the `"clock"` feed).
    pub hardware_out: AudioNode,
    /// What a host drains to the OS.
    pub out_controls: tutti_midi_runtime::MidiOutControls,
}

/// The tutti engine, Bevy-free: a [`Graph`], its transport, and the engine
/// side it renders on, in the order [`update`](Self::update) runs them.
///
/// Built with [`SessionBuilder`](crate::SessionBuilder): offline (the host
/// pulls blocks with [`render`](Self::render)), on a device (with a
/// [`Device`](crate::Device), `device`), or detached (the executor stays on
/// this side: [`render_frame`](Self::render_frame)).
///
/// `Send`, not `Sync`: one control thread owns it. With `plugin` on,
/// [`update`](Self::update), the [`Collect`](Phase::Collect),
/// [`Commit`](Phase::Commit) and [`AfterCommit`](Phase::AfterCommit)
/// phases, [`Graph::remove`] through [`remove`](Self::remove), and dropping
/// the session assert the main thread (`tutti_plugin_types::assert_main_thread`,
/// a no-op until a host marks one): they free what an in-process plugin's
/// `Drop` tears down.
///
/// ```
/// use tutti_core::{ChannelLayout, InterleavedMut, SampleRate, Samples};
/// use tutti_session::{GraphSource, Session};
/// use tutti_nodes::testing::Const;
///
/// // One global output: the mono device takes it as it is, unfolded.
/// let mut session = Session::builder()
///     .outputs_floor(1)
///     .build_offline(SampleRate(48_000.0), ChannelLayout::MONO, Samples(64))
///     .unwrap();
/// let (dc, _) = session.graph().insert(Const::mono(0.5));
/// session.graph().set_output_source(0, GraphSource::Node(dc, 0));
/// session.update();
///
/// let mut block = [0.0f32; 64];
/// session.render(&mut InterleavedMut::new(&mut block, ChannelLayout::MONO));
/// assert!(block.iter().all(|&s| s == 0.5));
/// ```
pub struct Session {
    pub(crate) graph: Graph,
    pub(crate) transport: Transport,
    /// The engine, for an offline session (the host pulls blocks); `None` on
    /// a device (the callback holds it) or detached.
    pub(crate) engine: Option<tutti_core::Engine>,
    pub(crate) meter: MasterMeter,
    pub(crate) tap: AudioTap,
    pub(crate) click: Option<AudioNode>,
    pub(crate) click_settings: Arc<ClickSettings>,
    /// The per-channel pre-roll of the plan sent last, for sources outside
    /// the graph (a disk stream), republished with every plan.
    pub(crate) compensation: Arc<RtPublish<Vec<Samples>>>,
    /// The total latency of the plan sent last.
    pub(crate) latency: Samples,
    /// A re-prepare resumed in this update's collect: its plan's figures
    /// are published at the commit.
    pub(crate) resumed: bool,
    #[cfg(feature = "midi")]
    pub(crate) midi: Option<MidiNodes>,
    #[cfg(feature = "midi-hardware")]
    pub(crate) midi_io: Option<tutti_midi_hardware::MidiSession>,
    #[cfg(feature = "sampler")]
    pub(crate) disk_streamer: Option<tutti_sampler::DiskStreamer>,
    /// The device's fault record, shared with the [`Device`](crate::Device),
    /// and the count the last poll saw.
    #[cfg(feature = "device")]
    pub(crate) faults: Option<(Arc<tutti_cpal::StreamFaults>, u64)>,
}

/// With `plugin`, the main-thread guard of §4.2; otherwise nothing.
#[inline]
fn main_thread() {
    #[cfg(feature = "plugin")]
    tutti_plugin_types::assert_main_thread();
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

    /// Runs every phase in [`PHASES`] order at `Instant::now()` with the
    /// session's own declarations, and returns what it found.
    ///
    /// A main-thread call (see the type docs).
    pub fn update(&mut self) -> Vec<SessionEvent> {
        main_thread();
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
        let _ = cx;
        match phase {
            Phase::Collect => {
                main_thread();
                // Before the collect, as a frame's spawns run before its
                // commit: a crossfade lands on the update that first finds
                // the re-prepare resumed.
                for retried in self.graph.retry_crossfades() {
                    out.push(match retried {
                        Retried::Landed { node, id, controls } => {
                            SessionEvent::CrossfadeLanded { node, id, controls }
                        }
                        Retried::Failed { node, id, why } => {
                            SessionEvent::CrossfadeFailed { node, id, why }
                        }
                    });
                }
                let repreparing = self.graph.is_repreparing();
                self.graph.collect();
                // A re-prepare resumed in that collect: it sent the resumed
                // plan, whose shapes are new (a lookahead is a time, so a
                // rate change moves latencies).
                self.resumed |= repreparing && !self.graph.is_repreparing();
            }
            Phase::Commit => {
                main_thread();
                let sent = self.graph.is_edited() && self.commit() == Committed::Sent;
                if !sent && self.resumed {
                    self.publish_sent();
                }
                self.resumed = false;
            }
            Phase::AfterCommit => main_thread(),
            Phase::Plugins | Phase::RenderMode | Phase::Modulation | Phase::MidiOut => {}
            Phase::Poll =>
            {
                #[cfg(feature = "device")]
                if let Some((faults, seen)) = &mut self.faults {
                    let count = faults.count();
                    if count != *seen {
                        *seen = count;
                        if let Some(fault) = faults.last() {
                            out.push(SessionEvent::DeviceFault(fault));
                        }
                    }
                }
            }
        }
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
        if let Some((channels, total)) = self.graph.sent_compensation() {
            self.latency = total;
            self.compensation.publish(Arc::new(channels));
        }
    }

    /// Takes `node` out of the graph ([`Graph::remove`]), on the main
    /// thread: what it frees on the next collect may be a plugin's editor.
    pub fn remove(&mut self, node: AudioNode) -> bool {
        main_thread();
        self.graph.remove(node)
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
    pub fn render(&self, output: &mut tutti_core::InterleavedMut<'_>) {
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

    /// The per-channel pre-roll of the plan sent last, for a source outside
    /// the graph (a disk stream seeks by it). Republished with every plan;
    /// empty until the first.
    pub fn compensation(&self) -> &Arc<RtPublish<Vec<Samples>>> {
        &self.compensation
    }

    /// The total latency of the plan sent last: what a DAW displays.
    pub fn latency(&self) -> Samples {
        self.latency
    }

    /// The metronome's node (unwired: the host declares where it lands),
    /// when the builder inserted one.
    pub fn click_node(&self) -> Option<AudioNode> {
        self.click
    }

    /// The metronome's settings.
    pub fn click_settings(&self) -> &Arc<ClickSettings> {
        &self.click_settings
    }

    /// The master meter the device callback feeds.
    pub fn meter(&self) -> &MasterMeter {
        &self.meter
    }

    /// The tap the device callback pushes to, closed until opened.
    pub fn tap(&self) -> &AudioTap {
        &self.tap
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

    /// Takes the disk streamer the builder made (subscribed to
    /// [`compensation`](Self::compensation)), for a host that holds it
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
        main_thread();
    }
}
