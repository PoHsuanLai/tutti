//! The audio graph's own ECS resources: the device config and the editable
//! DSP graph. Each other subsystem keeps its `*Res` beside its own module.
//!
//! Both are inserted by [`build_into`](crate::engine::build_into) once the
//! device is open and the session is built.

use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;

use tutti_core::{AudioNode, Compensation, CrossfadeCurve, Samples, Seconds, Tail};
use tutti_session::{
    AudioSide, Crossfade, CrossfadeRefused, EventSource, GraphSource, Phase, PhaseCx,
    ReplaceRefused, Session, SessionEvent, SessionNode,
};
use tutti_types::{ChannelLayout, SampleRate, UnitParam};

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Audio device configuration captured at engine build time.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Reflect)]
#[reflect(Resource, Clone)]
pub struct AudioConfig {
    /// The rate the open device is running at. Everything time-denominated in
    /// the graph is derived from it, so a node built against a stale one renders
    /// at the wrong speed.
    // Not reflected: `SampleRate` derives `Reflect` only under
    // `tutti-types/bevy`, which this crate enables solely via `modulation`.
    // Reflecting it would make an optional feature load-bearing for the default
    // build. Reflect-construction falls back to `SampleRate::default()` (0.0),
    // which is why this is always built by `engine::build`, never reflected
    // into existence.
    #[reflect(ignore)]
    pub sample_rate: SampleRate,
    /// The device's output width. The graph root is at least this wide (the
    /// build and a device restart widen it to a wider device, and never narrow
    /// it; the engine folds to a narrower one), and a wider declaration widens
    /// it further — see `MasterSources`.
    // Not reflected: `ChannelLayout` derives `Reflect` only under
    // `tutti-types/bevy`, the same optional-feature trap as `sample_rate`
    // above. Reflect-construction falls back to `ChannelLayout::EMPTY` — named
    // here because the type deliberately has no `Default` (a derived width is
    // how `Topology` once grew two global inputs nobody declared). Zero
    // channels, not a guessed stereo, for a config that was never built from a
    // device: like the 0.0 rate beside it, it reads as "not configured".
    #[reflect(ignore, default = "unconfigured_channels")]
    pub channels: ChannelLayout,
}

/// The width a reflected [`AudioConfig`] carries before a device fills it in.
fn unconfigured_channels() -> ChannelLayout {
    ChannelLayout::EMPTY
}

/// The editable DSP graph, as a Bevy resource: a `tutti_session::Session`.
///
/// Edit through its methods. Every edit marks the session edited, and the
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
/// # Why opaque
///
/// Every method is named in graph terms (insert a node, set a port's source,
/// replace a node under a fade) and takes an [`AudioNode`] and a
/// [`GraphSource`], never a runtime type. There is no `Deref`: graph mutation
/// is paired with the per-frame commit, and keeping it behind named methods
/// keeps the edit/commit boundary visible at the call site.
///
/// # Examples
///
/// ```
/// use bevy_tutti::prelude::*;
/// use tutti_nodes::testing::Const;
///
/// let mut graph = AudioGraphRes::headless(0, 1);
/// let (node, _controls) = graph.insert(Const::mono(0.5));
/// graph.set_output_source(0, GraphSource::Node(node, 0));
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
// Mutation: `pub struct AudioGraphRes(pub Mutex<Session>)` makes the doctest
// above compile, which fails it. Everything else in it compiles as written,
// so the privacy of the field is the only thing it can be failing on.
#[derive(Resource)]
// Behind a `Mutex` only because a `Resource` must be `Sync` and the session
// is `Send` but not `Sync` (it holds boxed nodes and ring ends). Every
// `&mut self` method reaches it with `get_mut`, lock-free; a `&self` query
// takes the lock, uncontended — the resource's own borrow already serializes
// access.
pub struct AudioGraphRes(
    Mutex<Session>,
    // Test builds: every phase run through `run_phase{,_with_mods}`, in order,
    // for the schedule test (`schedule::tests`).
    #[cfg(test)] Mutex<Vec<Phase>>,
);

impl AudioGraphRes {
    /// `&self` access. Poison is recovered: a panic mid-edit leaves a spec
    /// the next commit validates, never a torn audio thread.
    pub(crate) fn read(&self) -> MutexGuard<'_, Session> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `&mut self` access: no lock needed.
    fn write(&mut self) -> &mut Session {
        self.0.get_mut().unwrap_or_else(PoisonError::into_inner)
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
            Mutex::new(session),
            #[cfg(test)]
            Mutex::default(),
        )
    }

    /// A graph with no device behind it: `inputs` global inputs and `outputs`
    /// global outputs, prepared at 44.1 kHz until
    /// [`set_sample_rate`](Self::set_sample_rate)
    /// (`SessionBuilder::build_detached`).
    ///
    /// For tests and headless tools that drive the reconcile pipeline without
    /// opening a device. The executor stays on this side until
    /// [`take_audio_side`](Self::take_audio_side) takes it: while it is here,
    /// every commit is applied at once, and
    /// [`render_frame`](Self::render_frame) renders it.
    ///
    /// A [`set_param`](Self::set_param) lands on the next rendered block, on
    /// this graph as on an engine's: there is no control-side copy of a node
    /// for it to land on at once.
    pub fn headless(inputs: usize, outputs: usize) -> Self {
        Self::from_session(
            Session::builder()
                .inputs(inputs)
                .outputs_floor(outputs)
                .build_detached(SampleRate::DEFAULT)
                .expect("a detached session builds nothing that can fail"),
        )
    }

    /// What every node is prepared for: the rate, the largest block, and the
    /// device's callback size when the engine knows it.
    pub fn prepared(&self) -> tutti_graph::Prepare {
        self.read().graph_ref().prepared()
    }

    /// The engine's DSP-load meter ([`Session::load_meter`]); `None` on a
    /// [`headless`](Self::headless) graph, which has no engine.
    pub fn load_meter(&self) -> Option<std::sync::Arc<tutti_core::LoadMeter>> {
        self.read().load_meter().cloned()
    }

    /// The audio thread's half, for a test that renders what a device would
    /// hear: every commit lands on it, and ticking it plays the graph.
    ///
    /// # Panics
    ///
    /// If the audio side was already taken — by an earlier call, or by the
    /// engine builder.
    pub fn take_audio_side(&mut self) -> AudioSide {
        self.write().graph().take_audio_side()
    }

    /// Re-prepares every node in the graph, and every node inserted after,
    /// for `rate`.
    ///
    /// For a [`headless`](Self::headless) graph, which has no device to take a
    /// rate from. An engine-built graph already runs at the device's rate
    /// ([`AudioConfig::sample_rate`]). This is an `Editor::reprepare`: with
    /// the audio side still here it completes before this returns; with it
    /// taken, commits wait for its second half.
    pub fn set_sample_rate(&mut self, rate: SampleRate) {
        self.write().graph().set_sample_rate(rate);
    }

    // --- Nodes ---

    /// Adds `node` to the graph, unwired, through its own
    /// [`IntoNode`](tutti_graph::IntoNode), and return its handle and its
    /// controls. Its fork source goes to the editor, so an export forks it as
    /// its type says; it is prepared at the graph's rate.
    ///
    /// Binding the handle to an entity is the caller's (see
    /// [`CapturedControls::bind`](crate::graph::CapturedControls::bind)), as
    /// is addressing its params ([`set_node_params`](Self::set_node_params));
    /// [`add`](Self::add) addresses them, and
    /// [`spawn_audio_node`](crate::graph::SpawnAudioNode) does all three, for
    /// a [`SessionNode`].
    pub fn insert<N: tutti_graph::IntoNode>(&mut self, node: N) -> (AudioNode, N::Controls) {
        self.write().graph().insert(node)
    }

    /// [`insert`](Self::insert), addressing the node's params
    /// ([`SessionNode::params`]).
    pub fn add<N: SessionNode>(&mut self, node: N) -> (AudioNode, N::Controls) {
        self.write().graph().add(node)
    }

    /// Addresses `node`'s params by `params`, its controls: what
    /// [`set_param`](Self::set_param) writes through for it, and what a fork
    /// of it starts from. `None` takes the address away.
    /// [`add`](Self::add) does this from [`SessionNode::params`].
    pub fn set_node_params(&mut self, node: AudioNode, params: Option<tutti_graph::ParamSet>) {
        self.write().graph().set_node_params(node, params);
    }

    /// How many event inputs `node` declares.
    pub fn node_event_inputs(&self, node: AudioNode) -> usize {
        self.read().graph_ref().node_event_inputs(node)
    }

    /// Feeds `sink`'s event input `port` from exactly `sources` (each an
    /// event output: from an `AudioNode` alone, its output 0). Event inputs
    /// merge their sources by offset, so any number may feed one port. Takes
    /// effect with the frame's commit.
    /// [`EventSources`](crate::graph::EventSources) is the ECS form.
    pub fn set_event_sources(&mut self, sink: AudioNode, port: u16, sources: &[EventSource]) {
        self.write().graph().set_event_sources(sink, port, sources);
    }

    /// The event outputs feeding `sink`'s event input `port`, as the graph
    /// holds them: a modulation lane on a plugin's parameter events is one.
    pub fn event_sources(&self, sink: AudioNode, port: u16) -> Vec<EventSource> {
        self.read().graph_ref().event_sources(sink, port)
    }

    /// Sets what `feeder` feeds `sink`'s event input 0, replacing what it fed
    /// before; empty `sources` removes the feeder. The sink's input reads the
    /// union of every feeder's sources ([`EventSources`](crate::graph::EventSources)
    /// is the `"declared"` feeder). Takes effect with the frame's commit.
    pub fn set_feed(&mut self, sink: AudioNode, feeder: &'static str, sources: Vec<EventSource>) {
        self.write().graph().set_feed(sink, feeder, sources);
    }

    /// What `feeder` feeds `sink`: empty when nothing.
    pub fn feed(&self, sink: AudioNode, feeder: &'static str) -> Vec<EventSource> {
        self.read().graph_ref().feed(sink, feeder).to_vec()
    }

    /// Moves every feeder's sources from sink `from` to sink `to`: what the
    /// event reconcile does when an entity is re-bound to a new node.
    pub fn move_feeds(&mut self, from: AudioNode, to: AudioNode) {
        self.write().graph().move_feeds(from, to);
    }

    /// Inserts a hosted plugin: bound (`PluginClient::bind`) and inserted as
    /// the node it then is, handing the editor its fork source (a fork
    /// by state transfer) and its latency (its `Shape`). Capture its controls
    /// first ([`CapturedControls::for_plugin`](crate::graph::CapturedControls::for_plugin)):
    /// the plugin is out of reach once inserted.
    ///
    /// The session hosts it from here (`tutti_session::HostedPlugin`): its
    /// health is watched by
    /// [`plugin_health_poll`](crate::plugin_host::plugin_health_poll), which
    /// removes a dead one's node and the `AudioNode` of whichever entity
    /// holds it, and its latency and render mode are the session's.
    #[cfg(feature = "plugin")]
    pub fn insert_plugin(&mut self, client: Box<tutti_plugin::handles::PluginClient>) -> AudioNode {
        self.write().graph().insert_plugin(client)
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
        self.read().plugin(node).map(f)
    }

    /// The render mode last announced to the hosted plugins: `Offline`
    /// while an export is in flight, else `Realtime` (the session's
    /// `Phase::RenderMode`).
    #[cfg(feature = "plugin")]
    pub fn render_mode(&self) -> tutti_plugin::RenderMode {
        self.read().render_mode()
    }

    /// [`replace`](Self::replace) for a hosted plugin: swap the plugin behind
    /// `node` for `client`, fading when the running node is a plugin of the
    /// same ports and latency, a plain swap otherwise.
    /// [`crossfade_plugin_node`](crate::graph::crossfade_plugin_node) does
    /// this with the incoming plugin's captured controls.
    #[cfg(feature = "plugin")]
    pub fn replace_plugin(
        &mut self,
        node: AudioNode,
        client: Box<tutti_plugin::handles::PluginClient>,
        fade: Seconds,
        curve: CrossfadeCurve,
    ) -> Result<(), ReplaceRefused<Box<tutti_plugin::handles::PluginClient>>> {
        self.write()
            .graph()
            .replace_plugin(node, client, fade, curve)
    }

    /// Takes `node` out of the graph. Every edge to and from it reads silence
    /// afterwards, so a sink still naming it is left silent rather than
    /// dangling.
    ///
    /// Returns whether it was there; removing a node twice is a no-op.
    pub fn remove(&mut self, node: AudioNode) -> bool {
        self.write().remove(node)
    }

    /// Whether `node` is in the graph, a modulation lane or signal node
    /// included (staged, committed or not).
    pub fn contains(&self, node: AudioNode) -> bool {
        self.read().graph_ref().contains(node)
    }

    /// Swaps the node behind `node` for `incoming`, fading from one to the
    /// other over `fade` along `curve`. `node` keeps its handle and every edge
    /// to and from it. Hands back `incoming`'s controls; address its params
    /// with [`set_node_params`](Self::set_node_params).
    ///
    /// The fade needs a running node whose shape `incoming`'s fits (ports,
    /// latency, in-place acceptance, event resolution and capacity, declared
    /// params); without one (the node not committed yet, or a shape change)
    /// `incoming` lands as a plain swap on the next commit. And it can be
    /// refused: while the graph re-prepares, with `incoming` handed back
    /// ([`ReplaceRefused::Busy`], retry after the re-prepare resumes), and for
    /// good on a poisoned graph. [`crossfade`](Self::crossfade) keeps a
    /// `Busy` one waiting instead, and
    /// [`crossfade_audio_node`](crate::graph::crossfade_audio_node) binds the
    /// incoming controls once it lands.
    pub fn replace<N: tutti_graph::IntoNode>(
        &mut self,
        node: AudioNode,
        incoming: N,
        fade: Seconds,
        curve: CrossfadeCurve,
    ) -> Result<N::Controls, ReplaceRefused<N>> {
        self.write().graph().replace(node, incoming, fade, curve)
    }

    /// [`replace`](Self::replace), addressing the incoming node's params, and
    /// keeping it waiting while the graph re-prepares: the frame's
    /// [`commit_graph`](crate::graph::commit_graph) swaps it in once the
    /// graph has resumed (`tutti_session::Graph::crossfade`).
    ///
    /// # Errors
    ///
    /// On a poisoned graph, or a node not in it: the incoming node is dropped.
    pub fn crossfade<N>(
        &mut self,
        node: AudioNode,
        incoming: N,
        fade: Seconds,
        curve: CrossfadeCurve,
    ) -> Result<Crossfade<N::Controls>, CrossfadeRefused>
    where
        N: SessionNode,
        N::Controls: Send + 'static,
    {
        self.write().graph().crossfade(node, incoming, fade, curve)
    }

    /// [`crossfade`](Self::crossfade) for a hosted plugin.
    ///
    /// # Errors
    ///
    /// As `crossfade`.
    #[cfg(feature = "plugin")]
    pub fn crossfade_plugin(
        &mut self,
        node: AudioNode,
        client: Box<tutti_plugin::handles::PluginClient>,
        fade: Seconds,
        curve: CrossfadeCurve,
    ) -> Result<Crossfade<()>, CrossfadeRefused> {
        self.write()
            .graph()
            .crossfade_plugin(node, client, fade, curve)
    }

    /// Writes `value` to `node`'s scalar param `param`, through the node's
    /// [`ParamSet`](tutti_graph::ParamSet) (its controls, addressed with
    /// [`set_node_params`](Self::set_node_params)): the cell it reads at the
    /// start of its next block — on every graph, including a
    /// [`headless`](Self::headless) one — and the value a fork starts from.
    /// A node without the param takes nothing.
    pub fn set_param(&mut self, node: AudioNode, param: UnitParam, value: f32) {
        self.write().graph().set_param(node, param, value);
    }

    // --- Edges ---

    /// What feeds `node`'s input `port`.
    pub fn source(&self, node: AudioNode, port: usize) -> GraphSource {
        self.read().graph_ref().source(node, port)
    }

    /// Feeds `node`'s input `port` from `source`. A port holds one source; this
    /// replaces whatever it held.
    ///
    /// # Panics
    ///
    /// If `source` is `node` itself: a node cannot feed its own input.
    pub fn set_source(&mut self, node: AudioNode, port: usize, source: GraphSource) {
        self.write().graph().set_source(node, port, source);
    }

    // --- Param modulation ---

    /// Returns whether `node` declares `param` modulatable by the graph: its
    /// shape lists it among its params. False for a node not in the graph.
    pub fn declares_param(&self, node: AudioNode, param: UnitParam) -> bool {
        self.read().graph_ref().declares_param(node, param)
    }

    /// Drives `node`'s declared `param` from `source`'s event output 0 — a
    /// control lane, or any node sending param events (ramps, segments) —
    /// clamped to `range` by the graph's fused param step; `None` leaves the
    /// port reading its base under the range. Replaces whatever drove it;
    /// the change crossfades (`PARAM_DECLICK`) rather than stepping. Takes
    /// effect with the frame's commit, like any edge. A call that changes
    /// only `range` is a control move, not a rewire
    /// (`tutti_graph::Editor::set_param_range`): no commit is needed, and the
    /// running node clamps to the new range from its next block, stepping to
    /// it (a range move is not declicked).
    ///
    /// A new source starts at rest: the port holds its base until the
    /// source sends. A source that sends only on a change (a control lane
    /// whose knob is still) must be asked to send again once it is wired
    /// here (`LaneControls::resync`), and a hand-written one must send its
    /// value on its first block, so a fork of the graph reads it from frame
    /// 0 (doc 014 §10).
    ///
    /// Audio does not drive a param (doc 014 §11): a node that wants a
    /// signal-rate input takes it as one of its own audio inputs.
    ///
    /// # Errors
    ///
    /// Refused, changing nothing, with the error the commit would otherwise
    /// fail on (every commit after it, too): a NaN bound
    /// (`GraphInvalid::BadParamRange`).
    pub fn set_param_mod(
        &mut self,
        node: AudioNode,
        param: UnitParam,
        source: Option<AudioNode>,
        range: tutti_graph::ParamRange,
    ) -> Result<(), tutti_graph::GraphInvalid> {
        self.write()
            .graph()
            .set_param_mod(node, param, source, range)
    }

    /// Stops modulating `node`'s `param`: it reads its own control again,
    /// declicked.
    pub fn clear_param_mod(&mut self, node: AudioNode, param: UnitParam) {
        self.write().graph().clear_param_mod(node, param);
    }

    /// How `node`'s `param` is modulated, as the graph value holds it:
    /// `None` when nothing modulates it.
    pub fn param_mod(&self, node: AudioNode, param: UnitParam) -> Option<tutti_graph::ParamMod> {
        self.read().graph_ref().param_mod(node, param)
    }

    /// What feeds global output `channel`.
    pub fn output_source(&self, channel: usize) -> GraphSource {
        self.read().graph_ref().output_source(channel)
    }

    /// Feeds global output `channel` from `source`.
    ///
    /// # Panics
    ///
    /// If `channel` is past the global outputs.
    pub fn set_output_source(&mut self, channel: usize, source: GraphSource) {
        self.write().graph().set_output_source(channel, source);
    }

    /// Feeds **every** global output from `node`: channel `c` from its port
    /// `c % node_outputs(node)`, or silence if it has no outputs.
    ///
    /// For a headless graph a test or tool wires by hand. It overwrites every
    /// channel and wraps a narrow node across a wide root (stereo into six
    /// reads L R L R L R), which is why a host declares the master with
    /// [`MasterSources`](crate::graph::MasterSources) instead: that neither
    /// wraps nor claims channels it does not name.
    pub fn set_outputs_from(&mut self, node: AudioNode) {
        self.write().graph().set_outputs_from(node);
    }

    // --- Shape ---

    /// The graph's global input count.
    pub fn inputs(&self) -> usize {
        self.read().graph_ref().inputs()
    }

    /// The graph's global output count.
    pub fn outputs(&self) -> usize {
        self.read().graph_ref().outputs()
    }

    /// `node`'s input port count.
    pub fn node_inputs(&self, node: AudioNode) -> usize {
        self.read().graph_ref().node_inputs(node)
    }

    /// `node`'s output port count.
    pub fn node_outputs(&self, node: AudioNode) -> usize {
        self.read().graph_ref().node_outputs(node)
    }

    /// The latency `node` reports: the shape the editor holds, read when the
    /// node was inserted (prepared at the graph's rate) and moved since only
    /// by a hosted plugin's latency change.
    pub fn node_latency(&self, node: AudioNode) -> Samples {
        self.read().graph_ref().node_latency(node)
    }

    /// The tail `node` reports.
    pub fn node_tail(&self, node: AudioNode) -> Tail {
        self.read().graph_ref().node_tail(node)
    }

    /// Widen the global outputs to `channels`. New channels read silence.
    /// Never narrows.
    pub(crate) fn widen_outputs(&mut self, channels: usize) {
        self.write().graph().widen_outputs(channels);
    }

    // --- Latency ---

    /// What compensation this graph needs: the per-output-channel pre-roll
    /// and the total, folded over the graph as edited (committed or not):
    /// audio edges plus each node's event and param-modulation sources.
    /// Mutates nothing, so a latency readout can call it freely.
    ///
    /// Nothing is ever spliced into the graph to apply it — the compiler
    /// compensates every commit — so this is always the figure the plans
    /// compensate by.
    pub fn latency_plan(&self) -> Compensation {
        self.read().graph_ref().latency_plan()
    }

    /// The figures the frame's commit will compensate by — the per-channel
    /// pre-roll and the total — the spec compiled as the commit will compile
    /// it. Nothing is spliced in. `None` when the spec does not compile (the
    /// commit reports why).
    pub(crate) fn compensate(&self) -> Option<(Vec<Samples>, Samples)> {
        self.read().graph_ref().planned_compensation()
    }

    /// The plan last sent to the audio thread (`Editor::base`).
    #[cfg(all(test, feature = "modulation"))]
    pub(crate) fn sent_plan(&self) -> Option<std::sync::Arc<tutti_graph::Plan>> {
        self.read().graph_ref().sent_plan()
    }

    /// Whether an edit waits for a commit: the session's edit flag, set by
    /// every edit, cleared by a commit that sent its plan. The frame's
    /// [`commit_graph`](crate::graph::commit_graph) commits iff it is set.
    pub fn is_edited(&self) -> bool {
        self.read().graph_ref().is_edited()
    }

    /// The compensation of the plan last sent to the audio thread: the
    /// per-channel pre-roll and the total.
    #[cfg(test)]
    pub(crate) fn sent_compensation(&self) -> Option<(Vec<Samples>, Samples)> {
        self.read().graph_ref().sent_compensation()
    }

    /// The session's published per-channel pre-roll, republished with every
    /// plan: what [`ChannelCompensation`](crate::ChannelCompensation) holds.
    pub(crate) fn compensation(&self) -> std::sync::Arc<tutti_core::PrerollTable> {
        std::sync::Arc::clone(self.read().compensation())
    }

    /// The total latency of the plan sent last, as the session published it:
    /// what [`GraphLatency`](crate::GraphLatency) mirrors.
    pub(crate) fn latency(&self) -> Samples {
        self.read().latency()
    }

    /// A live duplicate of the whole graph that shares no state with it,
    /// rendered through its own [`AudioSide`]: `Editor::fork`, each node
    /// forked from its own fork source — what an export renders from
    /// (offline, through [`export`](Self::export)), so a test can check that
    /// a control write reaches it. `None` when a node cannot be forked.
    #[cfg(test)]
    pub(crate) fn fork(&self) -> Option<AudioSide> {
        self.read().graph_ref().fork_live().ok()
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
    /// [`outputs`](Self::outputs).
    ///
    /// For a [`headless`](Self::headless) graph, which has no audio thread to
    /// hear it: it renders the executor while this side holds it, committing
    /// any edit first, under a stopped transport.
    ///
    /// # Panics
    ///
    /// Once the audio side was taken — by
    /// [`take_audio_side`](Self::take_audio_side) or the engine: there is no
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
