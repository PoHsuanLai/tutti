//! The native backend behind [`AudioGraphRes`](super::AudioGraphRes): a
//! `tutti-graph` [`Editor`], and — until the engine or a test takes it — the
//! [`Executor`] it sends to.
//!
//! Design doc 013, Phase 3 PR 11. Selected with
//! [`GraphBackend::Native`](super::GraphBackend::Native); the default is still
//! fundsp's `Net`. Every method here answers one of `AudioGraphRes`'s, and
//! the mapping is:
//!
//! | `AudioGraphRes` | here |
//! |---|---|
//! | `insert` | `Legacy::controlled` at a [`NodeKey`] minted from a fresh `NodeId` |
//! | `set_source`, `set_output_source`, `widen_outputs` | written into `editor.spec_mut()` |
//! | `set_param` | the node's `LegacyControls` (its settings ring, and its shadow) |
//! | `replace` | `Editor::replace` with a [`Fade`]; a plain `insert` when there is nothing to fade from |
//! | `node_latency`, `node_tail`, `node_inputs`, `node_outputs` | the editor's [`Shapes`](tutti_graph::Shapes) |
//! | `latency_plan` | `tutti_types::latency::plan` over the spec's topology |
//! | `compensate` | compiles the spec, reads `Plan::compensation` / `total_latency`; inserts nothing |
//! | `commit` | `Editor::commit`, which collects first |
//! | `set_node_latency` | `Editor::set_latency` |
//!
//! # Every unit is `controlled`, none `pure`
//!
//! `insert` takes any `AudioUnit`, and nothing about one says whether its
//! output is a function of its audio inputs alone — a SoundFont fed through
//! its MIDI port looks exactly like a filter from here. A `pure` claim that is
//! wrong parks the unit for good the first time it is quiet (`tutti_graph`'s
//! `legacy` module docs), so the backend never makes one: every unit is
//! [`Legacy::controlled`], which runs every block and gives `set_param` its
//! settings ring. The price is the silence skip, which `Net` never had either.
//!
//! # Where the audio side lives
//!
//! `Editor::new` builds the pair. The executor stays here ("local") until
//! [`take_executor`](NativeGraph::take_executor) hands it to the engine or to
//! a test's [`AudioSide`](super::AudioSide). While it is local, a commit is
//! applied at once on this thread and its box collected, so a headless graph
//! never meets back-pressure, and [`render_frame`](NativeGraph::render_frame)
//! runs it. So on this backend `headless` and `unattached` build the same
//! thing; the difference `Net` has between them (whether `set_param` reaches a
//! control-side copy of the node) has no counterpart, because there is no
//! control-side copy.
//!
//! # `set_param` lands on the next block, never at once
//!
//! A `Net` with no audio side applies a setting straight to its only copy of
//! the node, so a test could read an atomic back the moment it wrote it. Here
//! a setting goes into the node's ring and reaches the unit at the start of
//! the executor's next block: one block later, on every graph. A test that
//! reads a unit's live state after a write renders a frame first
//! ([`render_frame`](super::AudioGraphRes::render_frame)); one that reads the
//! by-value state reads the shadow through
//! [`inspect`](super::AudioGraphRes::inspect), which every setting reaches at
//! once.

use std::collections::BTreeMap;

use tutti_core::dsp::NodeId;
use tutti_core::{
    AudioNode, AudioUnit, BufferMut, BufferRef, Compensation, CrossfadeCurve, EnvClock, Samples,
    Tail,
};
use tutti_graph::{
    CommitError, Editor, Executor, Fade, Legacy, LegacyControls, Prepare, Resolution, Transport,
};
use tutti_node::{AttoHash, Setting, SignalFrame};
use tutti_types::graph::{Edge, InPort, NodeKey, OutPort, Source};
use tutti_types::{ChannelLayout, Latency, SampleRate, Seconds, UnitParam};

use super::resources::GraphSource;

/// The largest block the native graph is prepared for.
///
/// A device block longer than this is rendered by `tutti_core::Engine` as
/// consecutive graph blocks of at most this many frames, so it bounds the
/// arena, not the device. 1024 frames covers every buffer size a DAW offers
/// by default; `Legacy` still runs each unit in 64-frame chunks inside it.
pub(crate) const NATIVE_MAX_BLOCK: Samples = Samples(1024);

/// The spec `kind` of a unit this backend inserted. Nothing builds a unit from
/// it (the same reason `topology::ENTITY_NODE_KIND` exists); it names the
/// layer in a debugger.
const UNIT_KIND: &str = "bevy-tutti:unit";

/// The spec `kind` of the engine's beat generator on this backend.
const ENV_CLOCK_KIND: &str = "bevy-tutti:env-clock";

/// A boxed unit as a sized, `Clone` one, which is what `Legacy::controlled`
/// takes (its shadow is a clone). Every method forwards, the defaulted ones
/// included — a forwarding wrapper that let `isolate`, `forkable` or
/// `latency` fall back to the trait default would silently change what the
/// graph believes about the unit (a plugin would become forkable, a limiter
/// latency-free). `as_any` forwards too, so a downcast through
/// [`inspect`](super::AudioGraphRes::inspect) sees the unit, not this.
#[derive(Clone)]
pub(crate) struct Boxed(pub(crate) Box<dyn AudioUnit>);

impl AudioUnit for Boxed {
    fn reset(&mut self) {
        self.0.reset();
    }
    fn isolate(&mut self) {
        self.0.isolate();
    }
    fn rebind_offline(&mut self, ctx: &dyn core::any::Any) {
        self.0.rebind_offline(ctx);
    }
    fn forkable(&self) -> bool {
        self.0.forkable()
    }
    fn set_sample_rate(&mut self, sample_rate: SampleRate) {
        self.0.set_sample_rate(sample_rate);
    }
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        self.0.tick(input, output);
    }
    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        self.0.process(size, input, output);
    }
    fn set(&mut self, setting: Setting) {
        self.0.set(setting);
    }
    fn inputs(&self) -> usize {
        self.0.inputs()
    }
    fn outputs(&self) -> usize {
        self.0.outputs()
    }
    fn route(&mut self, input: &SignalFrame, frequency: f64) -> SignalFrame {
        self.0.route(input, frequency)
    }
    fn get_id(&self) -> u64 {
        self.0.get_id()
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self.0.as_any()
    }
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self.0.as_any_mut()
    }
    fn set_hash(&mut self, hash: u64) {
        self.0.set_hash(hash);
    }
    fn ping(&mut self, probe: bool, hash: AttoHash) -> AttoHash {
        self.0.ping(probe, hash)
    }
    fn footprint(&self) -> usize {
        self.0.footprint()
    }
    fn allocate(&mut self) {
        self.0.allocate();
    }
    fn latency(&mut self) -> Option<f64> {
        self.0.latency()
    }
    fn tail(&mut self) -> Tail {
        self.0.tail()
    }
}

/// One node's handle, and its controls when it has a settings path.
struct Entry {
    node: AudioNode,
    /// `None` for a native node (the beat generator), which takes no
    /// settings and has no `AudioUnit` to inspect.
    controls: Option<LegacyControls<Boxed>>,
}

/// The executor, while this side still holds it, and what a local render
/// needs beside it.
pub(crate) struct Local {
    exec: Executor,
    /// Planar scratch, one `Vec` per global output.
    scratch: Vec<Vec<f32>>,
}

/// The native backend. See the module docs.
pub(crate) struct NativeGraph {
    editor: Editor,
    local: Option<Local>,
    nodes: BTreeMap<NodeKey, Entry>,
    /// Edited since the last commit — only a local render reads it, to
    /// commit before it renders (a `Net`'s control-side tick renders the
    /// graph as edited, uncommitted edits included).
    edited: bool,
}

/// What a commit came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Committed {
    /// Sent, or refused for good (logged): either way, nothing to retry.
    Done,
    /// Refused for now — the executor has not drained enough commits, or a
    /// re-prepare is between its halves. Keep `GraphDirty` and retry next
    /// frame.
    Retry,
}

/// `node`'s key: its `NodeId`'s bits. `NodeId::new` draws from a global
/// counter, so a key minted this way is unique without a map, and
/// [`AudioNode`] keeps wrapping a `NodeId` on both backends.
pub(crate) fn key(node: AudioNode) -> NodeKey {
    NodeKey(node.0.value())
}

/// The latency `Legacy` will declare for `unit` at `rate`, by `Legacy`'s own
/// probe (`tutti-graph/src/legacy.rs`, `Adapter::probe`): `latency()` after
/// `set_sample_rate`, rounded to the nearest frame, never negative. Asked
/// before a replace, because a refused `Editor::replace` consumes its node.
fn probe_latency(unit: &mut dyn AudioUnit, rate: SampleRate) -> Latency {
    unit.set_sample_rate(rate);
    Latency::new(Samples(
        unit.latency().unwrap_or(0.0).round().max(0.0) as usize
    ))
}

impl NativeGraph {
    /// An empty graph with `inputs` global inputs and `outputs` global
    /// outputs, prepared for `rate`, its executor local.
    pub(crate) fn new(inputs: usize, outputs: usize, rate: SampleRate) -> Self {
        let (mut editor, exec) = Editor::new(Prepare::new(rate, NATIVE_MAX_BLOCK));
        let topology = &mut editor.spec_mut().topology;
        topology.inputs = ChannelLayout::from_count(inputs as u16);
        topology.outputs = vec![Source::Zero; outputs];
        Self {
            editor,
            local: Some(Local {
                exec,
                scratch: Vec::new(),
            }),
            nodes: BTreeMap::new(),
            edited: true,
        }
    }

    /// Hand the executor over — to the engine, or to a test's audio side.
    ///
    /// # Panics
    ///
    /// If it was already taken.
    pub(crate) fn take_executor(&mut self) -> Executor {
        self.local
            .take()
            .expect("the native graph's audio side was already taken")
            .exec
    }

    /// The editor, for `Engine::with_graph`.
    pub(crate) fn editor_mut(&mut self) -> &mut Editor {
        &mut self.editor
    }

    /// Re-prepare every node for `rate`. With the executor local, both halves
    /// of the re-prepare run here, now; with it taken, the second half lands
    /// on a later `collect` (and commits are `Retry` until it does).
    pub(crate) fn set_sample_rate(&mut self, rate: SampleRate) {
        if let Err(e) = self.editor.reprepare(Prepare::new(
            rate,
            self.editor.prepare().max_block().samples(),
        )) {
            bevy_log::error!("native graph: re-prepare at {} Hz refused: {e}", rate.get());
            return;
        }
        self.pump_local();
    }

    /// Apply whatever the editor sent to a local executor, and collect what
    /// comes back, until nothing is in flight. A no-op once the executor is
    /// taken. Bounded: a re-prepare is two round trips, anything else one.
    fn pump_local(&mut self) {
        let Some(local) = &mut self.local else {
            return;
        };
        for _ in 0..4 {
            if self.editor.in_flight() == 0 {
                break;
            }
            local.exec.apply_pending();
            self.editor.collect();
        }
    }

    // --- Nodes ---

    pub(crate) fn insert(&mut self, unit: Box<dyn AudioUnit>) -> AudioNode {
        let node = AudioNode(NodeId::new());
        let (legacy, controls) = Legacy::controlled(&mut self.editor, Boxed(unit));
        self.editor.insert(key(node), UNIT_KIND, legacy);
        self.nodes.insert(
            key(node),
            Entry {
                node,
                controls: Some(controls),
            },
        );
        self.edited = true;
        node
    }

    /// The beat generator a graph engine needs in place of a
    /// `TransportClock` (`Engine::with_graph` forbids one in the graph).
    pub(crate) fn insert_env_clock(&mut self) -> AudioNode {
        let node = AudioNode(NodeId::new());
        self.editor
            .insert(key(node), ENV_CLOCK_KIND, EnvClock::new());
        self.nodes.insert(
            key(node),
            Entry {
                node,
                controls: None,
            },
        );
        self.edited = true;
        node
    }

    pub(crate) fn remove(&mut self, node: AudioNode) -> bool {
        if self.nodes.remove(&key(node)).is_none() {
            return false;
        }
        self.editor.remove(key(node));
        self.edited = true;
        true
    }

    pub(crate) fn contains(&self, node: AudioNode) -> bool {
        self.nodes.contains_key(&key(node))
    }

    /// Swap the unit at `node`, crossfading when the running unit can fade to
    /// it; otherwise a plain swap.
    ///
    /// `Editor::replace` fades only between units of one shape (ports,
    /// latency, in-place acceptance, event resolution; doc 013, PR 3) and
    /// only from a unit that is running. `Net::crossfade` asks neither. So
    /// where the fade cannot be had — the node is not committed yet, or the
    /// new unit declares another latency — this lands the unit with
    /// `Editor::insert` instead: the same key, every edge kept, heard as a
    /// swap on the commit's block. Checked here rather than by trying,
    /// because a refused `replace` has already consumed the unit.
    pub(crate) fn replace(
        &mut self,
        node: AudioNode,
        mut unit: Box<dyn AudioUnit>,
        fade: Seconds,
        curve: CrossfadeCurve,
    ) {
        let k = key(node);
        let Some(entry) = self.nodes.get(&k) else {
            bevy_log::warn!("native graph: replace names {node:?}, which is not in the graph");
            return;
        };
        let rate = self.editor.prepare().sample_rate();
        let running = self
            .editor
            .base()
            .and_then(|plan| plan.unit(k))
            .map(|u| u.shape)
            .filter(|_| self.editor.spec().topology.nodes.contains_key(&k))
            // A native node at the key (the beat generator) is not what a
            // `Legacy` can fade from: its in-place and resolution differ.
            .filter(|_| entry.controls.is_some());
        let fits = running.is_some_and(|s| {
            s.audio_in.count() as usize == unit.inputs()
                && s.audio_out.count() as usize == unit.outputs()
                && s.event_in == 0
                && s.event_out == 0
                && s.in_place
                && s.event_resolution == Resolution::Block
                && s.latency == probe_latency(unit.as_mut(), rate)
        });
        let (legacy, controls) = Legacy::controlled(&mut self.editor, Boxed(unit));
        if fits {
            let fade = Fade::seconds(fade, rate, curve);
            if let Err(e) = self.editor.replace(k, legacy, fade) {
                // Checked above; a refusal here is this module's bug.
                debug_assert!(false, "a checked replace was refused: {e}");
                bevy_log::error!("native graph: replace refused: {e}");
                return;
            }
        } else {
            let kind = self.editor.spec().topology.nodes[&k].kind.clone();
            self.editor.insert(k, &kind, legacy);
        }
        if let Some(entry) = self.nodes.get_mut(&k) {
            entry.controls = Some(controls);
        }
        self.edited = true;
    }

    /// Send `param`'s setting through `node`'s ring, and apply it to its
    /// shadow. Lands on the unit at the start of the executor's next block
    /// (see "`set_param` lands on the next block" in the module docs). A full
    /// ring holds the setting control-side; the next `collect` flushes it.
    pub(crate) fn set_param(&mut self, node: AudioNode, param: UnitParam, value: f32) {
        let Some(controls) = self
            .nodes
            .get_mut(&key(node))
            .and_then(|e| e.controls.as_mut())
        else {
            return;
        };
        let _ = controls.set(tutti_core::unit_param::setting(param, value));
    }

    /// Apply `param`'s setting to `node`'s shadow **only** — what a fork of the
    /// node starts from — leaving the live unit to whoever drives it.
    ///
    /// For a param the modulation driver owns: live, the driver writes
    /// `clamp(base + Σ layers)` into the node's own cell every frame, and a
    /// ring write of the bare base would fight it for a block. A fork (an
    /// export) is not modulated by this driver — it gets its modulation from
    /// its own offline one — so what it must carry is the authored base.
    #[cfg(feature = "modulation")]
    pub(crate) fn set_param_snapshot(&mut self, node: AudioNode, param: UnitParam, value: f32) {
        let Some(controls) = self.nodes.get(&key(node)).and_then(|e| e.controls.as_ref()) else {
            return;
        };
        controls
            .shadow()
            .set(tutti_core::unit_param::setting(param, value));
    }

    /// A live duplicate of the whole graph that shares no state with it
    /// (`Editor::fork`, `ForkMode::Live`), as an audio side to render. Each
    /// node is forked from its shadow, so it carries every setting sent — and
    /// only those: a value written into a cell the live unit shares is at what
    /// the unit's `isolate` left in the shadow.
    #[cfg(test)]
    pub(crate) fn fork(&self) -> Result<AudioSide, tutti_graph::ForkError> {
        let (editor, exec) = self.editor.fork(
            tutti_graph::ForkTarget::Master,
            tutti_graph::ForkMode::Live,
            *self.editor.prepare(),
        )?;
        Ok(AudioSide::forked(editor, exec))
    }

    /// `f` over `node`'s shadow: an isolated copy with every setting sent
    /// applied, never processed. `None` for a native node.
    pub(crate) fn inspect<R>(
        &self,
        node: AudioNode,
        f: impl FnOnce(&dyn AudioUnit) -> R,
    ) -> Option<R> {
        let controls = self.nodes.get(&key(node))?.controls.as_ref()?;
        let shadow = controls.shadow();
        Some(f(shadow.0.as_ref()))
    }

    // --- Edges ---

    fn lower(source: GraphSource) -> Source {
        match source {
            GraphSource::Node(node, port) => Source::Node(OutPort {
                node: key(node),
                port: port as u16,
            }),
            GraphSource::Input(port) => Source::Global(port as u16),
            GraphSource::Silence => Source::Zero,
        }
    }

    fn lift(&self, source: Source) -> GraphSource {
        match source {
            Source::Node(p) => self.nodes.get(&p.node).map_or(GraphSource::Silence, |e| {
                GraphSource::Node(e.node, p.port as usize)
            }),
            Source::Global(port) => GraphSource::Input(port as usize),
            Source::Zero => GraphSource::Silence,
        }
    }

    pub(crate) fn source(&self, node: AudioNode, port: usize) -> GraphSource {
        let at = InPort {
            node: key(node),
            port: port as u16,
        };
        match self.editor.spec().topology.edges.get(&at) {
            Some(Edge::Direct(source)) => self.lift(*source),
            // This backend writes no feedback edge (`Net` has none).
            Some(Edge::Feedback(_)) | None => GraphSource::Silence,
        }
    }

    pub(crate) fn set_source(&mut self, node: AudioNode, port: usize, source: GraphSource) {
        assert!(
            !matches!(source, GraphSource::Node(from, _) if from == node),
            "a node cannot feed its own input"
        );
        let at = InPort {
            node: key(node),
            port: port as u16,
        };
        let edges = &mut self.editor.spec_mut().topology.edges;
        match Self::lower(source) {
            // An unwritten port reads silence, so silence is no edge: the
            // value stays the same whether a port was never wired or wired
            // and cleared.
            Source::Zero => {
                edges.remove(&at);
            }
            source => {
                edges.insert(at, Edge::Direct(source));
            }
        }
        self.edited = true;
    }

    pub(crate) fn output_source(&self, channel: usize) -> GraphSource {
        self.editor
            .spec()
            .topology
            .outputs
            .get(channel)
            .map_or(GraphSource::Silence, |s| self.lift(*s))
    }

    /// # Panics
    ///
    /// If `channel` is past the global outputs, as `Net` does.
    pub(crate) fn set_output_source(&mut self, channel: usize, source: GraphSource) {
        self.editor.spec_mut().topology.outputs[channel] = Self::lower(source);
        self.edited = true;
    }

    pub(crate) fn set_outputs_from(&mut self, node: AudioNode) {
        let outs = self.node_outputs(node);
        let k = key(node);
        for (c, source) in self
            .editor
            .spec_mut()
            .topology
            .outputs
            .iter_mut()
            .enumerate()
        {
            *source = if outs == 0 {
                Source::Zero
            } else {
                Source::Node(OutPort {
                    node: k,
                    port: (c % outs) as u16,
                })
            };
        }
        self.edited = true;
    }

    // --- Shape ---

    pub(crate) fn inputs(&self) -> usize {
        self.editor.spec().topology.inputs.count() as usize
    }

    pub(crate) fn outputs(&self) -> usize {
        self.editor.spec().topology.outputs.len()
    }

    fn shape(&self, node: AudioNode) -> Option<tutti_graph::Shape> {
        self.editor.shapes().get(&key(node)).copied()
    }

    pub(crate) fn node_inputs(&self, node: AudioNode) -> usize {
        self.shape(node).map_or(0, |s| s.audio_in.count() as usize)
    }

    pub(crate) fn node_outputs(&self, node: AudioNode) -> usize {
        self.shape(node).map_or(0, |s| s.audio_out.count() as usize)
    }

    pub(crate) fn node_latency(&self, node: AudioNode) -> Samples {
        self.shape(node).map_or(Samples(0), |s| s.latency.samples())
    }

    pub(crate) fn node_tail(&self, node: AudioNode) -> Tail {
        self.shape(node).map_or(Tail::None, |s| s.tail)
    }

    pub(crate) fn widen_outputs(&mut self, channels: usize) {
        let outputs = &mut self.editor.spec_mut().topology.outputs;
        if outputs.len() < channels {
            outputs.resize(channels, Source::Zero);
            self.edited = true;
        }
    }

    // --- Latency ---

    pub(crate) fn latency_plan(&self) -> Compensation {
        tutti_types::latency::plan(&self.editor.spec().topology)
    }

    /// The compensation the next commit's plan carries: the spec compiled
    /// against the shapes, as `commit` will compile it. Nothing is inserted —
    /// PDC is the compiler's. `None` when the spec does not compile (the
    /// commit will say why).
    pub(crate) fn planned_compensation(&self) -> Option<(Vec<Samples>, Samples)> {
        let valid = self.editor.spec().validate().ok()?;
        let (plan, _) = tutti_graph::compile(
            &valid,
            self.editor.shapes(),
            self.editor.prepare(),
            self.editor.base().map(|p| &**p),
        )
        .ok()?;
        Some((plan.compensation().to_vec(), plan.total_latency().samples()))
    }

    /// The compensation of the plan sent last, for a test that checks the
    /// published figures against what the executor was actually handed.
    #[cfg(test)]
    pub(crate) fn sent_compensation(&self) -> Option<(Vec<Samples>, Samples)> {
        let plan = self.editor.base()?;
        Some((plan.compensation().to_vec(), plan.total_latency().samples()))
    }

    /// A node's latency moved at runtime (a plugin's latency cell): the next
    /// commit moves PDC to it without touching the unit.
    #[cfg(feature = "plugin")]
    pub(crate) fn set_node_latency(&mut self, node: AudioNode, latency: Samples) {
        if !self.contains(node) || self.node_latency(node) == latency {
            return;
        }
        match self.editor.set_latency(key(node), Latency::new(latency)) {
            Ok(()) => self.edited = true,
            Err(e) => bevy_log::error!("native graph: latency of {node:?} refused: {e}"),
        }
    }

    // --- Publishing and rendering ---

    /// Drain what the executor sent back, freeing retired units here, and
    /// flush every node's held settings.
    pub(crate) fn collect(&mut self) {
        self.editor.collect();
    }

    pub(crate) fn commit(&mut self) -> Committed {
        match self.editor.commit() {
            Ok(()) => {
                self.edited = false;
                self.pump_local();
                Committed::Done
            }
            Err(CommitError::Backpressure | CommitError::Repreparing) => Committed::Retry,
            Err(e) => {
                // Not retried: the same spec fails the same way next frame.
                // The next edit marks the graph dirty again.
                bevy_log::error!("native graph: commit refused: {e}");
                Committed::Done
            }
        }
    }

    /// Render one frame on a local executor, committing any edit first.
    ///
    /// # Panics
    ///
    /// If the executor was taken: there is no control-side copy of any node
    /// to render instead (see the module docs).
    pub(crate) fn render_frame(&mut self, output: &mut [f32]) {
        assert!(
            self.local.is_some(),
            "render_frame on the native backend needs the audio side, and it was taken; \
             render through it (`AudioGraphRes::take_audio_side`) instead"
        );
        if self.edited {
            let _ = self.commit();
        }
        let local = self.local.as_mut().expect("checked above");
        render(
            &mut local.exec,
            &mut local.scratch,
            &Transport::default(),
            &[],
            output,
        );
    }
}

/// Render one frame of `exec` into `output` (one sample per global output),
/// reading `input` (one sample per global input).
///
/// `output` wider than the plan reads silence past it; narrower, the extra
/// channels are dropped — the width a caller passes is its own business, as
/// with `Net::tick`.
fn render(
    exec: &mut Executor,
    scratch: &mut Vec<Vec<f32>>,
    transport: &Transport,
    input: &[f32],
    output: &mut [f32],
) {
    exec.apply_pending();
    let Some(plan) = exec.plan() else {
        output.fill(0.0);
        return;
    };
    let (ins, outs) = (plan.global_inputs() as usize, plan.global_outputs());
    scratch.resize_with(outs, || vec![0.0]);
    let inputs: Vec<&[f32]> = (0..ins)
        .map(|c| input.get(c).map_or(&[0.0f32][..], std::slice::from_ref))
        .collect();
    let mut outputs: Vec<&mut [f32]> = scratch.iter_mut().map(|c| &mut c[..1]).collect();
    exec.process(1, transport, &inputs, &mut outputs);
    for (c, o) in output.iter_mut().enumerate() {
        *o = scratch.get(c).map_or(0.0, |s| s[0]);
    }
}

/// The audio thread's half of a graph, for a test that renders what a device
/// would hear: every commit lands on it, and rendering it plays the graph.
/// Taken with [`AudioGraphRes::take_audio_side`](super::AudioGraphRes::take_audio_side).
pub struct AudioSide(Side);

#[allow(
    clippy::large_enum_variant,
    reason = "one per test render, held for its life; boxing buys a pointer hop per block"
)]
enum Side {
    Net(tutti_core::NetBackend),
    Native {
        exec: Executor,
        transport: Transport,
        scratch: Vec<Vec<f32>>,
        /// A fork's own editor, kept for as long as its executor runs: the
        /// executor sends its boxes back to it. `None` for the live graph's
        /// audio side, whose editor stays in `AudioGraphRes`.
        _editor: Option<Editor>,
    },
}

impl AudioSide {
    pub(crate) fn net(backend: tutti_core::NetBackend) -> Self {
        Self(Side::Net(backend))
    }

    pub(crate) fn native(exec: Executor) -> Self {
        Self(Side::Native {
            exec,
            transport: Transport::default(),
            scratch: Vec::new(),
            _editor: None,
        })
    }

    /// A fork's pair, rendered as an audio side.
    #[cfg(test)]
    fn forked(editor: Editor, exec: Executor) -> Self {
        Self(Side::Native {
            exec,
            transport: Transport::default(),
            scratch: Vec::new(),
            _editor: Some(editor),
        })
    }

    /// Render one frame: `input` one sample per global input, `output` one
    /// per global output. Commits sent since the last call land first.
    pub fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        match &mut self.0 {
            Side::Net(backend) => backend.tick(input, output),
            Side::Native {
                exec,
                transport,
                scratch,
                ..
            } => render(exec, scratch, transport, input, output),
        }
    }

    /// Render `frames` frames with no global input into `output`, planar,
    /// one slice per global output, in the blocks a device would hand over:
    /// `Net` through `process` in 64-frame chunks (as `tutti_core::Engine`
    /// renders it), the native graph in executor blocks of up to
    /// `block` frames. The transport is stopped at beat zero on both.
    ///
    /// # Panics
    ///
    /// If `block` is zero, or past the native graph's prepared maximum.
    pub fn render(&mut self, frames: usize, block: usize, output: &mut [Vec<f32>]) {
        assert!(block > 0, "a zero-frame block");
        for o in output.iter_mut() {
            o.clear();
            o.resize(frames, 0.0);
        }
        match &mut self.0 {
            Side::Net(backend) => {
                let width = backend.outputs();
                let mut buf = tutti_core::BufferVec::new(width);
                let none = tutti_core::BufferVec::new(backend.inputs());
                let mut done = 0;
                while done < frames {
                    let len = (frames - done).min(block).min(tutti_core::MAX_BUFFER_SIZE);
                    backend.process(len, &none.buffer_ref(), &mut buf.buffer_mut());
                    for (c, o) in output.iter_mut().enumerate().take(width) {
                        o[done..done + len].copy_from_slice(&buf.channel_f32(c)[..len]);
                    }
                    done += len;
                }
            }
            Side::Native {
                exec, transport, ..
            } => {
                let mut done = 0;
                while done < frames {
                    let len = (frames - done).min(block);
                    exec.apply_pending();
                    let (ins, outs) = exec
                        .plan()
                        .map_or((0, 0), |p| (p.global_inputs() as usize, p.global_outputs()));
                    let zeros = vec![0.0f32; len];
                    let inputs: Vec<&[f32]> = (0..ins).map(|_| &zeros[..]).collect();
                    let mut planes: Vec<Vec<f32>> = vec![vec![0.0; len]; outs];
                    let mut slices: Vec<&mut [f32]> =
                        planes.iter_mut().map(|p| &mut p[..]).collect();
                    if exec.plan().is_some() {
                        exec.process(len, transport, &inputs, &mut slices);
                    }
                    for (o, p) in output.iter_mut().zip(&planes) {
                        o[done..done + len].copy_from_slice(p);
                    }
                    done += len;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bevy_app::prelude::*;
    use tutti_core::{AtomicF32, Drive, Ordering, Signal};

    use crate::graph::{
        AudioGraphRes, AudioParam, AudioParamAppExt, GraphBackend, GraphReconcilePlugin,
        MasterSources, PortSource, SpawnAudioNode,
    };
    use crate::AudioEngineState;

    use super::*;

    type DriveParam = AudioParam<Drive, { UnitParam::Drive as u16 }>;

    /// A source whose one output is its `Drive` param, held in a shared cell —
    /// the shape of every node whose param a host drives (`DistortionNode`'s
    /// drive, a filter's cutoff): a `set` writes the cell, a clone shares it.
    ///
    /// Its `isolate` **snapshots the cell**, as every in-tree unit's does once
    /// its `Param` cells are isolated (#29): so a fork of it carries exactly
    /// what reached its shadow, and a write into the live cell does not leak
    /// into the fork through a shared `Arc`. That is what lets this test tell a
    /// path that goes through the settings ring from one that does not.
    #[derive(Clone)]
    struct Knob {
        drive: Arc<AtomicF32>,
    }

    impl Knob {
        const BUILT_WITH: f32 = 1.0;

        fn new() -> Self {
            Self {
                drive: Arc::new(AtomicF32::new(Self::BUILT_WITH)),
            }
        }
    }

    impl AudioUnit for Knob {
        fn isolate(&mut self) {
            self.drive = Arc::new(AtomicF32::new(self.drive.load(Ordering::Acquire)));
        }
        fn tick(&mut self, _: &[f32], output: &mut [f32]) {
            output[0] = self.drive.load(Ordering::Acquire);
        }
        fn process(&mut self, size: usize, _: &BufferRef, output: &mut BufferMut) {
            let v = self.drive.load(Ordering::Acquire);
            for i in 0..size {
                output.set_f32(0, i, v);
            }
        }
        fn set(&mut self, setting: Setting) {
            if let Some((UnitParam::Drive, v)) = tutti_core::unit_param::from_setting(&setting) {
                self.drive.store(v, Ordering::Release);
            }
        }
        fn inputs(&self) -> usize {
            0
        }
        fn outputs(&self) -> usize {
            1
        }
        fn route(&mut self, _: &SignalFrame, _: f64) -> SignalFrame {
            let mut out = SignalFrame::new(1);
            out.set(0, Signal::Latency(0.0));
            out
        }
        fn get_id(&self) -> u64 {
            0
        }
        fn as_any(&self) -> &dyn core::any::Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
            self
        }
        fn tail(&mut self) -> Tail {
            Tail::None
        }
    }

    #[cfg(feature = "modulation")]
    impl tutti_mod::ModParams for Knob {
        fn mod_target(
            &self,
            param: tutti_types::ParamAddr,
            base: f32,
            min: f32,
            max: f32,
        ) -> Option<Arc<dyn tutti_mod::ModTarget>> {
            (param == tutti_types::ParamAddr::Unit(UnitParam::Drive)).then(|| {
                Arc::new(tutti_mod::AtomicTarget::with_mirror(
                    base,
                    min,
                    max,
                    Arc::clone(&self.drive),
                )) as Arc<dyn tutti_mod::ModTarget>
            })
        }
    }

    /// **Every bevy path that writes a param by value reaches a fork of the
    /// node** on the native backend — the constraint export by `Editor::fork`
    /// (doc 013, PR 12) rests on: a fork is cloned from each node's shadow, so a
    /// write that bypasses the settings ring (a captured handle, a shared cell)
    /// moves the live unit and leaves the fork at the value it was built with.
    ///
    /// The paths, one knob each, rendered on one global output each:
    /// - `AudioParam` on an unmodulated param (`write_param` → `set_param`);
    /// - `AudioGraphRes::set_param` called directly;
    /// - `AudioParam` on a param the control-rate driver owns
    ///   (`write_param` → `ModulationMatrix::set_base`): the fork carries the
    ///   authored **base**. The driver's live offset is the exception — a fork
    ///   (an export) runs its own modulation — and the live render shows it is
    ///   there, so the fork's plain base is not the driver doing nothing.
    ///
    /// Not a path here, and listed in doc 013 as a known export limitation:
    /// the audio-rate base (`AudioRateChains::base_cell`), a cell the chain's
    /// `AtomicSourceNode` reads, which takes no `Setting`.
    ///
    /// Mutations (run; each fails its channel):
    /// - `write_param`'s unmodulated arm not going through `graph.set_param`
    ///   (the value lands wherever a captured handle points, which the shadow
    ///   never sees) → channel 0 stays at 1;
    /// - `write_param` dropping `set_param_snapshot` after `set_base` (the
    ///   base reaches only the driver's accumulator) → channel 2 stays at 1.
    ///
    /// Channel 1 pins `set_param` itself; a mutation of it is `LegacyControls`'
    /// own (`tutti-graph`'s `legacy` tests), since this crate only calls it.
    #[test]
    fn every_param_write_path_reaches_a_fork() {
        let mut graph = AudioGraphRes::headless_with(GraphBackend::Native, 0, 3);
        graph.set_sample_rate(tutti_core::SampleRate(48_000.0));
        let mut app = App::new();
        app.insert_resource(graph);
        app.insert_resource(AudioEngineState::Running);
        app.add_plugins(GraphReconcilePlugin);
        #[cfg(feature = "modulation")]
        {
            app.insert_resource(crate::graph::TransportRes(
                tutti_core::transport::Transport::new(48_000.0),
            ));
            app.add_plugins(crate::modulation::TuttiModulationPlugin);
            // Before any knob is bound: the registry is read at capture.
            app.world_mut()
                .resource_mut::<crate::modulation::ModTargetRegistry>()
                .register::<Knob>();
        }
        app.add_audio_param::<Drive, { UnitParam::Drive as u16 }>();

        let mut commands = app.world_mut().commands();
        let knobs = [(); 3].map(|()| commands.spawn_audio_node(Knob::new()).id());
        commands.insert_resource(
            MasterSources::default()
                .with(0, PortSource::node(knobs[0]))
                .with(1, PortSource::node(knobs[1]))
                .with(2, PortSource::node(knobs[2])),
        );
        app.world_mut().flush();
        app.update();

        // Path 1: an `AudioParam` on an unmodulated param.
        app.world_mut()
            .entity_mut(knobs[0])
            .insert(DriveParam::new(Drive(4.0)));
        // Path 2: the graph's own `set_param`.
        let node = *app.world().get::<AudioNode>(knobs[1]).unwrap();
        app.world_mut()
            .resource_mut::<AudioGraphRes>()
            .set_param(node, UnitParam::Drive, 5.0);
        // Path 3: an `AudioParam` on a param the control-rate driver owns.
        #[cfg(feature = "modulation")]
        {
            use crate::modulation::{LfoShape, ModParamRange, ModRoute, ModSource, ModSourceRate};
            use tutti_types::{Depth, Hz, ParamAddr};
            let drive = ParamAddr::Unit(UnitParam::Drive);
            app.world_mut()
                .entity_mut(knobs[2])
                .insert(ModParamRange::default().with(drive, 1.0, 0.0, 10.0));
            // A square at zero rate: a constant offset, so the live value
            // stands visibly off the base.
            let lfo = app
                .world_mut()
                .spawn((
                    ModSource::new(LfoShape::Square),
                    ModSourceRate::free_running(Hz(0.0)),
                ))
                .id();
            app.world_mut()
                .spawn(ModRoute::new(lfo, knobs[2], drive).with_depth(Depth(0.2)));
            app.update();
            app.world_mut()
                .entity_mut(knobs[2])
                .insert(DriveParam::new(Drive(6.0)));
        }
        app.update();
        app.update();

        let graph = app.world().resource::<AudioGraphRes>();
        let mut fork = graph.fork().expect("every knob is forkable");
        let mut forked = [0.0f32; 3];
        fork.tick(&[], &mut forked);
        assert_eq!(forked[0], 4.0, "an AudioParam write reaches the fork");
        assert_eq!(forked[1], 5.0, "a set_param write reaches the fork");
        #[cfg(feature = "modulation")]
        {
            assert_eq!(forked[2], 6.0, "a modulated param's base reaches the fork");
            let mut live = [0.0f32; 3];
            app.world_mut()
                .resource_mut::<AudioGraphRes>()
                .render_frame(&mut live);
            assert!(
                (live[2] - 6.0).abs() > 0.5,
                "the live knob is modulated off its base (got {}), so the fork's \
                 plain base is the exception at work, not a driver that did nothing",
                live[2]
            );
        }
    }
}
