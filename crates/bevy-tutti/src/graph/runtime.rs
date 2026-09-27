//! The runtime behind [`AudioGraphRes`](super::AudioGraphRes): a
//! `tutti-graph` [`Editor`], and — until the engine or a test takes it — the
//! [`Executor`] it sends to.
//!
//! Design doc 013, Phase 3: added beside fundsp's `Net` in PR 11, the only
//! runtime since PR 13. Every method here answers one of `AudioGraphRes`'s,
//! and the mapping is:
//!
//! | `AudioGraphRes` | here |
//! |---|---|
//! | `insert` | `Editor::insert` of the node's own [`IntoNode`] at a [`NodeKey`] minted by `AudioNode::fresh`, its controls handed back |
//! | `set_source`, `set_output_source`, `widen_outputs` | written into `editor.spec_mut()` |
//! | `set_param` | the node's [`ParamSet`], when it has one |
//! | `replace` | `Editor::replace_or_swap` with a [`Fade`]: a plain swap when there is nothing to fade from |
//! | `node_latency`, `node_tail`, `node_inputs`, `node_outputs` | the editor's [`Shapes`](tutti_graph::Shapes) |
//! | `latency_plan` | `tutti_types::latency::plan` over the spec's topology |
//! | `compensate` | compiles the spec, reads `Plan::compensation` / `total_latency`; inserts nothing |
//! | `commit` | `Editor::commit`, which collects first |
//! | `set_node_latency` | `Editor::set_latency` |
//! | `insert_plugin`, `replace_plugin` | the bound `PluginClient` itself, a node with its own fork source |
//! | `export` | `Editor::fork` in `ForkMode::Offline`, through `tutti_export::RenderGraph::fork` |
//!
//! # Where the audio side lives
//!
//! `Editor::new` builds the pair. The executor stays here ("local") until
//! [`take_executor`](GraphRuntime::take_executor) hands it to the engine or to
//! a test's [`AudioSide`](super::AudioSide). While it is local, a commit is
//! applied at once on this thread and its box collected, so a headless graph
//! never meets back-pressure, and [`render_frame`](GraphRuntime::render_frame)
//! runs it.
//!
//! # `set_param` lands on the next block, never at once
//!
//! There is no control-side copy of a node for a setting to land on at once
//! (a `Net` with no audio side, before PR 13, applied one straight to its only
//! copy, so a test could read an atomic back the moment it wrote it). A
//! write goes into the node's own `Param` cell through its [`ParamSet`], and
//! the node reads it at the start of the executor's next block: one block
//! later, on every graph. A test that reads what a write did renders a frame
//! first ([`render_frame`](super::AudioGraphRes::render_frame)).

use std::collections::BTreeMap;
use std::sync::Arc;

use tutti_core::{AudioNode, Compensation, CrossfadeCurve, Samples, Tail};
use tutti_graph::{
    CommitError, Editor, EventEdge, EventIn, EventOut, Executor, Fade, GraphInvalid, IntoNode,
    ParamFrom, ParamIn, ParamMod, ParamRange, ParamSet, ParamShaping, Prepare, Transport,
    MAX_PARAM_SOURCES,
};
use tutti_types::graph::{Edge, InPort, NodeKey, OutPort, Source};
#[cfg(feature = "plugin")]
use tutti_types::Latency;
use tutti_types::{ChannelLayout, SampleRate, Seconds, UnitParam};

use super::resources::GraphSource;

/// The largest block the graph is prepared for.
///
/// A device block longer than this is rendered by `tutti_core::Engine` as
/// consecutive graph blocks of at most this many frames, so it bounds the
/// arena, not the device. 1024 frames covers every buffer size a DAW offers
/// by default.
pub(crate) const LIVE_MAX_BLOCK: Samples = Samples(1024);

/// The spec `kind` of a hosted plugin.
#[cfg(feature = "plugin")]
const PLUGIN_KIND: &str = "bevy-tutti:plugin";

/// Why [`AudioGraphRes::replace`](super::AudioGraphRes::replace) did not
/// take a node — or, as `ReplaceRefused<Box<PluginClient>>`,
/// `AudioGraphRes::replace_plugin` a plugin.
pub enum ReplaceRefused<U> {
    /// Not now: the graph is re-preparing (a sample-rate or block-size change
    /// between its two commits). The node is handed back, untouched; retry
    /// once the re-prepare has resumed —
    /// [`crossfade_audio_node`](super::crossfade_audio_node) keeps it pending
    /// and does.
    Busy(U),
    /// Never: the graph is poisoned (a re-prepare failed with its units out),
    /// or the node is not in it. The incoming node was dropped.
    Failed(String),
}

impl<U> std::fmt::Debug for ReplaceRefused<U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy(_) => f.write_str("Busy(..)"),
            Self::Failed(why) => f.debug_tuple("Failed").field(why).finish(),
        }
    }
}

/// One node's handle, and its params by address when it has any.
struct Entry {
    node: AudioNode,
    /// What [`set_param`](GraphRuntime::set_param) writes through, and what a
    /// fork of the node starts from.
    params: Option<ParamSet>,
}

/// The executor, while this side still holds it, and what a local render
/// needs beside it.
pub(crate) struct Local {
    exec: Executor,
    /// Planar scratch, one `Vec` per global output.
    scratch: Vec<Vec<f32>>,
}

/// The graph runtime. See the module docs.
pub(crate) struct GraphRuntime {
    editor: Editor,
    local: Option<Local>,
    nodes: BTreeMap<NodeKey, Entry>,
    /// Edited since the last commit — only a local render reads it, to
    /// commit before it renders, so a local render plays the graph as
    /// edited, uncommitted edits included.
    edited: bool,
    /// Threads per block, the caller included (`TuttiPlugin::render_workers`;
    /// `0` for one per core).
    render_workers: usize,
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

/// `node`'s key. [`AudioNode::fresh`] draws it from
/// [`NodeKey::fresh`]'s process-wide counter, so a key minted this way is
/// unique without a map.
pub(crate) fn key(node: AudioNode) -> NodeKey {
    node.0
}

/// The graph's `Prepare`: `rate`, `max_block`, and the device's callback
/// `quantum` when the host knows it (`tutti_cpal::OutputSpec::quantum`). A
/// hosted out-of-process plugin ships one quantum per callback to its server
/// (doc 013, decision 8 reversed), so the quantum is what keeps its pipeline
/// in step with the device.
pub(crate) fn prepare_for(
    rate: SampleRate,
    max_block: Samples,
    quantum: Option<Samples>,
) -> Prepare {
    let p = Prepare::new(rate, max_block);
    match quantum {
        Some(q) => p.with_quantum(q),
        None => p,
    }
}

/// The graph's current `Prepare`.
impl GraphRuntime {
    pub(crate) fn prepared(&self) -> Prepare {
        *self.editor.prepare()
    }
}

/// `latency` clamped to what PDC compensates, as the editor clamps a latency
/// it probes at insert.
#[cfg(feature = "plugin")]
pub(crate) fn clamp_latency(latency: Latency) -> Latency {
    Latency::new(
        latency
            .samples()
            .min(tutti_types::latency::MAX_NODE_LATENCY),
    )
}

impl GraphRuntime {
    /// An empty graph with `inputs` global inputs and `outputs` global
    /// outputs, prepared for `rate` and the device's callback `quantum` (when
    /// known: see [`prepare_for`]), its executor local.
    pub(crate) fn new(
        inputs: usize,
        outputs: usize,
        rate: SampleRate,
        quantum: Option<Samples>,
    ) -> Self {
        let (mut editor, exec) = Editor::new(prepare_for(rate, LIVE_MAX_BLOCK, quantum));
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
            render_workers: 1,
        }
    }

    /// See `AudioGraphRes::set_render_workers`.
    pub(crate) fn set_render_workers(&mut self, workers: usize) {
        self.render_workers = workers;
    }

    /// A fresh pool of the configured size, its threads named `name`, or
    /// `None` for one participant (render on the caller alone).
    pub(crate) fn render_pool(&self, name: &str) -> Option<Arc<dyn tutti_graph::Pool>> {
        let n = match self.render_workers {
            0 => std::thread::available_parallelism().map_or(1, |n| n.get()),
            n => n,
        };
        (n > 1).then(|| {
            Arc::new(tutti_core::WorkerPool::builder(n).name(name).build())
                as Arc<dyn tutti_graph::Pool>
        })
    }

    /// Hand the executor over — to the engine, or to a test's audio side.
    ///
    /// # Panics
    ///
    /// If it was already taken.
    pub(crate) fn take_executor(&mut self) -> Executor {
        self.local
            .take()
            .expect("the graph's audio side was already taken")
            .exec
    }

    /// The editor, for `Engine::new`.
    pub(crate) fn editor_mut(&mut self) -> &mut Editor {
        &mut self.editor
    }

    /// Re-prepare every node for `rate`. With the executor local, both halves
    /// of the re-prepare run here, now; with it taken, the second half lands
    /// on a later `collect` (and commits are `Retry` until it does).
    pub(crate) fn set_sample_rate(&mut self, rate: SampleRate) {
        let current = *self.editor.prepare();
        if let Err(e) = self.editor.reprepare(prepare_for(
            rate,
            current.max_block().samples(),
            current.quantum(),
        )) {
            bevy_log::error!("graph: re-prepare at {} Hz refused: {e}", rate.get());
            return;
        }
        self.pump_local();
    }

    /// Whether a re-prepare is between its two commits.
    pub(crate) fn is_repreparing(&self) -> bool {
        self.editor.is_repreparing()
    }

    /// Refuse, before a device restart stops anything, a re-prepare to
    /// `max_block` this graph could not start: past the engine's block
    /// capacity (the editor's limits, set by `Engine::new`), with one
    /// already between its halves, or on a poisoned editor. The rest of what
    /// `Editor::reprepare` checks depends on the new `Prepare` and is left
    /// to it.
    pub(crate) fn check_reprepare(&self, max_block: Option<Samples>) -> Result<(), CommitError> {
        if let Some(cause) = self.editor.poisoned() {
            return Err(CommitError::Poisoned {
                cause: cause.to_owned(),
            });
        }
        if self.editor.is_repreparing() {
            return Err(CommitError::Repreparing);
        }
        let limit = self.editor.limits().max_block;
        match max_block {
            Some(b) if b.get() > limit => Err(CommitError::BlockTooLong {
                max_block: b.get(),
                limit,
            }),
            _ => Ok(()),
        }
    }

    /// Re-prepare every node for `rate`, the device's callback `quantum`
    /// (the new device's: `None` when it does not say) and, if given,
    /// `max_block` (else the block it has): the first half of
    /// `Editor::reprepare`, sent. The executor checks its units out on its
    /// next block, and a later `collect` sends them back re-prepared (every
    /// frame's `commit_graph` does it). A no-op when nothing moves.
    pub(crate) fn reprepare(
        &mut self,
        rate: SampleRate,
        max_block: Option<Samples>,
        quantum: Option<Samples>,
    ) -> Result<(), CommitError> {
        let current = *self.editor.prepare();
        let prepare = prepare_for(
            rate,
            max_block.unwrap_or(current.max_block().samples()),
            quantum,
        );
        if prepare == current {
            return Ok(());
        }
        self.editor.reprepare(prepare)?;
        self.pump_local();
        Ok(())
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

    /// Insert `node` through its own [`IntoNode`]: its controls come back to
    /// the caller, and its fork source (which carries whatever MIDI it
    /// plays: a clip node's events, a synth's installed clip) goes to the
    /// editor. Address its params with [`set_node_params`](Self::set_node_params).
    pub(crate) fn insert<N: IntoNode>(&mut self, node: N) -> (AudioNode, N::Controls) {
        let id = AudioNode::fresh();
        let controls = self.editor.insert(key(id), N::kind(), node);
        self.nodes.insert(
            key(id),
            Entry {
                node: id,
                params: None,
            },
        );
        self.edited = true;
        (id, controls)
    }

    /// Insert a hosted plugin: bound (`PluginClient::bind`, the typestate
    /// transition doc 013 §2 describes) and inserted as the `Node` it
    /// then is, which hands the editor its own
    /// [`ForkSource`](tutti_graph::ForkSource) (a fork by state transfer) and
    /// declares its latency in its `Shape`.
    ///
    /// The host drives it through the `PluginControls` it captured before
    /// inserting (`CapturedControls::for_plugin`), and a latency change
    /// reaches the editor as the figure those controls declare
    /// ([`refresh_node_latency`](Self::refresh_node_latency)).
    #[cfg(feature = "plugin")]
    pub(crate) fn insert_plugin(
        &mut self,
        client: Box<tutti_plugin::handles::PluginClient>,
    ) -> AudioNode {
        let node = AudioNode::fresh();
        let _controls = self.editor.insert(key(node), PLUGIN_KIND, client.bind());
        self.nodes.insert(key(node), Entry { node, params: None });
        self.edited = true;
        node
    }

    /// Swap the plugin at `node` for `client`, as [`replace`](Self::replace)
    /// swaps any node: crossfading when the running node's shape fits the
    /// bound plugin's (ports, latency, the rest `Editor::replace_or_swap`
    /// checks), else a plain swap at the same key. Refused, handing `client`
    /// back unbound, while a re-prepare is between its two commits.
    #[cfg(feature = "plugin")]
    pub(crate) fn replace_plugin(
        &mut self,
        node: AudioNode,
        client: Box<tutti_plugin::handles::PluginClient>,
        fade: Seconds,
        curve: CrossfadeCurve,
    ) -> Result<(), ReplaceRefused<Box<tutti_plugin::handles::PluginClient>>> {
        // Asked here, before binding: `replace` hands back what it refuses,
        // and a bound plugin cannot be unbound.
        if let Some(cause) = self.editor.poisoned() {
            return Err(ReplaceRefused::Failed(format!(
                "the graph is poisoned ({cause}); build a new one"
            )));
        }
        if self.editor.is_repreparing() {
            return Err(ReplaceRefused::Busy(client));
        }
        match self.replace(node, client.bind(), fade, curve) {
            Ok(_controls) => Ok(()),
            Err(ReplaceRefused::Failed(why)) => Err(ReplaceRefused::Failed(why)),
            Err(ReplaceRefused::Busy(_)) => unreachable!("checked above"),
        }
    }

    /// A copy of `target` for an offline render at `rate`, sharing no state
    /// with this graph: `Editor::fork` with `ForkMode::Offline(ctx)`, through
    /// tutti-export (`RenderGraph::fork`, which prepares it at the render's
    /// rate and `GRAPH_MAX_BLOCK`). `ctx` is the render's timeline, the one
    /// type `ForkMode::Offline` takes.
    #[cfg(feature = "export")]
    pub(crate) fn fork_for_export(
        &self,
        target: tutti_graph::ForkTarget,
        ctx: &tutti_core::transport::OfflineTransport,
        rate: SampleRate,
    ) -> tutti_export::Result<tutti_export::RenderGraph> {
        let mut graph = tutti_export::RenderGraph::fork(
            &self.editor,
            target,
            tutti_graph::ForkMode::Offline(ctx),
            rate,
        )?;
        // Its own pool, not the live engine's: the live callback would find
        // a shared one busy with the export's job and render alone.
        graph.set_pool(self.render_pool("tutti-export"));
        Ok(graph)
    }

    /// Address `node`'s params by `params` (a `ParamNode`'s controls), so
    /// [`set_param`](Self::set_param) and the fork snapshot reach it.
    pub(crate) fn set_node_params(&mut self, node: AudioNode, params: Option<ParamSet>) {
        if let Some(entry) = self.nodes.get_mut(&key(node)) {
            entry.params = params;
        }
    }

    /// Swap the unit behind `node` for `incoming` under a `fade`
    /// along `curve` when its shape fits the running unit's, else as a plain
    /// swap on the next commit (`Editor::replace_or_swap`). Hands back its
    /// controls; the caller addresses its params
    /// ([`set_node_params`](Self::set_node_params)). Refused as
    /// [`ReplaceRefused::Busy`] with the node handed back while the graph
    /// re-prepares, and for good on a poisoned graph.
    pub(crate) fn replace<N: IntoNode>(
        &mut self,
        node: AudioNode,
        incoming: N,
        fade: Seconds,
        curve: CrossfadeCurve,
    ) -> Result<N::Controls, ReplaceRefused<N>> {
        if let Some(cause) = self.editor.poisoned() {
            return Err(ReplaceRefused::Failed(format!(
                "the graph is poisoned ({cause}); build a new one"
            )));
        }
        if self.editor.is_repreparing() {
            return Err(ReplaceRefused::Busy(incoming));
        }
        let k = key(node);
        if !self.nodes.contains_key(&k) {
            return Err(ReplaceRefused::Failed(format!(
                "{node:?} is not in the graph"
            )));
        }
        let rate = self.editor.prepare().sample_rate();
        let controls = self
            .editor
            .replace_or_swap(k, incoming, Fade::seconds(fade, rate, curve))
            .map_err(|e| ReplaceRefused::Failed(e.to_string()))?;
        if let Some(entry) = self.nodes.get_mut(&k) {
            entry.params = None;
        }
        self.edited = true;
        Ok(controls)
    }

    /// How many event inputs `node` declares.
    pub(crate) fn node_event_inputs(&self, node: AudioNode) -> usize {
        self.shape(node).map_or(0, |s| usize::from(s.event_in))
    }

    /// Feed `sink`'s event input `port` from exactly `sources` (fan-in merges
    /// them by offset; the graph orders ties by source).
    /// Touches nothing when that is what it already holds.
    pub(crate) fn set_event_sources(
        &mut self,
        sink: AudioNode,
        port: u16,
        sources: &[crate::graph::EventSource],
    ) {
        let at = EventIn {
            node: key(sink),
            port,
        };
        let mut want: Vec<EventOut> = sources
            .iter()
            .map(|s| EventOut {
                node: key(s.node),
                port: s.port,
            })
            .collect();
        want.sort();
        want.dedup();
        let spec = self.editor.spec_mut();
        let have: Vec<EventOut> = spec
            .events
            .get(&at)
            .map(|v| v.iter().map(|e| e.from()).collect())
            .unwrap_or_default();
        if have == want {
            return;
        }
        spec.events.remove(&at);
        for from in want {
            spec.connect_events(at, EventEdge::Direct(from));
        }
        self.edited = true;
    }

    /// The event outputs feeding `sink`'s event input `port`, in the graph's
    /// order.
    pub(crate) fn event_sources(
        &self,
        sink: AudioNode,
        port: u16,
    ) -> Vec<crate::graph::EventSource> {
        let at = EventIn {
            node: key(sink),
            port,
        };
        self.editor
            .spec()
            .events
            .get(&at)
            .into_iter()
            .flatten()
            .filter_map(|e| {
                let from = e.from();
                self.nodes
                    .get(&from.node)
                    .map(|n| crate::graph::EventSource::new(n.node, from.port))
            })
            .collect()
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

    /// Write `param` through `node`'s [`ParamSet`]: the cell the node reads
    /// at the start of its next block (see "`set_param` lands on the next
    /// block" in the module docs), and the authored value a fork starts
    /// from. A node with no set, or no such param, takes nothing.
    pub(crate) fn set_param(&mut self, node: AudioNode, param: UnitParam, value: f32) {
        if let Some(params) = self.nodes.get(&key(node)).and_then(|e| e.params.as_ref()) {
            params.set(param, value);
        }
    }

    /// Write `param`'s **authored** value only — what a fork of the node
    /// starts from — leaving the live cell to whoever drives it.
    ///
    /// For a param the modulation driver owns: live, the driver writes
    /// `clamp(base + Σ layers)` into the node's own cell every frame, and a
    /// live write of the bare base would fight it for a block. A fork (an
    /// export) is not modulated by this driver — it gets its modulation from
    /// its own offline one — so what it must carry is the authored base.
    #[cfg(feature = "modulation")]
    pub(crate) fn set_param_snapshot(&mut self, node: AudioNode, param: UnitParam, value: f32) {
        if let Some(params) = self.nodes.get(&key(node)).and_then(|e| e.params.as_ref()) {
            params.set_authored(param, value);
        }
    }

    /// A live duplicate of the whole graph that shares no state with it
    /// (`Editor::fork`, `ForkMode::Live`), as an audio side to render. Each
    /// node is forked from its own fork source: a [`ParamNode`](tutti_graph::ParamNode)
    /// from its set's authored values.
    #[cfg(test)]
    pub(crate) fn fork(&self) -> Result<AudioSide, tutti_graph::ForkError> {
        let (editor, exec) = self.editor.fork(
            tutti_graph::ForkTarget::Master,
            tutti_graph::ForkMode::Live,
            *self.editor.prepare(),
        )?;
        Ok(AudioSide::forked(editor, exec))
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
            // This adapter writes no feedback edge.
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

    // --- Param modulation (design doc 013 item 6) ---

    /// Whether `node` declares `param` modulatable (its shape's params).
    pub(crate) fn declares_param(&self, node: AudioNode, param: UnitParam) -> bool {
        self.shape(node)
            .is_some_and(|s| s.params.index_of(param).is_some())
    }

    /// Drive `node`'s `param` from exactly `sources` (each a node's output 0,
    /// shaped), clamped to `range`: replaces whatever modulated it. Refused,
    /// touching nothing, with the error the commit would otherwise fail on
    /// — every commit after it, since the same spec fails the same way — for
    /// a NaN bound, more than `MAX_PARAM_SOURCES` sources, or one node listed
    /// twice (which the spec could only keep once).
    pub(crate) fn set_param_mod(
        &mut self,
        node: AudioNode,
        param: UnitParam,
        sources: &[(AudioNode, ParamShaping)],
        range: ParamRange,
    ) -> Result<(), GraphInvalid> {
        let at = ParamIn {
            node: key(node),
            param,
        };
        if range.min.is_nan() || range.max.is_nan() {
            return Err(GraphInvalid::BadParamRange { at, range });
        }
        if sources.len() > MAX_PARAM_SOURCES {
            return Err(GraphInvalid::TooManyParamSources {
                at,
                count: sources.len(),
            });
        }
        if sources
            .iter()
            .enumerate()
            .any(|(i, (n, _))| sources[..i].iter().any(|(m, _)| m == n))
        {
            return Err(GraphInvalid::UnsortedParamSources { at });
        }
        let spec = self.editor.spec_mut();
        spec.params.remove(&at);
        for (from, shaping) in sources {
            spec.connect_param(
                at,
                ParamFrom::Audio(OutPort {
                    node: key(*from),
                    port: 0,
                }),
                shaping.clone(),
            );
        }
        spec.set_param_range(at, range);
        self.edited = true;
        Ok(())
    }

    /// Stop modulating `node`'s `param`: it reads its own control again.
    pub(crate) fn clear_param_mod(&mut self, node: AudioNode, param: UnitParam) {
        let at = ParamIn {
            node: key(node),
            param,
        };
        if self.editor.spec_mut().params.remove(&at).is_some() {
            self.edited = true;
        }
    }

    /// How `node`'s `param` is modulated, as the graph value holds it.
    pub(crate) fn param_mod(&self, node: AudioNode, param: UnitParam) -> Option<ParamMod> {
        self.editor
            .spec()
            .params
            .get(&ParamIn {
                node: key(node),
                param,
            })
            .cloned()
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
    /// If `channel` is past the global outputs.
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

    /// Over the whole spec, so event and param sources count toward a node's
    /// arrival as the compiler counts them.
    pub(crate) fn latency_plan(&self) -> Compensation {
        tutti_types::latency::plan(self.editor.spec())
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

    /// The compensation of the plan sent last: what `commit_graph` publishes,
    /// since it is what the executor is handed.
    pub(crate) fn sent_compensation(&self) -> Option<(Vec<Samples>, Samples)> {
        let plan = self.editor.base()?;
        Some((plan.compensation().to_vec(), plan.total_latency().samples()))
    }

    /// A node's latency moved at runtime (a plugin's latency cell): hand
    /// `latency` — the figure the node's `Shape` declares now, which for a
    /// plugin is `PluginControls::declared_latency` (its own figure **plus**
    /// the chunk its pipeline holds) — to the editor (`Editor::set_latency`),
    /// so the next commit moves PDC to it without touching the running node.
    /// Doc 013: a latency change is a `Shape` change in the next commit.
    ///
    /// Returns whether the editor's figure moved. `tests/plugin_capture.rs`
    /// pins the path.
    #[cfg(feature = "plugin")]
    pub(crate) fn refresh_node_latency(&mut self, node: AudioNode, latency: Latency) -> bool {
        if !self.nodes.contains_key(&key(node)) {
            return false;
        }
        // Clamped to what PDC compensates, as the editor clamps a latency it
        // probes at insert: `set_latency` refuses a figure past it, and a
        // refused one would never be retried (the poll only fires again when
        // the plugin's figure moves).
        let latency = clamp_latency(latency);
        if self.node_latency(node) == latency.samples() {
            return false;
        }
        match self.editor.set_latency(key(node), latency) {
            Ok(()) => {
                self.edited = true;
                true
            }
            Err(e) => {
                bevy_log::error!("graph: latency of {node:?} refused: {e}");
                false
            }
        }
    }

    // --- Publishing and rendering ---

    /// Drain what the executor sent back, freeing retired units here.
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
                bevy_log::error!("graph: commit refused: {e}");
                Committed::Done
            }
        }
    }

    /// Render one frame on a local executor, committing any edit first,
    /// with the transport at `transport`.
    ///
    /// # Panics
    ///
    /// If the executor was taken: there is no control-side copy of any node
    /// to render instead (see the module docs).
    pub(crate) fn render_frame_at(&mut self, transport: &Transport, output: &mut [f32]) {
        assert!(
            self.local.is_some(),
            "render_frame needs the audio side, and it was taken; \
             render through it (`AudioGraphRes::take_audio_side`) instead"
        );
        if self.edited {
            let _ = self.commit();
        }
        let local = self.local.as_mut().expect("checked above");
        render(&mut local.exec, &mut local.scratch, transport, &[], output);
    }
}

/// Render one frame of `exec` into `output` (one sample per global output),
/// reading `input` (one sample per global input).
///
/// `output` wider than the plan reads silence past it; narrower, the extra
/// channels are dropped — the width a caller passes is its own business.
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
pub struct AudioSide {
    exec: Executor,
    transport: Transport,
    scratch: Vec<Vec<f32>>,
    /// A fork's own editor, kept for as long as its executor runs: the
    /// executor sends its boxes back to it. `None` for the live graph's
    /// audio side, whose editor stays in `AudioGraphRes`.
    _editor: Option<Editor>,
}

impl AudioSide {
    pub(crate) fn live(exec: Executor) -> Self {
        Self {
            exec,
            transport: Transport::default(),
            scratch: Vec::new(),
            _editor: None,
        }
    }

    /// A fork's pair, rendered as an audio side.
    #[cfg(test)]
    fn forked(editor: Editor, exec: Executor) -> Self {
        Self {
            exec,
            transport: Transport::default(),
            scratch: Vec::new(),
            _editor: Some(editor),
        }
    }

    /// Render one frame: `input` one sample per global input, `output` one
    /// per global output. Commits sent since the last call land first.
    pub fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        render(
            &mut self.exec,
            &mut self.scratch,
            &self.transport,
            input,
            output,
        );
    }

    /// The transport every block from here on renders under, as a device's
    /// engine hands its graph the playhead: a node that reads the transport
    /// from its `Env` (a placed sampler voice) plays on it. Stopped at beat
    /// zero until set.
    pub fn set_transport(&mut self, transport: Transport) {
        self.transport = transport;
    }

    /// Render `frames` frames with no global input into `output`, planar,
    /// one slice per global output, in the blocks a device would hand over:
    /// executor blocks of up to `block` frames, each under the transport
    /// last [set](Self::set_transport) (stopped at beat zero until one is).
    ///
    /// # Panics
    ///
    /// If `block` is zero, or past the graph's prepared maximum.
    pub fn render(&mut self, frames: usize, block: usize, output: &mut [Vec<f32>]) {
        assert!(block > 0, "a zero-frame block");
        for o in output.iter_mut() {
            o.clear();
            o.resize(frames, 0.0);
        }
        let exec = &mut self.exec;
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
            let mut slices: Vec<&mut [f32]> = planes.iter_mut().map(|p| &mut p[..]).collect();
            if exec.plan().is_some() {
                exec.process(len, &self.transport, &inputs, &mut slices);
            }
            for (o, p) in output.iter_mut().zip(&planes) {
                o[done..done + len].copy_from_slice(p);
            }
            done += len;
        }
    }
}

#[cfg(test)]
mod tests {

    use bevy_app::prelude::*;
    use tutti_core::Drive;
    use tutti_graph::NodeParts;

    use crate::graph::{
        AudioGraphRes, AudioParam, AudioParamAppExt, GraphReconcilePlugin, MasterSources,
        PortSource, SpawnAudioNode,
    };
    use crate::AudioEngineState;

    use super::*;

    type DriveParam = AudioParam<Drive, { UnitParam::Drive as u16 }>;

    /// A source whose one output is its `Drive` param — the shape of every
    /// node whose param a host drives (`DistortionNode`'s drive, a filter's
    /// cutoff): its drive a `Param` cell addressed by a `ParamSet`, inserted
    /// through `param_parts` (so its fork starts from the set's authored
    /// values) and spawned as a `GraphNode` with its params.
    #[derive(Clone)]
    struct Knob {
        drive: tutti_types::Param<Drive>,
    }

    impl Knob {
        const BUILT_WITH: f32 = 1.0;

        fn new() -> Self {
            Self::at(Self::BUILT_WITH)
        }

        fn at(drive: f32) -> Self {
            Self {
                drive: tutti_types::Param::new(Drive(drive)),
            }
        }
    }

    impl tutti_graph::Node for Knob {
        fn shape(&self) -> tutti_graph::Shape {
            tutti_graph::Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(
            &mut self,
            _: &tutti_graph::Cx<'_>,
            mut io: tutti_graph::Io<'_>,
        ) -> tutti_graph::Status {
            io.output(0).fill(self.drive.load().get());
            tutti_graph::Status::Modified
        }
        fn reset(&mut self) {}
    }

    impl tutti_graph::ParamNode for Knob {
        fn param_set(&self) -> ParamSet {
            ParamSet::builder()
                .param(UnitParam::Drive, self.drive.as_atomic())
                .build()
        }
        fn fork_fresh(&self) -> Self {
            let mut fork = self.clone();
            fork.drive.detach();
            fork
        }
    }

    impl IntoNode for Knob {
        type Controls = ParamSet;
        fn into_parts(self) -> NodeParts<ParamSet> {
            tutti_graph::param_parts(self)
        }
    }

    impl crate::graph::GraphNode for Knob {
        fn captured(&self) -> crate::graph::CapturedControls {
            crate::graph::CapturedControls::for_params(&tutti_graph::ParamNode::param_set(self))
        }
        fn params(controls: &ParamSet) -> Option<ParamSet> {
            Some(controls.clone())
        }
    }

    /// **Every param write path reaches a fork of a node** — the constraint
    /// export by `Editor::fork` (doc 013, PR 12) rests on — through the
    /// node's `ParamSet`: `AudioParam` and `set_param` write the live cell and the authored
    /// value, a param the control-rate driver owns gets its base as the
    /// authored value only (the live cell is the driver's), and a fork
    /// starts from the authored values. Under `modulation` the set addresses
    /// the cells as control-rate targets (`CapturedControls::for_params`).
    ///
    /// The paths, one knob each, rendered on one global output each:
    /// `AudioParam` on an unmodulated param (`write_param` → `set_param`);
    /// `AudioGraphRes::set_param` directly; `AudioParam` on a param the
    /// control-rate driver owns (`write_param` → `ModulationMatrix::set_base`:
    /// the fork carries the authored **base**); and a param the driver owns
    /// with no write at all (the base its rebuild seeded from
    /// `ModParamRange`). The driver's live offset is the exception — a fork
    /// (an export) runs its own modulation — and the live render shows it is
    /// there, so the fork's plain base is not the driver doing nothing.
    /// (This was the twin of a test of the same paths through the `Legacy`
    /// adapter's settings ring and shadow, deleted with it.)
    ///
    /// Mutations (run; each fails its channel):
    /// - `GraphRuntime::set_param` skipping a node's `ParamSet` → channels 0
    ///   and 1 stay at 1;
    /// - `set_param_snapshot` skipping it → channel 2 forks at the live
    ///   composite, not 6 (and channel 3 not at 2.5);
    /// - `insert_and_bind` not addressing the params
    ///   (`set_node_params`) → channels 0 and 1 stay at 1.
    #[test]
    fn every_param_write_path_reaches_a_fork() {
        let mut graph = AudioGraphRes::headless(0, 4);
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
        }
        app.add_audio_param::<Drive, { UnitParam::Drive as u16 }>();

        let mut commands = app.world_mut().commands();
        let knobs = [(); 4].map(|()| commands.spawn_audio_node(Knob::new()).id());
        commands.insert_resource(
            MasterSources::default()
                .with(0, PortSource::node(knobs[0]))
                .with(1, PortSource::node(knobs[1]))
                .with(2, PortSource::node(knobs[2]))
                .with(3, PortSource::node(knobs[3])),
        );
        app.world_mut().flush();
        app.update();

        app.world_mut()
            .entity_mut(knobs[0])
            .insert(DriveParam::new(Drive(4.0)));
        let node = *app.world().get::<AudioNode>(knobs[1]).unwrap();
        app.world_mut()
            .resource_mut::<AudioGraphRes>()
            .set_param(node, UnitParam::Drive, 5.0);
        #[cfg(feature = "modulation")]
        {
            use crate::modulation::{LfoShape, ModParamRange, ModRoute, ModSource, ModSourceRate};
            use tutti_types::{Depth, Hz, ParamAddr};
            let drive = ParamAddr::Unit(UnitParam::Drive);
            app.world_mut()
                .entity_mut(knobs[2])
                .insert(ModParamRange::default().with(drive, 1.0, 0.0, 10.0));
            let lfo = app
                .world_mut()
                .spawn((
                    ModSource::new(LfoShape::Square),
                    ModSourceRate::free_running(Hz(0.0)),
                ))
                .id();
            app.world_mut()
                .spawn(ModRoute::new(lfo, knobs[2], drive).with_depth(Depth(0.2)));
            app.world_mut()
                .entity_mut(knobs[3])
                .insert(ModParamRange::default().with(drive, 2.5, 0.0, 10.0));
            app.world_mut()
                .spawn(ModRoute::new(lfo, knobs[3], drive).with_depth(Depth(0.2)));
            app.update();
            app.world_mut()
                .entity_mut(knobs[2])
                .insert(DriveParam::new(Drive(6.0)));
        }
        app.update();
        app.update();

        let mut live = [0.0f32; 4];
        app.world_mut()
            .resource_mut::<AudioGraphRes>()
            .render_frame(&mut live);
        assert_eq!(live[0], 4.0, "an AudioParam write reaches the live node");
        assert_eq!(live[1], 5.0, "a set_param write reaches the live node");
        let graph = app.world().resource::<AudioGraphRes>();
        let mut fork = graph.fork().expect("every knob is forkable");
        let mut forked = [0.0f32; 4];
        fork.tick(&[], &mut forked);
        assert_eq!(forked[0], 4.0, "an AudioParam write reaches the fork");
        assert_eq!(forked[1], 5.0, "a set_param write reaches the fork");
        #[cfg(feature = "modulation")]
        {
            assert_eq!(forked[2], 6.0, "a modulated param's base reaches the fork");
            assert_eq!(
                forked[3], 2.5,
                "the base a modulation rebuild seeds from the range reaches the fork"
            );
            assert!(
                (live[2] - 6.0).abs() > 0.5,
                "the live knob is modulated off its base (got {}), so the fork's \
                 plain base is the exception at work, not a driver that did nothing",
                live[2]
            );
        }
    }

    /// **A crossfade asked for while the graph re-prepares waits, lands once
    /// it resumes, and swaps the entity's controls only then** — the graph an
    /// engine runs (the audio side is taken) changing rate, and a crossfade
    /// before the executor has handed its units back.
    ///
    /// `Editor::replace` refuses while re-preparing and consumes its unit, so
    /// without the wait the new unit would be lost; binding the new controls
    /// at request time would leave the entity steering a unit that is not
    /// playing (or, on a poisoned graph, never will).
    ///
    /// Mutations (run):
    /// - `apply_crossfade` dropping the unit on `Busy` instead of parking it →
    ///   the old knob plays on, and the final value is 1, not 3;
    /// - binding the captured controls on `Busy` (and parking the request
    ///   without them) → while the crossfade waits the entity no longer
    ///   steers the playing unit (its handle is the pending unit's, then
    ///   gone), and the seeding fails (under `modulation`, which is what
    ///   captures a handle).
    #[test]
    fn a_crossfade_during_a_re_prepare_lands_after_it() {
        let mut graph = AudioGraphRes::headless(0, 1);
        graph.set_sample_rate(tutti_core::SampleRate(48_000.0));
        let mut side = graph.take_audio_side();
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
        }
        let knob = app
            .world_mut()
            .commands()
            .spawn_audio_node(Knob::new())
            .id();
        app.world_mut()
            .commands()
            .insert_resource(MasterSources::default().with(0, PortSource::node(knob)));
        app.world_mut().flush();
        app.update();
        let mut out = [0.0f32];
        side.tick(&[], &mut out);
        assert_eq!(out[0], Knob::BUILT_WITH, "the knob plays");

        // The rate changes on the running graph: the first half is sent, and
        // the executor has not run it yet.
        app.world_mut()
            .resource_mut::<AudioGraphRes>()
            .set_sample_rate(tutti_core::SampleRate(44_100.0));
        crate::graph::crossfade_audio_node(&mut app.world_mut().commands(), knob, Knob::at(3.0));
        app.world_mut().flush();
        assert_eq!(
            app.world()
                .resource::<crate::graph::PendingCrossfades>()
                .len(),
            1,
            "the crossfade waits for the re-prepare"
        );
        // Frame by frame through the re-prepare: the executor checks its units
        // out (a silent block), the editor sends them back, the executor
        // resumes. The crossfade still waits until a frame finds the graph
        // resumed.
        app.update();
        side.tick(&[], &mut out);
        app.update();
        // Still the old unit's controls: seed its cell through the entity's
        // handle, and the resumed old knob plays the seed.
        #[cfg(feature = "modulation")]
        seed(&app, knob, 7.0);
        side.tick(&[], &mut out);
        #[cfg(feature = "modulation")]
        assert_eq!(out[0], 7.0, "the entity still steers the playing unit");

        app.update();
        assert!(
            app.world()
                .resource::<crate::graph::PendingCrossfades>()
                .is_empty(),
            "landed once the graph resumed"
        );
        for _ in 0..512 {
            side.tick(&[], &mut out);
        }
        assert_eq!(out[0], 3.0, "the incoming knob is what sounds");
        // Its controls are the entity's now.
        #[cfg(feature = "modulation")]
        {
            seed(&app, knob, 8.0);
            side.tick(&[], &mut out);
            assert_eq!(out[0], 8.0, "the entity steers the incoming unit");
        }
    }

    /// A node that cannot run above 90 kHz: its `prepare` panics there,
    /// which is how a re-prepare poisons an editor.
    struct Grenade;

    impl tutti_graph::Node for Grenade {
        fn shape(&self) -> tutti_graph::Shape {
            tutti_graph::Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
        }
        fn prepare(&mut self, prepare: &Prepare) {
            assert!(prepare.sample_rate().get() < 90_000.0, "no such rate");
        }
        fn process(
            &mut self,
            _: &tutti_graph::Cx<'_>,
            mut io: tutti_graph::Io<'_>,
        ) -> tutti_graph::Status {
            io.output(0).fill(0.0);
            tutti_graph::Status::Modified
        }
        fn reset(&mut self) {}
    }

    /// **On a poisoned graph a crossfade is refused and logged**: nothing is
    /// parked (no frame will ever take it) and the entity's controls are left
    /// as they were — the incoming unit's are not bound to a node it never
    /// reached.
    ///
    /// Mutation (run): `GraphRuntime::replace` answering `Busy` on a poisoned
    /// graph → the request is parked for good, and this fails.
    #[test]
    fn a_crossfade_on_a_poisoned_graph_is_refused_and_keeps_the_controls() {
        let mut graph = AudioGraphRes::headless(0, 1);
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
        }
        let mut commands = app.world_mut().commands();
        let knob = commands.spawn_audio_node(Knob::new()).id();
        commands.spawn_audio_node(tutti_graph::Unforkable(Grenade));
        app.world_mut().flush();
        app.update();
        // Both halves run here (the executor is local); the second panics in
        // the grenade's `prepare` and poisons the editor.
        app.world_mut()
            .resource_mut::<AudioGraphRes>()
            .set_sample_rate(tutti_core::SampleRate(96_000.0));
        #[cfg(feature = "modulation")]
        let before = handle_ptr(&app, knob);

        crate::graph::crossfade_audio_node(&mut app.world_mut().commands(), knob, Knob::at(3.0));
        app.world_mut().flush();
        app.update();
        assert!(
            app.world()
                .resource::<crate::graph::PendingCrossfades>()
                .is_empty(),
            "a poisoned graph never takes it, so nothing waits"
        );
        #[cfg(feature = "modulation")]
        assert_eq!(handle_ptr(&app, knob), before, "the controls stayed put");
    }

    /// The address of `entity`'s captured params, to tell one capture from
    /// another.
    #[cfg(feature = "modulation")]
    fn handle_ptr(app: &App, entity: bevy_ecs::entity::Entity) -> *const () {
        let handle = app
            .world()
            .get::<crate::modulation::ModParamsHandle>(entity)
            .expect("a knob's params are captured");
        std::ptr::from_ref(handle.params()).cast()
    }

    /// Seed `entity`'s `Drive` through its captured modulation handle: an
    /// `AtomicTarget` writes its base into the unit's cell as it is built.
    #[cfg(feature = "modulation")]
    fn seed(app: &App, entity: bevy_ecs::entity::Entity, value: f32) {
        let handle = app
            .world()
            .get::<crate::modulation::ModParamsHandle>(entity)
            .expect("a knob's params are captured");
        handle
            .params()
            .mod_target(
                tutti_types::ParamAddr::Unit(UnitParam::Drive),
                value,
                0.0,
                10.0,
            )
            .expect("a knob answers on Drive");
    }
}
