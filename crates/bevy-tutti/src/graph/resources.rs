//! The audio graph's own ECS resources: the device config and the editable
//! DSP graph. Each other subsystem keeps its `*Res` beside its own module.
//!
//! Both are inserted by [`build_into`](crate::engine::build_into) once the
//! device is open and the graph is built.

use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;

#[cfg(feature = "export")]
use tutti_core::transport::OfflineTransport;
use tutti_core::{AudioNode, AudioUnit, Compensation, CrossfadeCurve, Samples, Seconds, Tail};
use tutti_types::{ChannelLayout, SampleRate, UnitParam};

use std::sync::{Mutex, MutexGuard, PoisonError};

use super::native::{AudioSide, Committed, NativeGraph, ReplaceRefused};

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

/// Where one input port — a node's, or a global output channel — reads from.
///
/// The graph's own answer to "what feeds this port", in the same vocabulary as
/// [`PortSource`](crate::graph::PortSource) but naming the node by its
/// [`AudioNode`] rather than by entity: a declaration names entities, the graph
/// holds nodes. Read back with [`AudioGraphRes::source`] and
/// [`AudioGraphRes::output_source`]; written with
/// [`set_source`](AudioGraphRes::set_source) and
/// [`set_output_source`](AudioGraphRes::set_output_source).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum GraphSource {
    /// Output `port` of `node`.
    Node(AudioNode, usize),
    /// Global graph input `port` — a hardware or host input channel.
    Input(usize),
    /// Silence: what a port nothing has written holds.
    #[default]
    Silence,
}

/// The editable DSP graph. Its methods are the only way to touch it.
///
/// Edit through them, then set [`GraphDirty`](crate::graph::GraphDirty): the
/// per-frame [`commit_graph`](crate::graph::commit_graph) publishes every edit
/// of the frame to the audio thread at once. Nothing here commits inline.
///
/// The graph is the native `tutti-graph` runtime (design doc 013): an
/// `Editor` on this side, its `Executor` on the audio thread. Every unit goes
/// in as a `Legacy::controlled` node, and PDC is the compiler's — nothing is
/// spliced into the graph to align it. fundsp's `Net` ran behind this type
/// until Phase 3 PR 13, which deleted it.
///
/// # Why opaque
///
/// Every method is named in graph terms — insert a node, set a port's source,
/// replace a unit under a fade — and takes an [`AudioNode`] and a
/// [`GraphSource`], never a runtime type, so what is behind it can change
/// without the signatures moving (it did: `Net`, then both, then native).
///
/// No `Deref`, for the same reason there never was one: graph mutation is paired
/// with the per-frame commit, and keeping it behind named methods keeps the
/// dirty/commit boundary visible at the call site.
///
/// The raw graph is not reachable from outside this crate:
///
/// ```compile_fail,E0616
/// use bevy_tutti::graph::AudioGraphRes;
///
/// let graph = AudioGraphRes::headless(0, 2);
/// let _editor = graph.0;
/// ```
// Mutation: `pub struct AudioGraphRes(pub Mutex<NativeGraph>)` (and a `pub`
// `NativeGraph`) makes the doctest above compile, which fails it. Everything
// else in it compiles as written, so the privacy of the field is the only
// thing it can be failing on.
#[derive(Resource)]
// Behind a `Mutex` only because a `Resource` must be `Sync` and the editor is
// not (it holds boxed nodes and ring ends). Every `&mut self` method reaches
// it with `get_mut`, lock-free; a `&self` query takes the lock, uncontended —
// the resource's own borrow already serializes access.
pub struct AudioGraphRes(Mutex<NativeGraph>);

/// The per-channel pre-roll and the total a compensation pass arrived at —
/// what [`compensate_graph`](crate::graph::latency::compensate_graph)
/// publishes.
pub(crate) struct PdcFigures {
    pub(crate) channels: Vec<Samples>,
    pub(crate) total: Samples,
}

impl AudioGraphRes {
    /// `&self` access. Poison is recovered: a panic mid-edit leaves a spec
    /// the next commit validates, never a torn audio thread.
    fn read(&self) -> MutexGuard<'_, NativeGraph> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `&mut self` access: no lock needed.
    fn write(&mut self) -> &mut NativeGraph {
        self.0.get_mut().unwrap_or_else(PoisonError::into_inner)
    }

    /// A graph with no device behind it: `inputs` global inputs and `outputs`
    /// global outputs, prepared at 44.1 kHz until
    /// [`set_sample_rate`](Self::set_sample_rate).
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
        Self::with_rate(inputs, outputs, SampleRate::DEFAULT)
    }

    /// A graph with every node run at `rate`: what the engine builder makes,
    /// at the device's rate.
    pub(crate) fn with_rate(inputs: usize, outputs: usize, rate: SampleRate) -> Self {
        Self(Mutex::new(NativeGraph::new(inputs, outputs, rate)))
    }

    /// The audio thread's half, for a test that renders what a device would
    /// hear: every commit lands on it, and ticking it plays the graph.
    ///
    /// # Panics
    ///
    /// If the audio side was already taken — by an earlier call, or by the
    /// engine builder.
    pub fn take_audio_side(&mut self) -> AudioSide {
        AudioSide::native(self.write().take_executor())
    }

    /// Re-prepare every node in the graph, and every node inserted after,
    /// for `rate`.
    ///
    /// For a [`headless`](Self::headless) graph, which has no device to take a
    /// rate from. An engine-built graph already runs at the device's rate
    /// ([`AudioConfig::sample_rate`]). This is an `Editor::reprepare`: with
    /// the audio side still here it completes before this returns; with it
    /// taken, commits wait for its second half.
    pub fn set_sample_rate(&mut self, rate: SampleRate) {
        self.write().set_sample_rate(rate);
    }

    /// Refuse, before a device restart stops the stream, a `max_block` the
    /// graph cannot be re-prepared to: past the engine's block capacity, or
    /// while a re-prepare is already between its halves, or on a poisoned
    /// graph.
    pub(crate) fn check_rerate(
        &self,
        max_block: Option<Samples>,
    ) -> Result<(), tutti_graph::CommitError> {
        self.read().check_reprepare(max_block)
    }

    /// Move a live graph to the device's new `rate` and, if given, a new
    /// `max_block`: what a device restart does between the stop and the
    /// start ([`restart_device`](crate::engine::restart_device)).
    ///
    /// The first half of `Editor::reprepare` ([`NativeGraph::reprepare`]);
    /// the executor adopts it on its next blocks, the engine following the
    /// rate on the first of them. Every unit instance is kept; only
    /// time-based state restarts.
    ///
    /// Not committed here; see [`restart_device`](crate::engine::restart_device).
    pub(crate) fn rerate(
        &mut self,
        rate: SampleRate,
        max_block: Option<Samples>,
    ) -> Result<(), tutti_graph::CommitError> {
        self.write().reprepare(rate, max_block)
    }

    /// Whether a re-prepare is between its two commits.
    pub(crate) fn is_repreparing(&self) -> bool {
        self.read().is_repreparing()
    }

    /// Build the engine over this graph's audio side, which it takes:
    /// `Engine::with_graph` over the editor and its executor, which bounds the
    /// editor to what the engine can render (a commit past it is refused, and
    /// logged).
    pub(crate) fn engine(
        &mut self,
        transport: &tutti_core::Transport,
    ) -> Result<tutti_core::Engine, tutti_core::GraphEngineError> {
        let graph = self.write();
        let exec = graph.take_executor();
        tutti_core::Engine::with_graph(transport, graph.editor_mut(), exec)
    }

    /// The node the engine's beat ports come from: an
    /// [`EnvClock`](tutti_core::EnvClock). A graph engine drives its own
    /// `TransportClock` and forbids a second in the graph, and an `EnvClock`
    /// emits the same samples from each block's `Env` (doc 013, Phase 3 gap
    /// 5).
    pub(crate) fn insert_beat_clock(&mut self) -> AudioNode {
        self.write().insert_env_clock()
    }

    // --- Nodes ---

    /// Add `unit` to the graph, unwired, and return its handle.
    ///
    /// Binding the handle to an entity is the caller's (see
    /// [`CapturedControls::bind`](crate::graph::CapturedControls::bind));
    /// [`spawn_audio_node`](crate::graph::SpawnAudioNode) does both.
    ///
    /// The unit is prepared at the graph's rate.
    pub fn insert<U: AudioUnit + 'static>(&mut self, unit: U) -> AudioNode {
        self.insert_boxed(Box::new(unit))
    }

    /// [`insert`](Self::insert) for a unit that is already boxed (a plugin, a
    /// trait-object factory's product).
    ///
    /// For a unit a registry captured a MIDI port from, use
    /// [`insert_with`](Self::insert_with): pushed this way, an export
    /// holding it is refused (`ExportError::NotForkable`), since its fork
    /// would not carry its clip.
    pub fn insert_boxed(&mut self, unit: Box<dyn AudioUnit>) -> AudioNode {
        self.write().insert(unit, None)
    }

    /// [`insert_boxed`](Self::insert_boxed) for a unit whose controls were
    /// captured ([`CapturedControls::capture`](crate::graph::CapturedControls::capture)):
    /// a unit with a captured MIDI port goes in so that a fork of the graph
    /// (an export) carries the clip installed on that port — through the
    /// type's own fork source, or the generic fork — and refuses by name
    /// when it cannot, never renders it as silence
    /// (`MidiNode::fork_source`, with the `midi` feature).
    ///
    /// Every insertion path in this crate goes this way. Then
    /// [`bind`](crate::graph::CapturedControls::bind) `controls` as ever.
    pub fn insert_with(
        &mut self,
        unit: Box<dyn AudioUnit>,
        controls: &mut crate::graph::CapturedControls,
    ) -> AudioNode {
        #[cfg(feature = "midi")]
        let fork = controls.take_fork();
        #[cfg(not(feature = "midi"))]
        let fork = {
            let _ = controls;
            None
        };
        self.write().insert(unit, fork)
    }

    /// [`insert_boxed`](Self::insert_boxed) for a hosted plugin, keeping the
    /// concrete `PluginClient` so the editor is handed its fork source (a fork
    /// by state transfer): an export of a graph holding a plugin inserted as
    /// a boxed unit is refused as not forkable.
    #[cfg(feature = "plugin")]
    pub(crate) fn insert_plugin(
        &mut self,
        client: Box<tutti_plugin::handles::PluginClient>,
    ) -> AudioNode {
        self.write().insert_plugin(client)
    }

    /// Take `node` out of the graph. Every edge to and from it reads silence
    /// afterwards, so a sink still naming it is left silent rather than
    /// dangling.
    ///
    /// Returns whether it was there; removing a node twice is a no-op.
    pub fn remove(&mut self, node: AudioNode) -> bool {
        self.write().remove(node)
    }

    /// Whether `node` is in the graph.
    pub fn contains(&self, node: AudioNode) -> bool {
        self.read().contains(node)
    }

    /// Swap the unit behind `node` for `unit`, fading from one to the other over
    /// `fade`. `node` keeps its handle and every edge to and from it.
    ///
    /// `unit` must have `node`'s input and output counts: this replaces a unit,
    /// not a node's shape. A shape change is a remove and an insert.
    ///
    /// The fade needs a running unit of the same latency to fade from; without
    /// one (the node not committed yet, or a latency change) the new unit
    /// lands as a plain swap on the next commit. And it can be refused: while
    /// the graph re-prepares, with the unit handed back
    /// ([`ReplaceRefused::Busy`], retry after the re-prepare resumes), and for
    /// good on a poisoned graph. Swap the captured controls only on `Ok` —
    /// [`crossfade_audio_node`](crate::graph::crossfade_audio_node) does all
    /// of this, with the incoming unit's captured controls
    /// ([`replace_with`](Self::replace_with)).
    pub fn replace(
        &mut self,
        node: AudioNode,
        unit: Box<dyn AudioUnit>,
        fade: Seconds,
        curve: CrossfadeCurve,
    ) -> Result<(), ReplaceRefused> {
        self.write().replace(node, unit, fade, curve, &mut None)
    }

    /// [`replace`](Self::replace) for a unit whose controls were captured,
    /// forking as [`insert_with`](Self::insert_with) says. The fork is taken
    /// from `controls` only when the unit lands, so a refused
    /// ([`ReplaceRefused::Busy`]) unit keeps it for its retry.
    pub fn replace_with(
        &mut self,
        node: AudioNode,
        unit: Box<dyn AudioUnit>,
        fade: Seconds,
        curve: CrossfadeCurve,
        controls: &mut crate::graph::CapturedControls,
    ) -> Result<(), ReplaceRefused> {
        #[cfg(feature = "midi")]
        {
            let mut fork = controls.take_fork();
            let landed = self.write().replace(node, unit, fade, curve, &mut fork);
            // Handed back untaken on a refusal: put it back for the retry.
            if let Some(fork) = fork {
                controls.put_fork(fork);
            }
            landed
        }
        #[cfg(not(feature = "midi"))]
        {
            let _ = controls;
            self.write().replace(node, unit, fade, curve, &mut None)
        }
    }

    /// Write `value` to `node`'s scalar param `param`, through the node's own
    /// settings ring, drained at the start of its next block — on every graph,
    /// including a [`headless`](Self::headless) one. Not by mutating the node.
    pub fn set_param(&mut self, node: AudioNode, param: UnitParam, value: f32) {
        self.write().set_param(node, param, value);
    }

    /// Run `f` on the unit behind `node`, for inspection: `None` if `node` is
    /// not in the graph.
    ///
    /// The node's shadow — never the unit the audio thread runs: a copy
    /// isolated when the unit was inserted, with every
    /// [`set_param`](Self::set_param) applied since, never processed. So it is
    /// for tests and diagnostics that probe a unit's construction (its ports,
    /// a LUT baked in when it was built), not a way to reach live state. A
    /// host that drives a node keeps the handles it captured at insertion;
    /// see [`capture`](crate::graph::capture). `None` for the engine's beat
    /// generator, which is not an `AudioUnit`.
    pub fn inspect<R>(&self, node: AudioNode, f: impl FnOnce(&dyn AudioUnit) -> R) -> Option<R> {
        self.read().inspect(node, f)
    }

    // --- Edges ---

    /// What feeds `node`'s input `port`.
    pub fn source(&self, node: AudioNode, port: usize) -> GraphSource {
        self.read().source(node, port)
    }

    /// Feed `node`'s input `port` from `source`. A port holds one source; this
    /// replaces whatever it held.
    ///
    /// # Panics
    ///
    /// If `source` is `node` itself: a node cannot feed its own input.
    pub fn set_source(&mut self, node: AudioNode, port: usize, source: GraphSource) {
        self.write().set_source(node, port, source);
    }

    /// What feeds global output `channel`.
    pub fn output_source(&self, channel: usize) -> GraphSource {
        self.read().output_source(channel)
    }

    /// Feed global output `channel` from `source`.
    ///
    /// # Panics
    ///
    /// If `channel` is past the global outputs.
    pub fn set_output_source(&mut self, channel: usize, source: GraphSource) {
        self.write().set_output_source(channel, source);
    }

    /// Feed **every** global output from `node`: channel `c` from its port
    /// `c % node_outputs(node)`, or silence if it has no outputs.
    ///
    /// For a headless graph a test or tool wires by hand. It overwrites every
    /// channel and wraps a narrow node across a wide root (stereo into six
    /// reads L R L R L R), which is why a host declares the master with
    /// [`MasterSources`](crate::graph::MasterSources) instead: that neither
    /// wraps nor claims channels it does not name.
    pub fn set_outputs_from(&mut self, node: AudioNode) {
        self.write().set_outputs_from(node);
    }

    // --- Shape ---

    /// The graph's global input count.
    pub fn inputs(&self) -> usize {
        self.read().inputs()
    }

    /// The graph's global output count.
    pub fn outputs(&self) -> usize {
        self.read().outputs()
    }

    /// `node`'s input port count.
    pub fn node_inputs(&self, node: AudioNode) -> usize {
        self.read().node_inputs(node)
    }

    /// `node`'s output port count.
    pub fn node_outputs(&self, node: AudioNode) -> usize {
        self.read().node_outputs(node)
    }

    /// The latency `node` reports: the shape the editor holds, probed when
    /// the unit was inserted (at the graph's rate, rounded to the nearest
    /// frame) and moved since only by a hosted plugin's latency change.
    pub fn node_latency(&self, node: AudioNode) -> Samples {
        self.read().node_latency(node)
    }

    /// The tail `node` reports.
    pub fn node_tail(&self, node: AudioNode) -> Tail {
        self.read().node_tail(node)
    }

    /// Widen the global outputs to `channels`. New channels read silence.
    /// Never narrows.
    pub(crate) fn widen_outputs(&mut self, channels: usize) {
        self.write().widen_outputs(channels);
    }

    // --- Latency ---

    /// What compensation this graph needs: the per-output-channel pre-roll
    /// and the total, folded over the authored graph. Mutates nothing, so a
    /// latency readout can call it freely.
    ///
    /// Nothing is ever spliced into the graph to apply it — the compiler
    /// compensates every commit — so this is always the figure the plans
    /// compensate by, whether or not
    /// [`LatencyCompensationPlugin`](crate::graph::latency::LatencyCompensationPlugin)
    /// runs.
    pub fn latency_plan(&self) -> Compensation {
        self.read().latency_plan()
    }

    /// The figures the frame's commit will compensate by: the spec compiled
    /// as the commit will compile it, read off the plan's `compensation()`
    /// and `total_latency()`. Nothing is spliced in. `None` when the spec does
    /// not compile (the commit reports why).
    pub(crate) fn compensate(&self) -> Option<PdcFigures> {
        self.read()
            .planned_compensation()
            .map(|(channels, total)| PdcFigures { channels, total })
    }

    /// The compensation of the plan last sent to the audio thread.
    #[cfg(test)]
    pub(crate) fn sent_compensation(&self) -> Option<PdcFigures> {
        self.read()
            .sent_compensation()
            .map(|(channels, total)| PdcFigures { channels, total })
    }

    /// A live duplicate of the whole graph that shares no state with it,
    /// rendered through its own [`AudioSide`]: `Editor::fork`, each node
    /// forked from its shadow — what an export renders from (offline, through
    /// [`export`](Self::export)), so a test can check that a control write
    /// reaches it. `None` when a node cannot be forked.
    #[cfg(test)]
    pub(crate) fn fork(&self) -> Option<AudioSide> {
        self.read().fork().ok()
    }

    /// Record the value a fork of `node` starts `param` at, without touching
    /// the live unit: the node's shadow, which its forks clone. For a param
    /// the modulation driver owns: live, the driver writes `base + Σ layers`
    /// into the node's cell every frame; a fork is not driven by it and must
    /// carry the authored base.
    #[cfg(feature = "modulation")]
    pub(crate) fn set_param_snapshot(&mut self, node: AudioNode, param: UnitParam, value: f32) {
        self.write().set_param_snapshot(node, param, value);
    }

    /// `node`'s latency may have moved at runtime — a hosted plugin's latency
    /// cell changed. The editor holds the latency it probed at insert, so
    /// this probes the node's shadow again and, if the figure moved, hands it
    /// to `Editor::set_latency`, which moves PDC on the next commit without
    /// touching the unit.
    #[cfg(feature = "plugin")]
    pub(crate) fn refresh_node_latency(&mut self, node: AudioNode) {
        self.write().refresh_node_latency(node);
    }

    // --- Publishing and rendering ---

    /// Publish every edit since the last commit to the audio thread. Returns
    /// whether it is done with them: `false` means "retry next frame", and the
    /// caller keeps [`GraphDirty`](crate::graph::GraphDirty) set.
    ///
    /// `Editor::commit`, which is refused for now while `QUEUE_CAPACITY`
    /// commits are out or a re-prepare is between its halves (the retry
    /// case), and for good when the graph does not compile (logged; the next
    /// edit tries again). A changed global output count needs nothing
    /// special: it is part of the spec the commit compiles.
    pub(crate) fn commit(&mut self) -> bool {
        self.write().commit() == Committed::Done
    }

    /// Drain what the audio thread sent back, freeing it here, and flush any
    /// settings a full ring held. On the main thread, every frame, from
    /// [`commit_graph`](crate::graph::commit_graph).
    pub(crate) fn collect(&mut self) {
        self.write().collect();
    }

    /// Render one frame on the calling thread: no inputs, `output` sized to
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

    // --- Export ---

    /// The graph an export renders: `node`'s sub-graph, or the whole graph
    /// for `None`, as a [`tutti_export::RenderGraph`] sharing no state with
    /// this one. Main thread; the render then runs on a worker. See
    /// `export::run::start_exports`.
    ///
    /// `Editor::fork` (`ForkTarget::Master` or `ForkTarget::Node`) in
    /// `ForkMode::Offline(ctx)`, prepared at `rate`. Every node is forked from
    /// its shadow — isolated, rebound onto `ctx`, reset — **the master
    /// included**: a master export renders what the graph is driven to play
    /// from `ctx`, starting silent, not a copy of what is sounding now (doc
    /// 013, PR 12).
    ///
    /// `Err(ExportRefused::GraphHasNoOutputs)` when the graph has no global
    /// outputs, and `Err(ExportRefused::NoOutputs)` when `node` has no audio
    /// outputs (or is not in the graph); a fork refusal (`NotForkable`, a
    /// plugin whose fresh instance did not load) is the renderer's own error.
    ///
    /// `midi` is every node with a captured MIDI port; one the fork holds
    /// that cannot carry its clip refuses the export
    /// (`NativeGraph::fork_for_export`).
    #[cfg(feature = "export")]
    pub(crate) fn export(
        &self,
        node: Option<AudioNode>,
        ctx: &OfflineTransport,
        rate: SampleRate,
        midi: &std::collections::BTreeSet<tutti_types::NodeKey>,
    ) -> Result<tutti_export::RenderGraph, ExportRefused> {
        if self.outputs() == 0 {
            return Err(ExportRefused::GraphHasNoOutputs);
        }
        let target = match node {
            None => tutti_graph::ForkTarget::Master,
            Some(node) => tutti_graph::ForkTarget::Node(super::native::key(node)),
        };
        match self.read().fork_for_export(target, ctx, rate, midi) {
            Ok(graph) => Ok(graph),
            Err(tutti_export::Error::Fork(
                tutti_graph::ForkError::NoOutputs { .. }
                | tutti_graph::ForkError::NoSuchNode { .. },
            )) => Err(ExportRefused::NoOutputs),
            Err(e) => Err(ExportRefused::Render(e)),
        }
    }
}

/// Why [`AudioGraphRes::export`] built nothing.
#[cfg(feature = "export")]
#[derive(Debug)]
pub(crate) enum ExportRefused {
    /// The target node has no audio outputs, or is not in the graph.
    NoOutputs,
    /// The graph has no global outputs: nothing any export could render.
    GraphHasNoOutputs,
    /// The fork was refused: a node cannot be forked, or its fork source
    /// failed.
    Render(tutti_export::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_nodes::testing::{Const, Through};

    /// Every [`GraphSource`] arm survives a write and a read-back, on a node
    /// port and on a global output: the translation to the spec's spelling
    /// and back is total and lossless.
    ///
    /// Mutation: lowering `Input(port)` to `Zero` (or lifting `Global` to
    /// `Silence`) fails the `Input` row; lowering `Node` with a fixed port 0
    /// fails the port-1 row.
    #[test]
    fn every_source_reads_back_as_written() {
        let mut graph = AudioGraphRes::headless(1, 2);
        let stereo = graph.insert(Const::frame(&[1.0, 2.0]));
        let sink = graph.insert(Through::mono());
        for source in [
            GraphSource::Node(stereo, 1),
            GraphSource::Node(stereo, 0),
            GraphSource::Input(0),
            GraphSource::Silence,
        ] {
            graph.set_source(sink, 0, source);
            assert_eq!(graph.source(sink, 0), source, "node port");
            graph.set_output_source(1, source);
            assert_eq!(graph.output_source(1), source, "global output");
        }
    }

    /// `remove` says whether the node was there, so removing a node twice is a
    /// no-op rather than a panic.
    ///
    /// Mutation: dropping the `contains` guard panics on the second call.
    #[test]
    fn removing_a_node_twice_is_a_no_op() {
        let mut graph = AudioGraphRes::headless(0, 1);
        let node = graph.insert(Const::mono(1.0));
        assert!(graph.contains(node));
        assert!(graph.remove(node));
        assert!(!graph.contains(node));
        assert!(!graph.remove(node));
    }
}
