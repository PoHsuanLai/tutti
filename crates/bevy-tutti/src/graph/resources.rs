//! The audio graph's own ECS resources: the device config and the editable
//! DSP graph. Each other subsystem keeps its `*Res` beside its own module.
//!
//! Both are inserted by [`build_into`](crate::engine::build_into) once the
//! device is open and the session is built.

use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;

use crate::graph::AudioNode;
use tutti_session::{Graph, Phase, PhaseCx, Session, SessionEvent};
use tutti_types::Samples;
use tutti_types::{ChannelLayout, SampleRate};

use parking_lot::{ReentrantMutex, ReentrantMutexGuard};
#[cfg(test)]
use std::sync::{Mutex, PoisonError};

/// Audio device configuration captured at engine build time.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Reflect)]
#[reflect(Resource, Clone)]
pub struct AudioConfig {
    /// The rate the open device is running at. Everything time-denominated in
    /// the graph is derived from it, so a node built against a stale one renders
    /// at the wrong speed.
    // Not reflected: `SampleRate` derives `Reflect` only under
    // `tutti-types/bevy`, which this crate enables solely via `modulation`.
    // A reflected config reads 0.0 Hz, "not configured".
    #[reflect(ignore)]
    pub sample_rate: SampleRate,
    /// The device's output width. The graph root is at least this wide (the
    /// build and a device restart widen it to a wider device, and never narrow
    /// it; the engine folds to a narrower one), and a wider declaration widens
    /// it further — see `MasterSources`.
    // Not reflected, for the same reason as `sample_rate`. A reflected config
    // reads zero channels (`ChannelLayout` has no `Default`), "not configured".
    #[reflect(ignore, default = "unconfigured_channels")]
    pub channels: ChannelLayout,
}

/// The width a reflected [`AudioConfig`] carries before a device fills it in.
fn unconfigured_channels() -> ChannelLayout {
    ChannelLayout::EMPTY
}

/// The editable DSP graph, as a Bevy resource: a `tutti_session::Session`.
///
/// Edit through [`graph`](Self::graph), the session's [`Graph`], and read
/// through [`graph_ref`](Self::graph_ref); the methods here are the ones that
/// are more than the graph's own. Every edit marks the session edited, and the
/// per-frame [`commit_graph`](crate::graph::commit_graph) sends every edit of
/// the frame to the audio thread at once (one commit per frame at most, and
/// none on a frame with no edit). Nothing here commits inline.
///
/// Behind it is the session's graph: a `tutti-graph`
/// [`Editor`](tutti_graph::Editor) on this side and its executor on the audio
/// thread. Every node goes in through its own
/// [`IntoNode`](tutti_graph::IntoNode), and latency compensation (PDC) is the
/// graph compiler's: nothing is spliced into the graph to align it.
///
/// Inserted by [`build_into`](crate::engine::build_into) once the device is
/// open; [`headless`](Self::headless) builds one with no device, for tests and
/// offline tools. Every method runs on the control side (the main thread or a
/// system), never on the audio thread; they may allocate.
///
/// Most apps do not call these methods directly: spawn nodes with
/// [`SpawnAudioNode`](crate::graph::SpawnAudioNode) and declare wiring with
/// [`PortSources`](crate::graph::PortSources) and
/// [`MasterSources`](crate::graph::MasterSources), and the reconcile systems
/// make the calls.
///
/// # Examples
///
/// ```
/// use bevy_tutti::prelude::*;
/// use tutti_nodes::testing::Const;
///
/// let mut graph = AudioGraphRes::headless(0, 1);
/// let (node, _controls) = graph.graph().insert(Const::mono(0.5));
/// graph.graph().set_output_source(0, GraphSource::Node(node, 0));
///
/// // A headless graph renders on the calling thread.
/// let mut out = [0.0f32];
/// graph.render_frame(&mut out);
/// assert_eq!(out[0], 0.5);
/// ```
///
/// The session is not reachable from outside this crate:
///
/// ```compile_fail,E0616
/// use bevy_tutti::graph::AudioGraphRes;
///
/// let graph = AudioGraphRes::headless(0, 2);
/// let _session = graph.0;
/// ```
// Mutation: `pub struct AudioGraphRes(pub ReentrantMutex<Session>)` makes the doctest
// above compile, which fails it. Everything else in it compiles as written,
// so the privacy of the field is the only thing it can be failing on.
#[derive(Resource)]
// Behind a lock only because a `Resource` must be `Sync` and the session is
// `Send` but not `Sync` (it holds boxed nodes and ring ends). Every `&mut
// self` method reaches it with `get_mut`, lock-free; a `&self` query takes
// the lock, uncontended — the resource's own borrow already serializes
// access. Re-entrant, so a statement that reads the graph twice does not
// deadlock; it hands out only `&Session`.
#[derive(Debug)]
pub struct AudioGraphRes(
    ReentrantMutex<Session>,
    // Test builds: every phase run through `run_phase{,_with_mods}`, in order,
    // for the schedule test (`schedule::tests`).
    #[cfg(test)] Mutex<Vec<Phase>>,
);

/// The session's [`Graph`], read through [`AudioGraphRes::graph_ref`].
///
/// Holds the resource's lock until dropped. The lock is re-entrant, so a
/// second guard on the same thread is fine.
#[derive(Debug)]
pub struct GraphRef<'a>(ReentrantMutexGuard<'a, Session>);

impl std::ops::Deref for GraphRef<'_> {
    type Target = Graph;

    fn deref(&self) -> &Graph {
        self.0.graph_ref()
    }
}

impl AudioGraphRes {
    /// `&self` access. A panic mid-edit leaves a spec the next commit
    /// validates, never a torn audio thread.
    pub(crate) fn read(&self) -> ReentrantMutexGuard<'_, Session> {
        self.0.lock()
    }

    /// `&mut self` access: no lock needed.
    fn write(&mut self) -> &mut Session {
        self.0.get_mut()
    }

    /// The session's graph, to edit: every node, edge, feed, param and
    /// crossfade (see [`Graph`]). An edit marks it edited, and the frame's
    /// commit sends it. [`remove`](Self::remove) is the session's, which also
    /// forgets the session's own declarations on the node.
    pub fn graph(&mut self) -> &mut Graph {
        self.write().graph()
    }

    /// The session's graph, to read, behind a guard that holds the
    /// resource's lock (see [`GraphRef`]).
    pub fn graph_ref(&self) -> GraphRef<'_> {
        GraphRef(self.read())
    }

    /// While held, the device policy asks for no restart
    /// ([`TuttiPlugin::watch_devices`](crate::TuttiPlugin::watch_devices)):
    /// hold it while a take is recording, because a restart can change the
    /// rate under it. The watch still reports devices.
    pub fn hold_device_restarts(&mut self, held: bool) {
        self.session().hold_device_restarts(held);
    }

    /// The session, for this crate's engine and restart paths.
    pub(crate) fn session(&mut self) -> &mut Session {
        self.write()
    }

    /// The session's MIDI ([`tutti_session::Midi`]): the hardware output
    /// a host connects (`set_hardware_out`), its drop count, and a host's
    /// own outbound mailbox drained through it (`drain_out`). Clips, live
    /// inputs and the hardware input's routes are declared in the ECS
    /// ([`MidiSourceInstall`](crate::midi::MidiSourceInstall),
    /// [`LiveMidiInput`](crate::midi::LiveMidiInput),
    /// [`MidiRouteRule`](crate::midi::MidiRouteRule)), whose systems call
    /// this; what a host edits here directly, they do not see.
    #[cfg(feature = "midi")]
    pub fn midi(&mut self) -> tutti_session::Midi<'_> {
        self.write().midi()
    }

    /// The resource over a built `session` ([`build_into`](crate::engine::build_into)).
    pub(crate) fn from_session(session: Session) -> Self {
        Self(
            ReentrantMutex::new(session),
            #[cfg(test)]
            Mutex::default(),
        )
    }

    /// A graph with no device behind it: `inputs` global inputs and `outputs`
    /// global outputs, prepared at 44.1 kHz until
    /// [`Graph::set_sample_rate`]
    /// (`SessionBuilder::build_detached`).
    ///
    /// For tests and headless tools that drive the reconcile pipeline without
    /// opening a device. The executor stays on this side until
    /// [`Graph::take_audio_side`] takes it: while it is here,
    /// every commit is applied at once, and
    /// [`render_frame`](Self::render_frame) renders it.
    ///
    /// A [`Graph::set_param`] lands on the next rendered block, on
    /// this graph as on an engine's: there is no control-side copy of a node
    /// for it to land on at once.
    #[allow(
        clippy::missing_panics_doc,
        reason = "`build_detached` has no failing path"
    )]
    pub fn headless(inputs: usize, outputs: usize) -> Self {
        Self::from_session(
            Session::builder()
                .inputs(inputs)
                .outputs_floor(outputs)
                .build_detached(SampleRate::DEFAULT)
                .expect("a detached session builds nothing that can fail"),
        )
    }

    /// The engine's DSP-load meter ([`Session::load_meter`]); `None` on a
    /// [`headless`](Self::headless) graph, which has no engine.
    pub fn load_meter(&self) -> Option<std::sync::Arc<tutti_core::LoadMeter>> {
        self.read().load_meter().cloned()
    }

    /// `f` over what the session keeps for the plugin hosted at `node`
    /// (`tutti_session::HostedPlugin`: its handle, controls, liveness, last
    /// snapshot and, with `modulation`, its parameter mirror); `None` when
    /// no plugin is hosted there.
    #[cfg(feature = "plugin")]
    pub fn with_plugin<R>(
        &self,
        node: AudioNode,
        f: impl FnOnce(&tutti_session::HostedPlugin) -> R,
    ) -> Option<R> {
        self.read().plugin(node.0).map(f)
    }

    /// The render mode last announced to the hosted plugins: `Offline`
    /// while an export is in flight, else `Realtime` (the session's
    /// `Phase::RenderMode`).
    #[cfg(feature = "plugin")]
    pub fn render_mode(&self) -> tutti_plugin::RenderMode {
        self.read().render_mode()
    }

    /// Takes `node` out of the graph. Every edge to and from it reads silence
    /// afterwards, so a sink still naming it is left silent rather than
    /// dangling.
    ///
    /// Returns whether it was there; removing a node twice is a no-op.
    pub fn remove(&mut self, node: AudioNode) -> bool {
        self.write().remove(node.0)
    }

    // --- Latency ---

    /// The session's published per-channel pre-roll, republished with every
    /// plan: what [`ChannelCompensation`](crate::ChannelCompensation) holds.
    pub(crate) fn compensation(&self) -> std::sync::Arc<tutti_graph::PrerollTable> {
        std::sync::Arc::clone(self.read().compensation())
    }

    /// The total latency of the plan sent last, as the session published it:
    /// what [`GraphLatency`](crate::GraphLatency) mirrors.
    pub(crate) fn latency(&self) -> Samples {
        self.read().latency()
    }

    /// The phases run since the last call, in the order they ran.
    #[cfg(test)]
    pub(crate) fn take_phase_log(&mut self) -> Vec<Phase> {
        std::mem::take(self.1.get_mut().unwrap_or_else(PoisonError::into_inner))
    }

    // --- Publishing and rendering ---

    /// Runs one of the session's phases (`Session::run_phase`) at `now`,
    /// returning what it found.
    pub(crate) fn run_phase(&mut self, phase: Phase, now: std::time::Instant) -> Vec<SessionEvent> {
        #[cfg(test)]
        self.1
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .push(phase);
        let mut out = Vec::new();
        self.write().run_phase(phase, PhaseCx::at(now), &mut out);
        out
    }

    /// [`run_phase`](Self::run_phase) with the app's modulation
    /// declarations (`PhaseCx::with_mods`): what the modulation systems run.
    #[cfg(feature = "modulation")]
    pub(crate) fn run_phase_with_mods(
        &mut self,
        phase: Phase,
        now: std::time::Instant,
        mods: tutti_session::ModInput<'_>,
    ) -> Vec<SessionEvent> {
        #[cfg(test)]
        self.1
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .push(phase);
        let mut out = Vec::new();
        self.write()
            .run_phase(phase, PhaseCx::at(now).with_mods(mods), &mut out);
        out
    }

    /// Renders one frame on the calling thread: no inputs, `output` sized to
    /// [`Graph::outputs`].
    ///
    /// For a [`headless`](Self::headless) graph, which has no audio thread to
    /// hear it: it renders the executor while this side holds it, committing
    /// any edit first, under a stopped transport.
    ///
    /// # Panics
    ///
    /// Once the audio side was taken — by
    /// [`Graph::take_audio_side`] or the engine: there is no
    /// control-side copy of any node to render instead.
    pub fn render_frame(&mut self, output: &mut [f32]) {
        self.write().render_frame(output);
    }

    /// [`render_frame`](Self::render_frame) with the transport at
    /// `transport`: what plays a clip in a graph driven by hand (a test, a
    /// tool), where no engine clock runs.
    pub fn render_frame_at(&mut self, transport: &tutti_graph::Transport, output: &mut [f32]) {
        self.write().render_frame_at(transport, output);
    }
}
