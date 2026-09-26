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
//! | `insert`, `insert_with` | `Legacy::controlled` at a [`NodeKey`] minted from a fresh `NodeId`, forking as its captured MIDI port asks |
//! | `set_source`, `set_output_source`, `widen_outputs` | written into `editor.spec_mut()` |
//! | `set_param` | the node's `LegacyControls` (its settings ring, and its shadow) |
//! | `replace` | `Editor::replace` with a [`Fade`]; a plain `insert` when there is nothing to fade from |
//! | `node_latency`, `node_tail`, `node_inputs`, `node_outputs` | the editor's [`Shapes`](tutti_graph::Shapes) |
//! | `latency_plan` | `tutti_types::latency::plan` over the spec's topology |
//! | `compensate` | compiles the spec, reads `Plan::compensation` / `total_latency`; inserts nothing |
//! | `commit` | `Editor::commit`, which collects first |
//! | `set_node_latency` | `Editor::set_latency` |
//! | `insert_plugin` | `Legacy::controlled`, handed to the editor with the plugin's own fork source |
//! | `export` | `Editor::fork` in `ForkMode::Offline`, through `tutti_export::RenderGraph::fork` |
//!
//! # Every unit is `controlled`, none `pure`
//!
//! `insert` takes any `AudioUnit`, and nothing about one says whether its
//! output is a function of its audio inputs alone — a SoundFont fed through
//! its MIDI port looks exactly like a filter from here. A `pure` claim that is
//! wrong parks the unit for good the first time it is quiet (`tutti_graph`'s
//! `legacy` module docs), so this adapter never makes one: every unit is
//! [`Legacy::controlled`], which runs every block and gives `set_param` its
//! settings ring. The price is the silence skip, which the `Net` this
//! replaced never had either.
//!
//! # Where the audio side lives
//!
//! `Editor::new` builds the pair. The executor stays here ("local") until
//! [`take_executor`](NativeGraph::take_executor) hands it to the engine or to
//! a test's [`AudioSide`](super::AudioSide). While it is local, a commit is
//! applied at once on this thread and its box collected, so a headless graph
//! never meets back-pressure, and [`render_frame`](NativeGraph::render_frame)
//! runs it.
//!
//! # `set_param` lands on the next block, never at once
//!
//! There is no control-side copy of a node for a setting to land on at once
//! (a `Net` with no audio side, before PR 13, applied one straight to its only
//! copy, so a test could read an atomic back the moment it wrote it). A
//! setting goes into the node's ring and reaches the unit at the start of
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
    CommitError, Editor, Executor, Fade, Legacy, LegacyControls, NodeParts, ParamFrom, ParamIn,
    ParamMod, ParamRange, ParamShaping, Prepare, Resolution, Transport,
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

/// The spec `kind` of a unit this adapter inserted. Nothing builds a unit from
/// it (the same reason `topology::ENTITY_NODE_KIND` exists); it names the
/// layer in a debugger.
const UNIT_KIND: &str = "bevy-tutti:unit";

/// The spec `kind` of the engine's beat generator.
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
    fn render_fault(&self) -> Option<std::sync::Arc<dyn tutti_core::RenderFault>> {
        self.0.render_fault()
    }
    fn param_feed(&mut self) -> Option<&mut tutti_core::ParamFeed> {
        self.0.param_feed()
    }
    fn param_base(&self, k: usize) -> Option<f32> {
        self.0.param_base(k)
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

/// Why [`AudioGraphRes::replace`](super::AudioGraphRes::replace) did not
/// take a unit.
pub enum ReplaceRefused {
    /// Not now: the graph is re-preparing (a sample-rate or block-size change
    /// between its two commits). The unit is handed back, untouched; retry
    /// once the re-prepare has resumed —
    /// [`crossfade_audio_node`](super::crossfade_audio_node) keeps it pending
    /// and does.
    Busy(Box<dyn AudioUnit>),
    /// Never: the graph is poisoned (a re-prepare failed with its units out),
    /// or the node is not in it. The unit was dropped.
    Failed(String),
}

impl std::fmt::Debug for ReplaceRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy(_) => f.write_str("Busy(..)"),
            Self::Failed(why) => f.debug_tuple("Failed").field(why).finish(),
        }
    }
}

/// One node's handle, and its controls when it has a settings path.
struct Entry {
    node: AudioNode,
    /// `None` for a native node (the beat generator), which takes no
    /// settings and has no `AudioUnit` to inspect.
    controls: Option<LegacyControls<Boxed>>,
    /// Whether a fork of the unit now at the key carries the MIDI clip on
    /// its live port: it went in with the fork its captured port asks for
    /// (`insert_with`), or is a plugin (whose fork source carries its own).
    /// A node an export reaches that has a captured port and not this is
    /// refused (`fork_for_export`) — its fork would drop the clip.
    carries_midi: bool,
}

/// The executor, while this side still holds it, and what a local render
/// needs beside it.
pub(crate) struct Local {
    exec: Executor,
    /// Planar scratch, one `Vec` per global output.
    scratch: Vec<Vec<f32>>,
}

/// The graph runtime. See the module docs.
pub(crate) struct NativeGraph {
    editor: Editor,
    local: Option<Local>,
    nodes: BTreeMap<NodeKey, Entry>,
    /// Edited since the last commit — only a local render reads it, to
    /// commit before it renders, so a local render plays the graph as
    /// edited, uncommitted edits included.
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
/// [`AudioNode`] keeps wrapping a `NodeId`.
pub(crate) fn key(node: AudioNode) -> NodeKey {
    NodeKey(node.0.value())
}

/// The latency `Legacy` will declare for `unit` at `rate`, by `Legacy`'s own
/// probe (`tutti-graph/src/legacy.rs`, `Adapter::probe`): `latency()` after
/// `set_sample_rate`, rounded to the nearest frame, never negative. Asked
/// before a replace, because a refused `Editor::replace` consumes its node.
fn probe_latency(unit: &mut dyn AudioUnit, rate: SampleRate) -> Latency {
    unit.set_sample_rate(rate);
    declared_latency(unit)
}

/// `unit.latency()` as `Legacy` declares it: rounded to the nearest frame,
/// never negative (`Adapter::probe`, whose rounding is `Net`'s own).
fn declared_latency(unit: &mut dyn AudioUnit) -> Latency {
    Latency::new(Samples(
        unit.latency().unwrap_or(0.0).round().max(0.0) as usize
    ))
}

/// How a unit's fork carries the MIDI clip on its live port, as the MIDI
/// registry captured it (`CapturedControls`). Without the `midi` feature no
/// unit has a captured port, and every unit forks from its shadow.
#[cfg(feature = "midi")]
pub(crate) type UnitFork = crate::midi::MidiFork;
/// See the `midi` build's `UnitFork`: here there is none.
#[cfg(not(feature = "midi"))]
pub(crate) enum UnitFork {}

/// `unit` as every unit goes in — [`Legacy::controlled`]: its node, settings
/// ring and shadow — forking as `fork` says: from the unit's own source, or
/// from the shadow with a hook carrying its MIDI clip (see
/// `MidiNode::fork_source`). With no `fork`, from the shadow alone, as
/// `Legacy` forks every forkable unit.
fn controlled(
    editor: &mut Editor,
    unit: Box<dyn AudioUnit>,
    fork: Option<UnitFork>,
) -> (NodeParts<()>, LegacyControls<Boxed>) {
    let (legacy, controls) = Legacy::controlled(editor, Boxed(unit));
    let parts = match fork {
        None => tutti_graph::IntoNode::into_parts(legacy),
        #[cfg(feature = "midi")]
        Some(UnitFork::Carry(hook)) => {
            tutti_graph::IntoNode::into_parts(legacy.with_fork_hook(hook))
        }
        #[cfg(feature = "midi")]
        Some(UnitFork::Own(own)) => {
            let mut parts = tutti_graph::IntoNode::into_parts(legacy);
            parts.fork = Some(own);
            parts
        }
    };
    (parts, controls)
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

    /// The editor, for `Engine::new`.
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

    /// Re-prepare every node for `rate` and, if given, `max_block` (else the
    /// block it has): the first half of `Editor::reprepare`, sent. The
    /// executor checks its units out on its next block, and a later
    /// `collect` sends them back re-prepared (every frame's `commit_graph`
    /// does it). A no-op when neither moves.
    pub(crate) fn reprepare(
        &mut self,
        rate: SampleRate,
        max_block: Option<Samples>,
    ) -> Result<(), CommitError> {
        let current = *self.editor.prepare();
        let prepare = Prepare::new(rate, max_block.unwrap_or(current.max_block().samples()));
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

    /// Insert `unit`, forking as `fork` says (see [`controlled`]): the fork
    /// its captured MIDI port asks for, or `None` for a unit with no port.
    pub(crate) fn insert(&mut self, unit: Box<dyn AudioUnit>, fork: Option<UnitFork>) -> AudioNode {
        let node = AudioNode(NodeId::new());
        let carries_midi = fork.is_some();
        let (parts, controls) = controlled(&mut self.editor, unit, fork);
        self.editor.insert(key(node), UNIT_KIND, parts);
        self.nodes.insert(
            key(node),
            Entry {
                node,
                controls: Some(controls),
                carries_midi,
            },
        );
        self.edited = true;
        node
    }

    /// Insert a hosted plugin so that a fork of the graph (an export) can
    /// fork it: `Legacy::controlled`'s node, settings ring and shadow, as
    /// [`insert`](Self::insert) gives every unit, handed to the editor with
    /// the plugin's own [`ForkSource`](tutti_graph::ForkSource) (a fork by
    /// state transfer, `PluginClient::fork_source`).
    ///
    /// Not through `insert`: a boxed plugin in a `Legacy` has no fork source
    /// (its `AudioUnit::forkable` is `false` — its clones share the one
    /// plugin process), so a fork of any graph holding it would be refused
    /// as not forkable. Nor through `IntoNode for PluginClient`, which has
    /// no settings ring and no shadow, and the latency re-probe
    /// ([`refresh_node_latency`](Self::refresh_node_latency)) and
    /// [`inspect`](Self::inspect) read the shadow.
    #[cfg(feature = "plugin")]
    pub(crate) fn insert_plugin(
        &mut self,
        client: Box<tutti_plugin::handles::PluginClient>,
    ) -> AudioNode {
        let node = AudioNode(NodeId::new());
        let fork = client.fork_source();
        let (legacy, controls) = Legacy::controlled(&mut self.editor, Boxed(client));
        let parts = tutti_graph::IntoNode::into_parts(legacy);
        debug_assert!(
            parts.fork.is_none(),
            "a plugin forks by state transfer, never by clone"
        );
        self.editor.insert(
            key(node),
            UNIT_KIND,
            tutti_graph::NodeParts {
                node: parts.node,
                controls: (),
                fork: Some(fork),
            },
        );
        self.nodes.insert(
            key(node),
            Entry {
                node,
                controls: Some(controls),
                // Its fork source carries the clip itself
                // (`PluginClient::fork_source`).
                carries_midi: true,
            },
        );
        self.edited = true;
        node
    }

    /// A copy of `target` for an offline render at `rate`, sharing no state
    /// with this graph: `Editor::fork` with `ForkMode::Offline(ctx)`, through
    /// tutti-export (`RenderGraph::fork`, which prepares it at the render's
    /// rate and `GRAPH_MAX_BLOCK`). `ctx` is handed over as the
    /// `&OfflineTransport` itself — the exact type every unit's
    /// `rebind_offline` downcasts; anything else would rebind nothing,
    /// silently (`ForkMode::Offline`'s docs).
    ///
    /// `midi` is every node with a captured MIDI port (its entity's
    /// `MidiTarget`). One the fork holds that went in **without** the fork
    /// its port asks for — a host that captured the unit's controls, then
    /// pushed it with the plain [`insert`](Self::insert) and bound them —
    /// would fork from its shadow and drop its clip, silently. It refuses
    /// the export instead (`NotForkable`, naming its entity).
    #[cfg(feature = "export")]
    pub(crate) fn fork_for_export(
        &self,
        target: tutti_graph::ForkTarget,
        ctx: &tutti_core::transport::OfflineTransport,
        rate: SampleRate,
        midi: &std::collections::BTreeSet<NodeKey>,
    ) -> tutti_export::Result<tutti_export::RenderGraph> {
        let graph = tutti_export::RenderGraph::fork(
            &self.editor,
            target,
            tutti_graph::ForkMode::Offline(ctx),
            rate,
        )?;
        // The keys the fork holds are exactly what it forked.
        let dropped = graph.editor().spec().topology.nodes.keys().find(|k| {
            self.nodes
                .get(k)
                .is_some_and(|e| !e.carries_midi && midi.contains(k))
        });
        if let Some(&key) = dropped {
            return Err(tutti_export::Error::NotForkable { key });
        }
        Ok(graph)
    }

    /// The beat generator a graph engine needs in place of a
    /// `TransportClock`, which the graph must not hold (see `Engine::new`:
    /// the engine drives its own).
    ///
    /// Forkable by clone (`ForkByClone`): it is a unit struct that reads only
    /// its block's `Env`, so a clone shares nothing, and a fork's renderer
    /// hands it the render's transport. Inserted plainly it would have no
    /// fork source, and every engine-built graph (whose click and beat-driven
    /// nodes it feeds) would refuse a master export as not forkable.
    pub(crate) fn insert_env_clock(&mut self) -> AudioNode {
        let node = AudioNode(NodeId::new());
        self.editor.insert(
            key(node),
            ENV_CLOCK_KIND,
            tutti_graph::ForkByClone(EnvClock::new()),
        );
        self.nodes.insert(
            key(node),
            Entry {
                node,
                controls: None,
                carries_midi: false,
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
    /// only from a unit that is running (`Net::crossfade`, before PR 13,
    /// asked neither). So
    /// where the fade cannot be had — the node is not committed yet, or the
    /// new unit declares another latency — this lands the unit with
    /// `Editor::insert` instead: the same key, every edge kept, heard as a
    /// swap on the commit's block. Checked here rather than by trying,
    /// because a refused `replace` has already consumed the unit.
    ///
    /// Refused, handing `unit` back, while a re-prepare is between its two
    /// commits (`Editor::replace` would consume it and refuse): the caller
    /// keeps it and retries once the re-prepare has resumed. Refused for good
    /// on a poisoned editor, where no unit can ever land again.
    ///
    /// `fork` is the incoming unit's (see [`insert`](Self::insert)), taken
    /// only when the unit lands, so a refused unit keeps it for its retry.
    pub(crate) fn replace(
        &mut self,
        node: AudioNode,
        mut unit: Box<dyn AudioUnit>,
        fade: Seconds,
        curve: CrossfadeCurve,
        fork: &mut Option<UnitFork>,
    ) -> Result<(), ReplaceRefused> {
        if let Some(cause) = self.editor.poisoned() {
            return Err(ReplaceRefused::Failed(format!(
                "the graph is poisoned ({cause}); build a new one"
            )));
        }
        if self.editor.is_repreparing() {
            return Err(ReplaceRefused::Busy(unit));
        }
        let k = key(node);
        let Some(entry) = self.nodes.get(&k) else {
            return Err(ReplaceRefused::Failed(format!(
                "{node:?} is not in the graph"
            )));
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
        let fork = fork.take();
        let carries_midi = fork.is_some();
        let (legacy, controls) = controlled(&mut self.editor, unit, fork);
        if fits {
            let fade = Fade::seconds(fade, rate, curve);
            if let Err(e) = self.editor.replace(k, legacy, fade) {
                // Every refusal `Editor::replace` has is checked above
                // (poisoned, repreparing, not running, shape); one here is
                // this module's bug, and the unit is gone with it.
                debug_assert!(false, "a checked replace was refused: {e}");
                return Err(ReplaceRefused::Failed(e.to_string()));
            }
        } else {
            let kind = self.editor.spec().topology.nodes[&k].kind.clone();
            self.editor.insert(k, &kind, legacy);
        }
        if let Some(entry) = self.nodes.get_mut(&k) {
            entry.controls = Some(controls);
            entry.carries_midi = carries_midi;
        }
        self.edited = true;
        Ok(())
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
    /// shaped), clamped to `range`: replaces whatever modulated it.
    pub(crate) fn set_param_mod(
        &mut self,
        node: AudioNode,
        param: UnitParam,
        sources: &[(AudioNode, ParamShaping)],
        range: ParamRange,
    ) {
        let at = ParamIn {
            node: key(node),
            param,
        };
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

    /// The compensation of the plan sent last: what `commit_graph` publishes,
    /// since it is what the executor is handed.
    pub(crate) fn sent_compensation(&self) -> Option<(Vec<Samples>, Samples)> {
        let plan = self.editor.base()?;
        Some((plan.compensation().to_vec(), plan.total_latency().samples()))
    }

    /// A node's latency may have moved at runtime (a plugin's latency cell):
    /// ask its shadow again, and if the answer differs, move the editor's
    /// figure (`Editor::set_latency`) so the next commit moves PDC to it,
    /// without touching the running unit.
    ///
    /// Asked of the shadow — a clone of the unit — rather than handed a figure,
    /// because the node's latency is the *unit's* declaration: a hosted
    /// plugin's is its own latency cell **plus** the block its pipeline holds
    /// (`tutti-plugin`'s `route`), and only the unit knows the second term.
    /// That is how `Net` read it too (a clone sharing the cell). It holds
    /// while the unit's `isolate` leaves the latency cell shared with the
    /// shadow, which `PluginClient`'s does (it isolates nothing: its clones
    /// share the plugin); `tests/plugin_capture.rs` pins the figure.
    #[cfg(feature = "plugin")]
    pub(crate) fn refresh_node_latency(&mut self, node: AudioNode) {
        let Some(controls) = self.nodes.get(&key(node)).and_then(|e| e.controls.as_ref()) else {
            return;
        };
        // Clamped to what PDC compensates, as the editor clamps a latency it
        // probes at insert: `set_latency` refuses a figure past it, and a
        // refused one would never be retried (the poll only fires again when
        // the plugin's figure moves).
        let latency = Latency::new(
            declared_latency(&mut *controls.shadow())
                .samples()
                .min(tutti_types::latency::MAX_NODE_LATENCY),
        );
        if self.node_latency(node) == latency.samples() {
            return;
        }
        match self.editor.set_latency(key(node), latency) {
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
            "render_frame needs the audio side, and it was taken; \
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
    pub(crate) fn native(exec: Executor) -> Self {
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

    /// Render `frames` frames with no global input into `output`, planar,
    /// one slice per global output, in the blocks a device would hand over:
    /// executor blocks of up to `block` frames. The transport is stopped at
    /// beat zero.
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
    use std::sync::Arc;

    use bevy_app::prelude::*;
    use tutti_core::{AtomicF32, Drive, Ordering, Signal};

    use crate::graph::{
        AudioGraphRes, AudioParam, AudioParamAppExt, GraphReconcilePlugin, MasterSources,
        PortSource, SpawnAudioNode,
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
            Self::at(Self::BUILT_WITH)
        }

        fn at(drive: f32) -> Self {
            Self {
                drive: Arc::new(AtomicF32::new(drive)),
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
    /// node** — the constraint export by `Editor::fork`
    /// (doc 013, PR 12) rests on: a fork is cloned from each node's shadow, so a
    /// write that bypasses the settings ring (a captured handle, a shared cell)
    /// moves the live unit and leaves the fork at the value it was built with.
    ///
    /// The paths, one knob each, rendered on one global output each:
    /// - `AudioParam` on an unmodulated param (`write_param` → `set_param`);
    /// - `AudioGraphRes::set_param` called directly;
    /// - `AudioParam` on a param the control-rate driver owns
    ///   (`write_param` → `ModulationMatrix::set_base`): the fork carries the
    ///   authored **base**; and a param the driver owns with no write at all
    ///   carries the base its rebuild seeded from `ModParamRange`. The
    ///   driver's live offset is the exception — a fork
    ///   (an export) runs its own modulation — and the live render shows it is
    ///   there, so the fork's plain base is not the driver doing nothing.
    ///
    /// An audio-rate modulated param is the first path: the graph's param
    /// modulation rides on the node's own control (design doc 013 item 6),
    /// so its authored base goes through `set_param` like any unmodulated
    /// write. (It used to live in a base chain's cell no `Setting` reached,
    /// which a fork could not see.)
    ///
    /// Mutations (run; each fails its channel):
    /// - `write_param`'s unmodulated arm not going through `graph.set_param`
    ///   (the value lands wherever a captured handle points, which the shadow
    ///   never sees) → channel 0 stays at 1;
    /// - `write_param` dropping `set_param_snapshot` after `set_base` (the
    ///   base reaches only the driver's accumulator) → channel 2 stays at 1;
    /// - the modulation `rebuild` dropping its `set_param_snapshot` of
    ///   `range.base` → channel 3 stays at 1.
    ///
    /// Channel 1 pins `set_param` itself; a mutation of it is `LegacyControls`'
    /// own (`tutti-graph`'s `legacy` tests), since this crate only calls it.
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
            // Before any knob is bound: the registry is read at capture.
            app.world_mut()
                .resource_mut::<crate::modulation::ModTargetRegistry>()
                .register::<Knob>();
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
            // Path 4: the base the modulation rebuild seeds from the declared
            // range, with no write at all — 2.5, not the 1 the knob was built
            // with.
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
            let mut live = [0.0f32; 4];
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
            app.world_mut()
                .resource_mut::<crate::modulation::ModTargetRegistry>()
                .register::<Knob>();
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
        crate::graph::crossfade_audio_node(
            &mut app.world_mut().commands(),
            knob,
            Box::new(Knob::at(3.0)),
        );
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

    /// A unit that cannot run above 90 kHz: its `set_sample_rate` panics
    /// there, which is how a re-prepare poisons an editor.
    #[derive(Clone)]
    struct Grenade;

    impl AudioUnit for Grenade {
        fn set_sample_rate(&mut self, rate: tutti_core::SampleRate) {
            assert!(rate.get() < 90_000.0, "no such rate");
        }
        fn tick(&mut self, _: &[f32], output: &mut [f32]) {
            output[0] = 0.0;
        }
        fn process(&mut self, _: usize, _: &BufferRef, _: &mut BufferMut) {}
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
    }

    /// **On a poisoned graph a crossfade is refused and logged**: nothing is
    /// parked (no frame will ever take it) and the entity's controls are left
    /// as they were — the incoming unit's are not bound to a node it never
    /// reached.
    ///
    /// Mutation (run): `NativeGraph::replace` answering `Busy` on a poisoned
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
            app.world_mut()
                .resource_mut::<crate::modulation::ModTargetRegistry>()
                .register::<Knob>();
        }
        let mut commands = app.world_mut().commands();
        let knob = commands.spawn_audio_node(Knob::new()).id();
        commands.spawn_audio_node(Grenade);
        app.world_mut().flush();
        app.update();
        // Both halves run here (the executor is local); the second panics in
        // the grenade's `prepare` and poisons the editor.
        app.world_mut()
            .resource_mut::<AudioGraphRes>()
            .set_sample_rate(tutti_core::SampleRate(96_000.0));
        #[cfg(feature = "modulation")]
        let before = handle_ptr(&app, knob);

        crate::graph::crossfade_audio_node(
            &mut app.world_mut().commands(),
            knob,
            Box::new(Knob::at(3.0)),
        );
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
            .expect("a registered knob has a handle");
        std::ptr::from_ref(handle.params()).cast()
    }

    /// Seed `entity`'s `Drive` through its captured modulation handle: an
    /// `AtomicTarget` writes its base into the unit's cell as it is built.
    #[cfg(feature = "modulation")]
    fn seed(app: &App, entity: bevy_ecs::entity::Entity, value: f32) {
        let handle = app
            .world()
            .get::<crate::modulation::ModParamsHandle>(entity)
            .expect("a registered knob has a handle");
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
