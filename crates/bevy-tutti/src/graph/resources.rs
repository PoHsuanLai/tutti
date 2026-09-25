//! The audio graph's own ECS resources: the device config and the editable
//! DSP graph. Each other subsystem keeps its `*Res` beside its own module.
//!
//! Both are inserted by [`build_into`](crate::engine::build_into) once the
//! device is open and the graph is built.

use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;

use tutti_core::dsp::Net;
#[cfg(feature = "export")]
use tutti_core::transport::OfflineTransport;
use tutti_core::{
    AudioNode, AudioUnit, Compensation, CrossfadeCurve, NetBackend, Samples, Seconds, Tail,
};
use tutti_types::{ChannelLayout, SampleRate, UnitParam};

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
    /// The device's output width. The graph root is widened to match a
    /// declaration, not to this — see `MasterSources`.
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

impl GraphSource {
    /// The backend's spelling. Total over both enums, so the two cannot drift.
    fn lower(self) -> tutti_core::dsp::Source {
        use tutti_core::dsp::Source;
        match self {
            GraphSource::Node(node, port) => Source::Local(node.0, port),
            GraphSource::Input(port) => Source::Global(port),
            GraphSource::Silence => Source::Zero,
        }
    }

    /// [`lower`](Self::lower)'s inverse.
    fn lift(source: tutti_core::dsp::Source) -> Self {
        use tutti_core::dsp::Source;
        match source {
            Source::Local(id, port) => GraphSource::Node(AudioNode(id), port),
            Source::Global(port) => GraphSource::Input(port),
            Source::Zero => GraphSource::Silence,
        }
    }
}

/// The editable DSP graph. Its methods are the only way to touch it.
///
/// Edit through them, then set [`GraphDirty`](crate::graph::GraphDirty): the
/// per-frame [`commit_graph`](crate::graph::commit_graph) publishes every edit
/// of the frame to the audio thread at once. Nothing here commits inline.
///
/// # Why opaque
///
/// The graph behind this is fundsp's `Net` today, and design doc 013 moves it to
/// `tutti-graph`. Every method is named in graph terms — insert a node, set a
/// port's source, replace a unit under a fade — and takes an [`AudioNode`] and a
/// [`GraphSource`], never a `Net` type, so a second backend can sit behind the
/// same signatures. What only `Net` has (the compensation delays it splices in,
/// the arity-permitting commit) stays inside the impl.
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
/// let _net = graph.0;
/// ```
// Mutation: `pub struct AudioGraphRes(pub Net)` makes the doctest above compile,
// which fails it. Everything else in it compiles as written, so the privacy of
// the field is the only thing it can be failing on.
#[derive(Resource)]
pub struct AudioGraphRes(Net);

impl AudioGraphRes {
    /// A graph with no device behind it: `inputs` global inputs, `outputs`
    /// global outputs, and an audio side taken and dropped so that commits have
    /// somewhere to go.
    ///
    /// For tests and headless tools that drive the reconcile pipeline without
    /// opening a device. [`render_frame`](Self::render_frame) renders it on the
    /// calling thread.
    pub fn headless(inputs: usize, outputs: usize) -> Self {
        let mut graph = Self::unattached(inputs, outputs);
        drop(graph.take_backend());
        graph
    }

    /// A graph whose audio side has not been taken yet — or, if it never is, a
    /// graph with none at all.
    ///
    /// With no audio side, a [`set_param`](Self::set_param) lands on the
    /// control side's copy of the node at once rather than being queued for an
    /// audio thread; a test of the write path that reads a node's state back
    /// wants exactly that. **It cannot [`commit`](crate::graph::commit_graph)**,
    /// so nothing may mark it dirty. Take the audio side with
    /// [`take_audio_side`](Self::take_audio_side) to render what a device would
    /// hear, or use [`headless`](Self::headless).
    pub fn unattached(inputs: usize, outputs: usize) -> Self {
        Self(Net::new(inputs, outputs))
    }

    /// The audio thread's half, for a test that renders what a device would
    /// hear: every commit lands on it, and ticking it plays the graph.
    ///
    /// # Panics
    ///
    /// If the audio side was already taken — by an earlier call, by
    /// [`headless`](Self::headless), or by the engine builder.
    pub fn take_audio_side(&mut self) -> impl AudioUnit + use<> {
        self.take_backend()
    }

    /// Re-rate every node in the graph, and every node inserted after.
    ///
    /// For a [`headless`](Self::headless) graph, which has no device to take a
    /// rate from. An engine-built graph already runs at the device's rate
    /// ([`AudioConfig::sample_rate`]).
    pub fn set_sample_rate(&mut self, rate: SampleRate) {
        self.0.set_sample_rate(rate);
    }

    /// [`take_audio_side`](Self::take_audio_side) as the concrete type the
    /// engine takes.
    pub(crate) fn take_backend(&mut self) -> NetBackend {
        self.0.backend()
    }

    // --- Nodes ---

    /// Add `unit` to the graph, unwired, and return its handle.
    ///
    /// Binding the handle to an entity is the caller's (see
    /// [`CapturedControls::bind`](crate::graph::CapturedControls::bind));
    /// [`spawn_audio_node`](crate::graph::SpawnAudioNode) does both.
    pub fn insert<U: AudioUnit + 'static>(&mut self, unit: U) -> AudioNode {
        AudioNode(self.0.add(unit))
    }

    /// [`insert`](Self::insert) for a unit that is already boxed (a plugin, a
    /// trait-object factory's product).
    pub fn insert_boxed(&mut self, unit: Box<dyn AudioUnit>) -> AudioNode {
        AudioNode(self.0.push(unit))
    }

    /// Take `node` out of the graph. Every edge to and from it reads silence
    /// afterwards, so a sink still naming it is left silent rather than
    /// dangling.
    ///
    /// Returns whether it was there; removing a node twice is a no-op.
    pub fn remove(&mut self, node: AudioNode) -> bool {
        if !self.0.contains(node.0) {
            return false;
        }
        drop(self.0.remove(node.0));
        true
    }

    /// Whether `node` is in the graph.
    pub fn contains(&self, node: AudioNode) -> bool {
        self.0.contains(node.0)
    }

    /// Swap the unit behind `node` for `unit`, fading from one to the other over
    /// `fade`. `node` keeps its handle and every edge to and from it.
    ///
    /// `unit` must have `node`'s input and output counts: this replaces a unit,
    /// not a node's shape. A shape change is a remove and an insert.
    pub fn replace(
        &mut self,
        node: AudioNode,
        unit: Box<dyn AudioUnit>,
        fade: Seconds,
        curve: CrossfadeCurve,
    ) {
        self.0
            .crossfade(node.0, tutti_core::net_fade(curve), fade.get(), unit);
    }

    /// Write `value` to `node`'s scalar param `param`. Delivered to the audio
    /// thread through the graph's settings queue, not by mutating the node.
    pub fn set_param(&mut self, node: AudioNode, param: UnitParam, value: f32) {
        self.0
            .set(tutti_core::unit_param::node_setting(node.0, param, value));
    }

    /// Run `f` on the unit behind `node`, for inspection: `None` if `node` is
    /// not in the graph.
    ///
    /// The control side's copy — never the one the audio thread runs — so it is
    /// for tests and diagnostics that probe a unit's construction (its ports, a
    /// LUT baked in when it was built), not a way to reach live state. A host
    /// that drives a node keeps the handles it captured at insertion; see
    /// [`capture`](crate::graph::capture).
    pub fn inspect<R>(&self, node: AudioNode, f: impl FnOnce(&dyn AudioUnit) -> R) -> Option<R> {
        self.0.contains(node.0).then(|| f(self.0.node(node.0)))
    }

    // --- Edges ---

    /// What feeds `node`'s input `port`.
    pub fn source(&self, node: AudioNode, port: usize) -> GraphSource {
        GraphSource::lift(self.0.source(node.0, port))
    }

    /// Feed `node`'s input `port` from `source`. A port holds one source; this
    /// replaces whatever it held.
    ///
    /// # Panics
    ///
    /// If `source` is `node` itself: a node cannot feed its own input.
    pub fn set_source(&mut self, node: AudioNode, port: usize, source: GraphSource) {
        self.0.set_source(node.0, port, source.lower());
    }

    /// What feeds global output `channel`.
    pub fn output_source(&self, channel: usize) -> GraphSource {
        GraphSource::lift(self.0.output_source(channel))
    }

    /// Feed global output `channel` from `source`.
    pub fn set_output_source(&mut self, channel: usize, source: GraphSource) {
        self.0.set_output_source(channel, source.lower());
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
        self.0.pipe_output(node.0);
    }

    // --- Shape ---

    /// The graph's global input count.
    pub fn inputs(&self) -> usize {
        self.0.inputs()
    }

    /// The graph's global output count.
    pub fn outputs(&self) -> usize {
        self.0.outputs()
    }

    /// `node`'s input port count.
    pub fn node_inputs(&self, node: AudioNode) -> usize {
        self.0.inputs_in(node.0)
    }

    /// `node`'s output port count.
    pub fn node_outputs(&self, node: AudioNode) -> usize {
        self.0.outputs_in(node.0)
    }

    /// The latency `node` reports.
    ///
    /// Through the `Net`'s own `LatencyGraph` impl rather than the unit: the
    /// `AudioUnit` method takes `&mut self`, and that impl is the one place the
    /// clone-to-probe is written down. [`node_tail`](Self::node_tail) likewise.
    pub fn node_latency(&self, node: AudioNode) -> Samples {
        tutti_core::LatencyGraph::latency(&self.0, node.0)
    }

    /// The tail `node` reports.
    pub fn node_tail(&self, node: AudioNode) -> Tail {
        tutti_core::TailGraph::tail(&self.0, node.0)
    }

    /// Widen the global outputs to `channels`. New channels read silence.
    ///
    /// Global outputs are sinks, so changing their count cannot dangle a
    /// reference; [`commit`](Self::commit) is the commit that accepts it.
    pub(crate) fn widen_outputs(&mut self, channels: usize) {
        self.0.set_output_arity_live(channels);
    }

    // --- Latency ---

    /// What compensation this graph would need, without applying any: the
    /// per-output-channel pre-roll and the total. Mutates nothing, so a latency
    /// readout can call it freely.
    ///
    /// Over the graph as it stands — after
    /// [`LatencyCompensationPlugin`](crate::graph::latency::LatencyCompensationPlugin)
    /// has run, that includes the delays it applied, so the figure is what is
    /// *still* misaligned.
    pub fn latency_plan(&self) -> Compensation {
        tutti_core::latency::plan(&self.0)
    }

    /// Align every path: re-plan compensation over the authored graph and apply
    /// it, returning the plan.
    pub(crate) fn compensate(&mut self) -> Compensation {
        tutti_core::latency::compensate(&mut self.0)
    }

    /// Whether any compensation delays [`compensate`](Self::compensate) applied
    /// are in the graph.
    pub(crate) fn has_compensation(&self) -> bool {
        self.0
            .ids()
            .any(|&id| self.0.node(id).get_id() == tutti_core::PDC_DELAY_ID)
    }

    /// Whether `source` reads from a compensation delay rather than from an
    /// authored node — a node with no entity, so no declaration names it.
    ///
    /// Identified by `AudioUnit::get_id`, which is how
    /// `DelayInsertion::clear_delays` finds them too — one marker, one
    /// definition of "this node is derived, not authored".
    pub(crate) fn is_compensation(&self, source: GraphSource) -> bool {
        let GraphSource::Node(node, _) = source else {
            return false;
        };
        self.0.contains(node.0) && self.0.node(node.0).get_id() == tutti_core::PDC_DELAY_ID
    }

    // --- Publishing and rendering ---

    /// Publish every edit since the last commit to the audio thread.
    ///
    /// Accepts a changed global output count (see
    /// [`widen_outputs`](Self::widen_outputs)): `tutti_core`'s
    /// `Engine::process_segment` re-reads the backend's output count every
    /// block. Identical to a plain commit when the count is unchanged.
    pub(crate) fn commit(&mut self) {
        self.0.commit_output_arity_change();
    }

    /// Render one frame on the calling thread: no inputs, `output` sized to
    /// [`outputs`](Self::outputs).
    ///
    /// For a [`headless`](Self::headless) graph, which has no audio thread to
    /// hear it. On an engine-built graph it runs the control side's copy of each
    /// node, which the audio thread never hears either, and advances their state.
    pub fn render_frame(&mut self, output: &mut [f32]) {
        self.0.tick(&[], output);
    }

    // --- Export ---

    /// The whole graph as an offline copy, keeping its live transport
    /// bindings. See `export::run::prepare_net`.
    #[cfg(feature = "export")]
    pub(crate) fn export_master(&self) -> Net {
        self.0.clone()
    }

    /// An offline copy rendering `node`'s outputs, isolated from live inputs,
    /// rebound onto `ctx` and reset. `None` when `node` has no outputs.
    #[cfg(feature = "export")]
    pub(crate) fn export_node(&self, node: AudioNode, ctx: &OfflineTransport) -> Option<Net> {
        let pending = self.0.clone_isolated(node.0)?;
        // Isolate (sever live inputs) and rebind (re-point at `ctx`) in the
        // one order they may happen — see `PendingClone::isolate_for_offline`.
        let mut net = pending.isolate_for_offline(ctx);
        // Reset every node's internal state. The clone inherited the live
        // nodes' filter memory, reverb tails and delay lines as of clone
        // time; rendering from those would make the result depend on *when*
        // the render was started — nondeterministic, and it breaks any
        // cache keyed on "what does this node sound like".
        net.reset();
        Some(net)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_nodes::testing::{Const, Through};

    /// Every [`GraphSource`] arm survives a write and a read-back, on a node
    /// port and on a global output: the translation to the backend's spelling
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
