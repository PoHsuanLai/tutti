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

/// Which graph runtime an [`AudioGraphRes`] runs on.
///
/// Chosen once, when the graph is built — by
/// [`TuttiPlugin::graph_backend`](crate::TuttiPlugin::graph_backend) for an
/// engine, or by [`headless_with`](AudioGraphRes::headless_with) /
/// [`unattached_with`](AudioGraphRes::unattached_with) for a graph with no
/// device — and never switched: the two share no state to switch between.
///
/// Design doc 013, Phase 3 PR 11. Both run the same method surface; where they
/// differ, the method says so. The differences, in one place:
///
/// - **`set_param` on a graph with no audio side.** `Net` applies it to its
///   only copy of the node at once; `Native` sends it through the node's
///   settings ring, and it lands on the next rendered block.
/// - **`inspect`** reads `Net`'s control-side copy; on `Native` it reads the
///   node's shadow (an isolated copy with every setting applied, never
///   processed). Both are construction-time probes, not live state.
/// - **`render_frame`** ticks `Net`'s control-side copy. `Native` has none: it
///   renders the executor while this side still holds it, and panics once
///   the engine or [`take_audio_side`](AudioGraphRes::take_audio_side) has it.
/// - **`replace`** crossfades on `Native` only between units of one latency,
///   and only once the node is committed; otherwise it swaps (`Net` fades
///   regardless).
/// - **Compensation** is inserted as `PdcDelay` nodes on `Net`; on `Native`
///   the compiler compensates every commit and nothing is inserted.
/// - **Export** (`ExportRequest`) is `Net`-only in this release: on `Native`
///   it reports an error (doc 013, PR 12 moves it to `Editor::fork`).
/// - **Block-oriented units** (a convolver's FFT, a plugin's batcher) run in
///   `Legacy`'s 64-frame chunks from the start of each native block, so a
///   render in blocks that are not a multiple of 64 can differ from `Net` by
///   where those chunks fall. Per-sample units render bit-identically.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum GraphBackend {
    /// fundsp's `Net`, as every release before this one.
    #[default]
    Net,
    /// The native `tutti-graph` runtime: an `Editor` on this side, its
    /// `Executor` on the audio thread.
    Native,
}

/// The editable DSP graph. Its methods are the only way to touch it.
///
/// Edit through them, then set [`GraphDirty`](crate::graph::GraphDirty): the
/// per-frame [`commit_graph`](crate::graph::commit_graph) publishes every edit
/// of the frame to the audio thread at once. Nothing here commits inline.
///
/// # Why opaque
///
/// There are two graphs behind this — fundsp's `Net` and the native
/// `tutti-graph` runtime ([`GraphBackend`]). Every method is named in graph
/// terms — insert a node, set a port's source, replace a unit under a fade —
/// and takes an [`AudioNode`] and a [`GraphSource`], never either runtime's
/// type, so both sit behind the same signatures. What only `Net` has (the
/// compensation delays it splices in, the arity-permitting commit) stays
/// inside its arm.
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
// Mutation: `pub struct AudioGraphRes(pub Backend)` (and `pub enum Backend`)
// makes the doctest above compile, which fails it. Everything else in it
// compiles as written, so the privacy of the field is the only thing it can be
// failing on.
#[derive(Resource)]
pub struct AudioGraphRes(Backend);

/// The runtime behind an [`AudioGraphRes`]: two variants, matched per call,
/// control thread only.
#[allow(
    clippy::large_enum_variant,
    reason = "one per app, held in place for its life; boxing buys a pointer hop per call"
)]
enum Backend {
    Net(Net),
    /// Behind a `Mutex` only because a `Resource` must be `Sync` and the
    /// editor is not (it holds boxed nodes and ring ends). Every `&mut self`
    /// method reaches it with `get_mut`, lock-free; a `&self` query takes the
    /// lock, uncontended — the resource's own borrow already serializes access.
    Native(Mutex<NativeGraph>),
}

/// `&self` access to a native graph. Poison is recovered: a panic mid-edit
/// leaves a spec the next commit validates, never a torn audio thread.
fn read(g: &Mutex<NativeGraph>) -> MutexGuard<'_, NativeGraph> {
    g.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `&mut self` access to a native graph: no lock needed.
fn write(g: &mut Mutex<NativeGraph>) -> &mut NativeGraph {
    g.get_mut().unwrap_or_else(PoisonError::into_inner)
}

/// The per-channel pre-roll and the total a compensation pass arrived at,
/// whichever backend computed it — what
/// [`compensate_graph`](crate::graph::latency::compensate_graph) publishes.
pub(crate) struct PdcFigures {
    pub(crate) channels: Vec<Samples>,
    pub(crate) total: Samples,
}

impl From<Compensation> for PdcFigures {
    fn from(c: Compensation) -> Self {
        Self {
            channels: c.channels().to_vec(),
            total: c.total(),
        }
    }
}

impl AudioGraphRes {
    /// A graph with no device behind it: `inputs` global inputs, `outputs`
    /// global outputs, and an audio side taken and dropped so that commits have
    /// somewhere to go. On [`GraphBackend::Net`]; see
    /// [`headless_with`](Self::headless_with).
    ///
    /// For tests and headless tools that drive the reconcile pipeline without
    /// opening a device. [`render_frame`](Self::render_frame) renders it on the
    /// calling thread.
    pub fn headless(inputs: usize, outputs: usize) -> Self {
        Self::headless_with(GraphBackend::Net, inputs, outputs)
    }

    /// [`headless`](Self::headless) on `backend`.
    ///
    /// On [`GraphBackend::Native`] the executor stays on this side: every
    /// commit is applied here, at once, and
    /// [`render_frame`](Self::render_frame) renders it — so a native headless
    /// graph is also what [`unattached_with`](Self::unattached_with) builds
    /// (see [`GraphBackend`]). Prepared at 44.1 kHz, `Net`'s default, until
    /// [`set_sample_rate`](Self::set_sample_rate).
    pub fn headless_with(backend: GraphBackend, inputs: usize, outputs: usize) -> Self {
        let mut graph = Self::unattached_with(backend, inputs, outputs);
        if let Backend::Net(net) = &mut graph.0 {
            drop(net.backend());
        }
        graph
    }

    /// A graph whose audio side has not been taken yet — or, if it never is, a
    /// graph with none at all. On [`GraphBackend::Net`]; see
    /// [`unattached_with`](Self::unattached_with).
    ///
    /// With no audio side, a [`set_param`](Self::set_param) lands on the
    /// control side's copy of the node at once rather than being queued for an
    /// audio thread; a test of the write path that reads a node's state back
    /// wants exactly that. **It cannot [`commit`](crate::graph::commit_graph)**,
    /// so nothing may mark it dirty. Take the audio side with
    /// [`take_audio_side`](Self::take_audio_side) to render what a device would
    /// hear, or use [`headless`](Self::headless).
    pub fn unattached(inputs: usize, outputs: usize) -> Self {
        Self::unattached_with(GraphBackend::Net, inputs, outputs)
    }

    /// [`unattached`](Self::unattached) on `backend`.
    ///
    /// On [`GraphBackend::Native`] the graph *can* commit before its audio
    /// side is taken — the executor is here, and a commit is applied to it at
    /// once — and a `set_param` lands on the next rendered block, not at once:
    /// see [`GraphBackend`].
    pub fn unattached_with(backend: GraphBackend, inputs: usize, outputs: usize) -> Self {
        Self::with_rate(backend, inputs, outputs, SampleRate::DEFAULT)
    }

    /// A graph on `backend`, every node run at `rate`: what the engine
    /// builder makes, at the device's rate.
    pub(crate) fn with_rate(
        backend: GraphBackend,
        inputs: usize,
        outputs: usize,
        rate: SampleRate,
    ) -> Self {
        Self(match backend {
            GraphBackend::Net => {
                let mut net = Net::new(inputs, outputs);
                net.set_sample_rate(rate);
                Backend::Net(net)
            }
            GraphBackend::Native => {
                Backend::Native(Mutex::new(NativeGraph::new(inputs, outputs, rate)))
            }
        })
    }

    /// Which runtime this graph runs on.
    pub fn backend(&self) -> GraphBackend {
        match &self.0 {
            Backend::Net(_) => GraphBackend::Net,
            Backend::Native(_) => GraphBackend::Native,
        }
    }

    /// The audio thread's half, for a test that renders what a device would
    /// hear: every commit lands on it, and ticking it plays the graph.
    ///
    /// # Panics
    ///
    /// If the audio side was already taken — by an earlier call, by
    /// [`headless`](Self::headless) on `Net`, or by the engine builder.
    pub fn take_audio_side(&mut self) -> AudioSide {
        match &mut self.0 {
            Backend::Net(net) => AudioSide::net(net.backend()),
            Backend::Native(g) => AudioSide::native(write(g).take_executor()),
        }
    }

    /// Re-rate every node in the graph, and every node inserted after.
    ///
    /// For a [`headless`](Self::headless) graph, which has no device to take a
    /// rate from. An engine-built graph already runs at the device's rate
    /// ([`AudioConfig::sample_rate`]). On [`GraphBackend::Native`] this is an
    /// `Editor::reprepare`: with the audio side still here it completes before
    /// this returns; with it taken, commits wait for its second half.
    pub fn set_sample_rate(&mut self, rate: SampleRate) {
        match &mut self.0 {
            Backend::Net(net) => net.set_sample_rate(rate),
            Backend::Native(g) => write(g).set_sample_rate(rate),
        }
    }

    /// Refuse, before a device restart stops the stream, a `max_block` the
    /// graph cannot be re-prepared to: past the engine's block capacity, or
    /// while a re-prepare is already between its halves, or on a poisoned
    /// graph (`Native`). `Net` renders any block and takes any.
    pub(crate) fn check_rerate(
        &self,
        max_block: Option<Samples>,
    ) -> Result<(), tutti_graph::CommitError> {
        match &self.0 {
            Backend::Net(_) => Ok(()),
            Backend::Native(g) => read(g).check_reprepare(max_block),
        }
    }

    /// Move a live graph to the device's new `rate` (and, on `Native`, a new
    /// `max_block`): what a device restart does between the stop and the
    /// start ([`restart_device`](crate::engine::restart_device)).
    ///
    /// - `Net`: [`Net::set_sample_rate`], which re-rates every unit on the
    ///   control side and marks it changed, so the next commit swaps the
    ///   *control side's* copies in — the beat clock's among them, which has
    ///   never run. So the clock is re-seated too: a seek to the live
    ///   playhead, taken on its first block (unless a seek is already
    ///   pending, which it takes instead). With no callback running the
    ///   playhead is exact, and the beat carries on across the restart.
    ///   Every other unit restarts from its control-side state, as a
    ///   re-prepare restarts everything time-based. `max_block` is ignored:
    ///   a `Net` renders any block.
    /// - `Native`: the first half of `Editor::reprepare`
    ///   ([`NativeGraph::reprepare`]); the executor adopts it on its next
    ///   blocks, the engine following the rate on the first of them.
    ///
    /// Not committed here; see [`restart_device`](crate::engine::restart_device).
    pub(crate) fn rerate(
        &mut self,
        rate: SampleRate,
        max_block: Option<Samples>,
        transport: &tutti_core::Transport,
    ) -> Result<(), tutti_graph::CommitError> {
        match &mut self.0 {
            Backend::Net(net) => {
                if SampleRate(net.sample_rate()) != rate {
                    net.set_sample_rate(rate);
                    if !transport.motion.seek.is_pending() {
                        transport.motion.seek.request(transport.settings.beat());
                    }
                }
                Ok(())
            }
            Backend::Native(g) => write(g).reprepare(rate, max_block),
        }
    }

    /// Whether a native re-prepare is between its two commits. Never on
    /// `Net`.
    pub(crate) fn is_repreparing(&self) -> bool {
        match &self.0 {
            Backend::Net(_) => false,
            Backend::Native(g) => read(g).is_repreparing(),
        }
    }

    /// Build the engine over this graph's audio side, which it takes.
    ///
    /// `Net`: `Engine::new` over its backend. `Native`: `Engine::with_graph`
    /// over the editor and its executor, which bounds the editor to what the
    /// engine can render (a commit past it is refused, and logged).
    pub(crate) fn engine(
        &mut self,
        transport: &tutti_core::Transport,
    ) -> Result<tutti_core::Engine, tutti_core::GraphEngineError> {
        match &mut self.0 {
            Backend::Net(net) => Ok(tutti_core::Engine::new(
                transport.motion.clone(),
                net.backend(),
            )),
            Backend::Native(g) => {
                let exec = write(g).take_executor();
                tutti_core::Engine::with_graph(transport, write(g).editor_mut(), exec)
            }
        }
    }

    /// The node the engine's beat ports come from: a `TransportClock` over
    /// `transport` on `Net`, an [`EnvClock`](tutti_core::EnvClock) on
    /// `Native` — a graph engine drives its own `TransportClock` and forbids a
    /// second in the graph, and an `EnvClock` emits the same samples from each
    /// block's `Env` (doc 013, Phase 3 gap 5).
    pub(crate) fn insert_beat_clock(&mut self, transport: &tutti_core::Transport) -> AudioNode {
        match &mut self.0 {
            Backend::Net(net) => {
                let rate = SampleRate(net.sample_rate());
                AudioNode(net.add(tutti_core::TransportClock::new(
                    transport.clock_links(),
                    rate,
                )))
            }
            Backend::Native(g) => write(g).insert_env_clock(),
        }
    }

    // --- Nodes ---

    /// Add `unit` to the graph, unwired, and return its handle.
    ///
    /// Binding the handle to an entity is the caller's (see
    /// [`CapturedControls::bind`](crate::graph::CapturedControls::bind));
    /// [`spawn_audio_node`](crate::graph::SpawnAudioNode) does both.
    ///
    /// The unit is re-rated to the graph's rate, on either backend.
    pub fn insert<U: AudioUnit + 'static>(&mut self, unit: U) -> AudioNode {
        self.insert_boxed(Box::new(unit))
    }

    /// [`insert`](Self::insert) for a unit that is already boxed (a plugin, a
    /// trait-object factory's product).
    pub fn insert_boxed(&mut self, unit: Box<dyn AudioUnit>) -> AudioNode {
        match &mut self.0 {
            Backend::Net(net) => AudioNode(net.push(unit)),
            Backend::Native(g) => write(g).insert(unit),
        }
    }

    /// Take `node` out of the graph. Every edge to and from it reads silence
    /// afterwards, so a sink still naming it is left silent rather than
    /// dangling.
    ///
    /// Returns whether it was there; removing a node twice is a no-op.
    pub fn remove(&mut self, node: AudioNode) -> bool {
        match &mut self.0 {
            Backend::Net(net) => {
                if !net.contains(node.0) {
                    return false;
                }
                drop(net.remove(node.0));
                true
            }
            Backend::Native(g) => write(g).remove(node),
        }
    }

    /// Whether `node` is in the graph.
    pub fn contains(&self, node: AudioNode) -> bool {
        match &self.0 {
            Backend::Net(net) => net.contains(node.0),
            Backend::Native(g) => read(g).contains(node),
        }
    }

    /// Swap the unit behind `node` for `unit`, fading from one to the other over
    /// `fade`. `node` keeps its handle and every edge to and from it.
    ///
    /// `unit` must have `node`'s input and output counts: this replaces a unit,
    /// not a node's shape. A shape change is a remove and an insert.
    ///
    /// On [`GraphBackend::Native`] the fade needs a running unit of the same
    /// latency to fade from; without one (the node not committed yet, or a
    /// latency change) the new unit lands as a plain swap on the next commit.
    /// And it can be refused: while the graph re-prepares, with the unit
    /// handed back ([`ReplaceRefused::Busy`], retry after the re-prepare
    /// resumes), and for good on a poisoned graph. Swap the captured controls
    /// only on `Ok` — [`crossfade_audio_node`](crate::graph::crossfade_audio_node)
    /// does all of this.
    pub fn replace(
        &mut self,
        node: AudioNode,
        unit: Box<dyn AudioUnit>,
        fade: Seconds,
        curve: CrossfadeCurve,
    ) -> Result<(), ReplaceRefused> {
        match &mut self.0 {
            Backend::Net(net) => {
                net.crossfade(node.0, tutti_core::net_fade(curve), fade.get(), unit);
                Ok(())
            }
            Backend::Native(g) => write(g).replace(node, unit, fade, curve),
        }
    }

    /// Write `value` to `node`'s scalar param `param`. Delivered to the audio
    /// thread through the graph's settings queue, not by mutating the node.
    ///
    /// On [`GraphBackend::Native`] the queue is the node's own settings ring,
    /// drained at the start of its next block — on every graph, including one
    /// whose audio side has not been taken (see [`GraphBackend`]).
    pub fn set_param(&mut self, node: AudioNode, param: UnitParam, value: f32) {
        match &mut self.0 {
            Backend::Net(net) => {
                net.set(tutti_core::unit_param::node_setting(node.0, param, value))
            }
            Backend::Native(g) => write(g).set_param(node, param, value),
        }
    }

    /// Run `f` on the unit behind `node`, for inspection: `None` if `node` is
    /// not in the graph.
    ///
    /// The control side's copy — never the one the audio thread runs — so it is
    /// for tests and diagnostics that probe a unit's construction (its ports, a
    /// LUT baked in when it was built), not a way to reach live state. A host
    /// that drives a node keeps the handles it captured at insertion; see
    /// [`capture`](crate::graph::capture).
    ///
    /// On [`GraphBackend::Native`] the copy is the node's shadow: isolated when
    /// the unit was inserted, with every [`set_param`](Self::set_param) applied
    /// since. `None` for the engine's beat generator, which is not an
    /// `AudioUnit` there.
    pub fn inspect<R>(&self, node: AudioNode, f: impl FnOnce(&dyn AudioUnit) -> R) -> Option<R> {
        match &self.0 {
            Backend::Net(net) => net.contains(node.0).then(|| f(net.node(node.0))),
            Backend::Native(g) => read(g).inspect(node, f),
        }
    }

    // --- Edges ---

    /// What feeds `node`'s input `port`.
    pub fn source(&self, node: AudioNode, port: usize) -> GraphSource {
        match &self.0 {
            Backend::Net(net) => GraphSource::lift(net.source(node.0, port)),
            Backend::Native(g) => read(g).source(node, port),
        }
    }

    /// Feed `node`'s input `port` from `source`. A port holds one source; this
    /// replaces whatever it held.
    ///
    /// # Panics
    ///
    /// If `source` is `node` itself: a node cannot feed its own input.
    pub fn set_source(&mut self, node: AudioNode, port: usize, source: GraphSource) {
        match &mut self.0 {
            Backend::Net(net) => net.set_source(node.0, port, source.lower()),
            Backend::Native(g) => write(g).set_source(node, port, source),
        }
    }

    /// What feeds global output `channel`.
    pub fn output_source(&self, channel: usize) -> GraphSource {
        match &self.0 {
            Backend::Net(net) => GraphSource::lift(net.output_source(channel)),
            Backend::Native(g) => read(g).output_source(channel),
        }
    }

    /// Feed global output `channel` from `source`.
    pub fn set_output_source(&mut self, channel: usize, source: GraphSource) {
        match &mut self.0 {
            Backend::Net(net) => net.set_output_source(channel, source.lower()),
            Backend::Native(g) => write(g).set_output_source(channel, source),
        }
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
        match &mut self.0 {
            Backend::Net(net) => net.pipe_output(node.0),
            Backend::Native(g) => write(g).set_outputs_from(node),
        }
    }

    // --- Shape ---

    /// The graph's global input count.
    pub fn inputs(&self) -> usize {
        match &self.0 {
            Backend::Net(net) => net.inputs(),
            Backend::Native(g) => read(g).inputs(),
        }
    }

    /// The graph's global output count.
    pub fn outputs(&self) -> usize {
        match &self.0 {
            Backend::Net(net) => net.outputs(),
            Backend::Native(g) => read(g).outputs(),
        }
    }

    /// `node`'s input port count.
    pub fn node_inputs(&self, node: AudioNode) -> usize {
        match &self.0 {
            Backend::Net(net) => net.inputs_in(node.0),
            Backend::Native(g) => read(g).node_inputs(node),
        }
    }

    /// `node`'s output port count.
    pub fn node_outputs(&self, node: AudioNode) -> usize {
        match &self.0 {
            Backend::Net(net) => net.outputs_in(node.0),
            Backend::Native(g) => read(g).node_outputs(node),
        }
    }

    /// The latency `node` reports.
    ///
    /// On `Net`, through its own `LatencyGraph` impl rather than the unit: the
    /// `AudioUnit` method takes `&mut self`, and that impl is the one place the
    /// clone-to-probe is written down. [`node_tail`](Self::node_tail) likewise.
    /// On `Native`, the shape the editor holds: probed when the unit was
    /// inserted (at the graph's rate, by `Legacy`'s rounding, which is
    /// `Net`'s), and moved since only by a hosted plugin's latency change.
    pub fn node_latency(&self, node: AudioNode) -> Samples {
        match &self.0 {
            Backend::Net(net) => tutti_core::LatencyGraph::latency(net, node.0),
            Backend::Native(g) => read(g).node_latency(node),
        }
    }

    /// The tail `node` reports.
    pub fn node_tail(&self, node: AudioNode) -> Tail {
        match &self.0 {
            Backend::Net(net) => tutti_core::TailGraph::tail(net, node.0),
            Backend::Native(g) => read(g).node_tail(node),
        }
    }

    /// Widen the global outputs to `channels`. New channels read silence.
    ///
    /// Global outputs are sinks, so changing their count cannot dangle a
    /// reference; [`commit`](Self::commit) is the commit that accepts it.
    pub(crate) fn widen_outputs(&mut self, channels: usize) {
        match &mut self.0 {
            Backend::Net(net) => net.set_output_arity_live(channels),
            Backend::Native(g) => write(g).widen_outputs(channels),
        }
    }

    // --- Latency ---

    /// What compensation this graph would need, without applying any: the
    /// per-output-channel pre-roll and the total. Mutates nothing, so a latency
    /// readout can call it freely.
    ///
    /// Over the graph as it stands — after
    /// [`LatencyCompensationPlugin`](crate::graph::latency::LatencyCompensationPlugin)
    /// has run on `Net`, that includes the delays it applied, so the figure is
    /// what is *still* misaligned. `Native` inserts no delays (its compiler
    /// aligns every path), so there it is always the authored graph's figure —
    /// the one its plans compensate.
    pub fn latency_plan(&self) -> Compensation {
        match &self.0 {
            Backend::Net(net) => tutti_core::latency::plan(net),
            Backend::Native(g) => read(g).latency_plan(),
        }
    }

    /// Align every path, and return the figures.
    ///
    /// `Net`: re-plan compensation over the authored graph and splice in the
    /// delays. `Native`: nothing to splice — the compiler compensates every
    /// commit — so this compiles the spec as the frame's commit will and
    /// reads the plan's `compensation()` and `total_latency()`. `None` when
    /// the spec does not compile (the commit reports why).
    pub(crate) fn compensate(&mut self) -> Option<PdcFigures> {
        match &mut self.0 {
            Backend::Net(net) => Some(tutti_core::latency::compensate(net).into()),
            Backend::Native(g) => write(g)
                .planned_compensation()
                .map(|(channels, total)| PdcFigures { channels, total }),
        }
    }

    /// The compensation of the plan last sent to the audio thread, on
    /// `Native`; `None` on `Net`, which has no plan.
    #[cfg(test)]
    pub(crate) fn sent_compensation(&self) -> Option<PdcFigures> {
        match &self.0 {
            Backend::Net(_) => None,
            Backend::Native(g) => read(g)
                .sent_compensation()
                .map(|(channels, total)| PdcFigures { channels, total }),
        }
    }

    /// A live duplicate of the whole graph that shares no state with it,
    /// rendered through its own [`AudioSide`]: `Editor::fork` on `Native`,
    /// each node forked from its shadow. What an export will render from once
    /// it moves to `Editor::fork` (doc 013, PR 12), so a test can check today
    /// that a control write reaches it. `None` on `Net` (whose export clones
    /// the `Net`), or when a node cannot be forked.
    #[cfg(test)]
    pub(crate) fn fork(&self) -> Option<AudioSide> {
        match &self.0 {
            Backend::Net(_) => None,
            Backend::Native(g) => read(g).fork().ok(),
        }
    }

    /// Record the value a fork of `node` starts `param` at, without touching
    /// the live unit. For a param the modulation driver owns: live, the driver
    /// writes `base + Σ layers` into the node's cell every frame; a fork is
    /// not driven by it and must carry the authored base.
    ///
    /// `Net` needs nothing: its export clones the `Net`, whose nodes share the
    /// live cells. `Native`: the node's shadow, which its forks clone.
    #[cfg(feature = "modulation")]
    pub(crate) fn set_param_snapshot(&mut self, node: AudioNode, param: UnitParam, value: f32) {
        if let Backend::Native(g) = &mut self.0 {
            write(g).set_param_snapshot(node, param, value);
        }
    }

    /// `node`'s latency may have moved at runtime — a hosted plugin's latency
    /// cell changed.
    ///
    /// `Net` needs nothing: it re-probes the unit (a clone sharing the cell)
    /// on every compensation pass. `Native` holds the latency it probed at
    /// insert, so it probes the node's shadow again and, if the figure moved,
    /// hands it to `Editor::set_latency`, which moves PDC on the next commit
    /// without touching the unit.
    #[cfg(feature = "plugin")]
    pub(crate) fn refresh_node_latency(&mut self, node: AudioNode) {
        if let Backend::Native(g) = &mut self.0 {
            write(g).refresh_node_latency(node);
        }
    }

    /// Whether any compensation delays [`compensate`](Self::compensate) applied
    /// are in the graph. Never on `Native`, which inserts none.
    pub(crate) fn has_compensation(&self) -> bool {
        match &self.0 {
            Backend::Net(net) => net
                .ids()
                .any(|&id| net.node(id).get_id() == tutti_core::PDC_DELAY_ID),
            Backend::Native(_) => false,
        }
    }

    /// Whether `source` reads from a compensation delay rather than from an
    /// authored node — a node with no entity, so no declaration names it.
    ///
    /// Identified by `AudioUnit::get_id`, which is how
    /// `DelayInsertion::clear_delays` finds them too — one marker, one
    /// definition of "this node is derived, not authored". Never on `Native`.
    pub(crate) fn is_compensation(&self, source: GraphSource) -> bool {
        let (Backend::Net(net), GraphSource::Node(node, _)) = (&self.0, source) else {
            return false;
        };
        net.contains(node.0) && net.node(node.0).get_id() == tutti_core::PDC_DELAY_ID
    }

    // --- Publishing and rendering ---

    /// Publish every edit since the last commit to the audio thread. Returns
    /// whether it is done with them: `false` means "retry next frame", and the
    /// caller keeps [`GraphDirty`](crate::graph::GraphDirty) set.
    ///
    /// `Net`: accepts a changed global output count (see
    /// [`widen_outputs`](Self::widen_outputs)) — `tutti_core`'s
    /// `Engine::process_segment` re-reads the backend's output count every
    /// block — and is identical to a plain commit when it is unchanged; always
    /// done. `Native`: `Editor::commit`, which is refused for now while
    /// `QUEUE_CAPACITY` commits are out or a re-prepare is between its halves
    /// (the retry case), and for good when the graph does not compile (logged;
    /// the next edit tries again).
    pub(crate) fn commit(&mut self) -> bool {
        match &mut self.0 {
            Backend::Net(net) => {
                net.commit_output_arity_change();
                true
            }
            Backend::Native(g) => write(g).commit() == Committed::Done,
        }
    }

    /// Drain what the audio thread sent back, freeing it here, and flush any
    /// settings a full ring held. `Native` only (a `Net` frees on commit); on
    /// the main thread, every frame, from
    /// [`commit_graph`](crate::graph::commit_graph).
    pub(crate) fn collect(&mut self) {
        if let Backend::Native(g) = &mut self.0 {
            write(g).collect();
        }
    }

    /// Render one frame on the calling thread: no inputs, `output` sized to
    /// [`outputs`](Self::outputs).
    ///
    /// For a [`headless`](Self::headless) graph, which has no audio thread to
    /// hear it. On `Net`, on an engine-built graph it runs the control side's
    /// copy of each node, which the audio thread never hears either, and
    /// advances their state. On `Native` it renders the executor while this
    /// side holds it, committing any edit first (as `Net`'s tick renders the
    /// graph as edited), under a stopped transport.
    ///
    /// # Panics
    ///
    /// On [`GraphBackend::Native`], once the audio side was taken: it has no
    /// control-side copy to render instead.
    pub fn render_frame(&mut self, output: &mut [f32]) {
        match &mut self.0 {
            Backend::Net(net) => net.tick(&[], output),
            Backend::Native(g) => write(g).render_frame(output),
        }
    }

    // --- Export ---

    /// The whole graph as an offline copy, keeping its live transport
    /// bindings. See `export::run::prepare_net`. `Net` only in this release.
    #[cfg(feature = "export")]
    pub(crate) fn export_master(&self) -> Result<Net, &'static str> {
        match &self.0 {
            Backend::Net(net) => Ok(net.clone()),
            Backend::Native(_) => Err(EXPORT_NOT_NATIVE),
        }
    }

    /// An offline copy rendering `node`'s outputs, isolated from live inputs,
    /// rebound onto `ctx` and reset. `Ok(None)` when `node` has no outputs.
    /// `Net` only in this release.
    #[cfg(feature = "export")]
    pub(crate) fn export_node(
        &self,
        node: AudioNode,
        ctx: &OfflineTransport,
    ) -> Result<Option<Net>, &'static str> {
        let Backend::Net(net) = &self.0 else {
            return Err(EXPORT_NOT_NATIVE);
        };
        let Some(pending) = net.clone_isolated(node.0) else {
            return Ok(None);
        };
        // Isolate (sever live inputs) and rebind (re-point at `ctx`) in the
        // one order they may happen — see `PendingClone::isolate_for_offline`.
        let mut net = pending.isolate_for_offline(ctx);
        // Reset every node's internal state. The clone inherited the live
        // nodes' filter memory, reverb tails and delay lines as of clone
        // time; rendering from those would make the result depend on *when*
        // the render was started — nondeterministic, and it breaks any
        // cache keyed on "what does this node sound like".
        net.reset();
        Ok(Some(net))
    }
}

/// Why an export on the native backend fails in this release.
///
/// Export renders a `Net` today, and the native graph has none to hand out.
/// Doc 013's PR 12 moves export to `Editor::fork`; until then the export is
/// refused explicitly rather than rendered from something that is not the
/// live graph.
#[cfg(feature = "export")]
pub(crate) const EXPORT_NOT_NATIVE: &str =
    "export is not yet available on GraphBackend::Native (design doc 013, PR 12); \
     build the graph on GraphBackend::Net to export";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::both_backends;
    use tutti_nodes::testing::{Const, Through};

    /// Every [`GraphSource`] arm survives a write and a read-back, on a node
    /// port and on a global output: the translation to the backend's spelling
    /// and back is total and lossless.
    ///
    /// Mutation: lowering `Input(port)` to `Zero` (or lifting `Global` to
    /// `Silence`) fails the `Input` row; lowering `Node` with a fixed port 0
    /// fails the port-1 row.
    fn every_source_reads_back_as_written(backend: GraphBackend) {
        let mut graph = AudioGraphRes::headless_with(backend, 1, 2);
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
    both_backends!(every_source_reads_back_as_written);

    /// `remove` says whether the node was there, so removing a node twice is a
    /// no-op rather than a panic.
    ///
    /// Mutation: dropping the `contains` guard panics on the second call.
    fn removing_a_node_twice_is_a_no_op(backend: GraphBackend) {
        let mut graph = AudioGraphRes::headless_with(backend, 0, 1);
        let node = graph.insert(Const::mono(1.0));
        assert!(graph.contains(node));
        assert!(graph.remove(node));
        assert!(!graph.contains(node));
        assert!(!graph.remove(node));
    }
    both_backends!(removing_a_node_twice_is_a_no_op);
}
