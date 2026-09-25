//! Declaring what feeds what.
//!
//! A node added to the graph is unwired and renders nothing. What reaches its
//! input ports is declared with [`PortSources`] on the sink entity; what
//! reaches the speakers is declared with the [`MasterSources`] resource.
//!
//! ```rust
//! use bevy_app::prelude::*;
//! use bevy_ecs::prelude::*;
//! use bevy_tutti::prelude::*;
//! use tutti_core::{ChannelLayout, Hz, Q};
//! use tutti_nodes::testing::Osc;
//! use tutti_nodes::{SvfFilterNode, SvfType};
//!
//! fn build(mut commands: Commands) {
//!     // Stereo throughout, so `MasterSources::from` has two output ports to take.
//!     let tone = Osc::sine(Hz(440.0)).with_layout(ChannelLayout::STEREO);
//!     let osc = commands.spawn_audio_node(tone).id();
//!     let filt = commands
//!         .spawn_audio_node(SvfFilterNode::<f64>::with_channels(
//!             ChannelLayout::STEREO,
//!             SvfType::LowPass,
//!             Hz(1000.0),
//!             Q(1.0),
//!         ))
//!         .insert(PortSources::stereo_from(osc))
//!         .id();
//!     commands.insert_resource(MasterSources::from(filt));
//! }
//!
//! let mut app = App::new();
//! app.insert_resource(AudioGraphRes::headless(0, 2));
//! app.insert_resource(AudioEngineState::Running);
//! app.add_plugins(GraphReconcilePlugin);
//! app.add_systems(Startup, build);
//! app.update();
//!
//! // Read the edges back off the engine — the declaration's actual effect,
//! // as opposed to `LiveGraph`, which is what this layer meant to write.
//! let graph = app.world().resource::<AudioGraphRes>();
//! assert!(matches!(graph.output_source(0), GraphSource::Node(_, 0)));
//! assert!(matches!(graph.output_source(1), GraphSource::Node(_, 1)));
//! ```
//!
//! # Why the sink owns the declaration
//!
//! The graph is a *total function from input port to source*: every port —
//! `(node, channel)` and `(global, channel)` — holds exactly one
//! [`GraphSource`], defaulting to `Silence`. There is no fan-in and no partial
//! state.
//!
//! So the declaration is keyed the way the engine is keyed. [`PortSources`] is
//! a component (one per entity, enforced by the ECS) whose index *i* is input
//! port *i* (one slot, enforced by the type). **Two sources into one port cannot
//! be expressed.** An edge-entity model could express it, and would resolve it
//! by archetype iteration order — a silent, nondeterministic last-write-wins.
//! Summing is a node's job: the graph has no summing bus and this layer must not
//! invent one.
//!
//! An edge-component model is also ruled out. Such a component needs a tracked
//! `HashMap<Entity, (NodeId, PortIndex)>` to know what to disconnect — adapter
//! shadow state mirroring the engine. The declaration *is* that record, and
//! [`LiveGraph`] holds what it last compiled to, so nothing here mirrors the
//! engine port by port.
//!
//! # The value is the truth for edges and outputs
//!
//! [`rebuild`] derives a [`Topology`](tutti_types::graph::Topology) from the
//! declarations, compares it against [`LiveGraph`] — one comparison, the whole
//! change detection — and, when it differs, writes the ports that differ through
//! `AudioGraphRes::set_source` / `set_output_source`. Those are the same calls this
//! module always made; what changed is that a *value* decides them rather than a
//! port-by-port re-read of the runtime.
//!
//! **Units are not the value's.** A node is added by
//! [`spawn_audio_node`](super::SpawnAudioNode) and removed by the
//! `On<Remove, AudioNode>` observer, exactly as before; the value names nodes by
//! [`NodeKey`](tutti_types::graph::NodeKey) and never builds one. That split is
//! deliberate and is what keeps a hosted plugin's C-pointer state, the sampler's
//! butler-shared buffers and a queued crossfade alive across a rebuild — see the
//! [`topology`] module docs for the two runtime constraints
//! behind it.
//!
//! # One writer per declared port
//!
//! A port named by a [`PortSources`] belongs to that declaration. Writing it
//! imperatively through [`AudioGraphRes::set_source`] as well is a bug in the host — and one
//! this layer now **detects and repairs**. An engine-side write leaves the
//! declaration untouched, so the value is unchanged and `want != live` is *not*
//! what catches it; what catches it is that the write lands on a port the value
//! names, and every such port is compared against the engine before being
//! written. The declaration is reasserted on the next rebuild.
//!
//! That is a real narrowing of the old hazard, not its removal. The repair still
//! waits for a rebuild, so an imperative value survives until one happens — it is
//! no longer "silently, and at an unpredictable moment", but it is not
//! instantaneous either. Declare the port, or own it — not both.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;

use tutti_core::AudioNode;
use tutti_core::{ChannelLayout, MAX_ROOT_CHANNELS};

use super::topology::{self, LiveGraph};
use super::{engine_ready, AudioGraphRes, GraphDirty, GraphReconcileSystems, GraphSource};

/// Where one input port's signal comes from.
///
/// [`GraphSource`] with the node named by *entity* rather than by
/// [`AudioNode`]. That one difference is load-bearing: a
/// [`crossfade`](super::crossfade_audio_node) keeps a node's `NodeId` but an id
/// stored on an entity is stale the moment anything else replaces the node, so
/// the declaration names the entity and [`rebuild`] re-derives the id every
/// time. It also means a host never handles an engine id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PortSource {
    /// Output `port` of the node bound to `entity`. Mirrors [`GraphSource::Node`].
    Node {
        /// The entity carrying the source node. Resolved to a `NodeId` on every
        /// rebuild, so a crossfade cannot strand it.
        entity: Entity,
        /// Which of that node's **output** ports to take.
        port: usize,
    },
    /// Global network input `port` — a hardware or host input channel.
    /// Mirrors [`GraphSource::Input`].
    Input {
        /// Index into the graph's global inputs. Out of range resolves to
        /// nothing rather than to silence.
        port: usize,
    },
    /// Silence. Mirrors [`GraphSource::Silence`], and what an unlisted port gets.
    #[default]
    Silence,
}

impl PortSource {
    /// Output port 0 of `entity` — the common case.
    pub fn node(entity: Entity) -> Self {
        Self::Node { entity, port: 0 }
    }
}

/// What feeds an entity's audio-node input ports. Index *i* is input port *i*.
///
/// A `Vec` shorter than the node's input count leaves the trailing ports
/// **undeclared** — this layer does not touch them, so whatever wired them keeps
/// them. To say "silent" and mean it, name the port with
/// [`PortSource::Silence`].
#[derive(Component, Debug, Clone, Default, PartialEq)]
pub struct PortSources(pub Vec<PortSource>);

impl PortSources {
    /// Declares nothing. Add ports with [`with`](Self::with).
    pub fn silent() -> Self {
        Self(Vec::new())
    }

    /// Port 0 from `entity`'s port 0.
    pub fn from(entity: Entity) -> Self {
        Self(vec![PortSource::node(entity)])
    }

    /// Ports 0 and 1 from `entity`'s ports 0 and 1.
    pub fn stereo_from(entity: Entity) -> Self {
        Self(vec![
            PortSource::Node { entity, port: 0 },
            PortSource::Node { entity, port: 1 },
        ])
    }

    /// Every channel of `layout` from `entity`'s matching port — the identity
    /// mapping, at any width.
    ///
    /// [`stereo_from`](Self::stereo_from) generalized. Without this, a wide sink
    /// writes a `.with()` fold at the call site, and every such fold is an
    /// opportunity to get the mapping wrong: the correct mapping is *identity*,
    /// and a hand-written loop invites `port % n` or an off-by-one start.
    ///
    /// **No wrapping and no fold**, for the same reason
    /// [`MasterSources::from`] gives at length: `pipe_output` wraps with
    /// `channel % node_outputs`, silently turning "route this" into "route
    /// this, duplicated". This holds an `Entity` rather than a graph, so it
    /// cannot see the arity it would wrap against. A declaration cannot fold;
    /// only a node can.
    pub fn from_node_at_width(entity: Entity, layout: impl Into<ChannelLayout>) -> Self {
        Self(
            (0..layout.into().count() as usize)
                .map(|port| PortSource::Node { entity, port })
                .collect(),
        )
    }

    /// Set one port, growing with [`PortSource::Silence`] to reach it.
    ///
    /// # Audio-rate param ports belong here too
    ///
    /// A node's audio-rate *param* port (a filter's cutoff, a distortion's
    /// drive) is an ordinary input port that happens to sit after the audio
    /// inputs — get its index from the unit's
    /// [`ParamPorts::param_port`](tutti_nodes::ParamPorts) before boxing it into
    /// the graph, then name it here like any other port.
    ///
    /// Declaring it here is not a stylistic preference. Wiring a param port
    /// imperatively is *fragile*: a wiring call that walks **every** input
    /// port of a node (as `Net::pipe_input` did, before the native graph) lets
    /// a later "wire the audio in" call silently
    /// overwrites a param edge with a global input — no error, no warning, the
    /// modulation just stops arriving.
    ///
    /// One `PortSources` per entity (the ECS enforces that) and one index per
    /// port makes that clobber unrepresentable: [`rebuild`] writes the whole
    /// declared range from a single `Vec`. A sibling `ParamSources` component
    /// would put two writers back in one port space and let archetype iteration
    /// order pick the winner — the silent last-write-wins this module refuses
    /// for audio fan-in.
    pub fn with(mut self, port: usize, source: PortSource) -> Self {
        self.set(port, source);
        self
    }

    /// [`with`](Self::with) against an existing value, for a caller holding
    /// `&mut Self`.
    ///
    /// The consuming builder is the right shape when assembling a declaration
    /// from nothing, and the wrong one when amending a component already in the
    /// world: reaching it through `&mut` costs a full clone of the port vector
    /// per amendment, which turns a loop over N changed routes into N clones of
    /// an N-element vector. `modulation::audio_rate` re-points shaper entities
    /// exactly that way.
    pub fn set(&mut self, port: usize, source: PortSource) {
        if self.0.len() <= port {
            self.0.resize(port + 1, PortSource::Silence);
        }
        self.0[port] = source;
    }
}

/// What feeds each global output channel. Index = channel.
///
/// Empty — the default — declares nothing, so a host that never writes this
/// keeps whatever it wired through [`AudioGraphRes`] itself. Once written, it is
/// the single declaration of what reaches the speakers, which is what makes "two
/// nodes both own the master" unrepresentable rather than a race.
///
/// **It declares every root channel, not just the ones its `Vec` reaches.** A
/// channel past its length is silent, so shrinking the declaration releases
/// the channels it dropped: replacing a stereo declaration with a one-channel
/// one disconnects whatever fed channel 1. The root keeps its width — it is the
/// device's, and a shrink is not a narrowing — while a *longer* declaration
/// widens it (see [`from_node_at_width`](Self::from_node_at_width)).
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct MasterSources(pub Vec<PortSource>);

impl MasterSources {
    /// A **stereo** source on both output channels: port 0 to channel 0, port 1
    /// to channel 1.
    ///
    /// For a source with fewer outputs, say so explicitly — a mono node feeding
    /// both channels is [`mono_from`](Self::mono_from). There is no wrapping
    /// here on purpose. `AudioGraphRes::set_outputs_from` wraps with `channel % node_outputs`,
    /// which silently turns "route this" into "route this, duplicated", and the
    /// arity it wraps against is the *node's*, which this constructor cannot see
    /// — it has an `Entity`, not a graph. Guessing wrong leaves a channel
    /// unresolvable, which [`rebuild`] skips, which strands whatever the channel
    /// held before.
    pub fn from(entity: Entity) -> Self {
        Self(vec![
            PortSource::Node { entity, port: 0 },
            PortSource::Node { entity, port: 1 },
        ])
    }

    /// A **mono** source on both output channels, from its port 0.
    pub fn mono_from(entity: Entity) -> Self {
        Self(vec![
            PortSource::Node { entity, port: 0 },
            PortSource::Node { entity, port: 0 },
        ])
    }

    /// Every channel of `layout` from `entity`'s matching port — the identity
    /// mapping, at any width.
    ///
    /// [`from`](Self::from) generalized past stereo, with the same refusal to
    /// wrap or fold and for the same reason. A declaration wider than the root
    /// **widens the root** ([`rebuild`]) rather than being truncated, so this
    /// is how a host asks for a surround master.
    ///
    /// [`mono_from`](Self::mono_from) is deliberately not expressible through
    /// this: it maps two channels to one port, which is a duplication, not an
    /// identity.
    pub fn from_node_at_width(entity: Entity, layout: impl Into<ChannelLayout>) -> Self {
        Self(
            (0..layout.into().count() as usize)
                .map(|port| PortSource::Node { entity, port })
                .collect(),
        )
    }

    /// One channel from one of `entity`'s ports.
    pub fn with(mut self, channel: usize, source: PortSource) -> Self {
        if self.0.len() <= channel {
            self.0.resize(channel + 1, PortSource::Silence);
        }
        self.0[channel] = source;
        self
    }
}

/// Compile every declaration into the graph, writing only what differs.
///
/// # What counts as a change
///
/// A declaration edit, a removal, a [`MasterSources`] edit — and a *touched
/// [`AudioNode`]*, which is the non-obvious one, for two reasons. A declaration
/// routinely names an entity whose node arrives a frame later, and nothing about
/// the declaration changes when it does. And an entity can be *re-bound* to a
/// different node, which must re-derive every wire naming it — that is the whole
/// reason [`PortSource::Node`] holds an `Entity` rather than a `NodeId`.
///
/// The gate is `Changed<AudioNode>`, not `Added`: a replacement `insert` on an
/// entity that already has the component fires `Changed` but **not** `Added`, so
/// an `Added` gate leaves a re-bound entity's wires pointing at the retired node
/// forever. `Added` is a subset of `Changed`, so this covers arrival too.
///
/// It is **also** `RemovedComponents<AudioNode>`, and that arm is not a
/// belt-and-braces addition — `Changed` does not report a removal, and a removal
/// is a real graph edit. `remove::<AudioNode>()` without a despawn takes the
/// node out of the engine (the `On<Remove, AudioNode>` observer calls
/// `AudioGraphRes::remove`, which zeroes every edge to and from it) while leaving the
/// entity, its `PortSources` and every declaration naming it untouched. None of
/// the other four arms fires, so before this arm existed the pass simply did not
/// run: the engine was repaired and the declaration side was never re-derived.
///
/// That was invisible while the only record of the graph was the engine itself —
/// there was nothing to be stale. It is visible the moment a value records what
/// the declarations mean, which is how it was found.
///
/// The gate is still a gate, not the change detection. It answers "is it worth
/// deriving the value at all", cheaply, from change ticks; the value comparison
/// below answers "did anything actually move". Both are needed: without the
/// gate every frame pays for a `Topology`, and without the comparison an edit
/// that cancels out would still write the graph.
///
/// # Why a diff rather than a wholesale replace
///
/// The value decides *what* the graph is, and [`topology::apply`] writes only
/// the ports whose runtime source differs from it, so a rebuild that moves
/// nothing leaves the graph clean: no `GraphDirty`, so no compile and no
/// commit. And a wholesale replace would also clear the ports the declaration
/// does not name, which belong to whoever wired them (see [`PortSources`]).
#[allow(
    clippy::too_many_arguments,
    reason = "Bevy systems declare their data access as parameters; each one here \
              is a distinct query or resource the rebuild genuinely needs"
)]
pub fn rebuild(
    graph: Option<ResMut<AudioGraphRes>>,
    dirty: Option<ResMut<GraphDirty>>,
    live: Option<ResMut<LiveGraph>>,
    nodes: Query<(Entity, &AudioNode)>,
    sinks: Query<(Entity, &PortSources)>,
    master: Res<MasterSources>,
    changed: Query<(), Changed<PortSources>>,
    rebound: Query<(), Changed<AudioNode>>,
    mut removed: RemovedComponents<PortSources>,
    mut unbound: RemovedComponents<AudioNode>,
    #[cfg(feature = "modulation")] shaping: Query<&crate::modulation::audio_rate::ShaperShaping>,
) {
    let is_dirty = !changed.is_empty()
        || !removed.is_empty()
        || !unbound.is_empty()
        || !rebound.is_empty()
        || master.is_changed();
    // Event readers: draining is what marks this frame's removals as seen, so
    // it happens whether or not a rebuild follows.
    removed.clear();
    unbound.clear();
    if !is_dirty {
        return;
    }
    // `build_into`'s graph and `GraphReconcilePlugin`'s flag; `engine_ready`
    // guarantees neither, and a wire rebuild with no graph is a no-op. Taken
    // after the dirty gate so the removal drain above still happens.
    let (Some(mut graph), Some(mut dirty)) = (graph, dirty) else {
        return;
    };

    // What the ECS says the graph should be, built before anything is written.
    // From here on this value is the truth for edges and outputs; the engine is
    // what gets brought into line with it.
    let want = topology::build(
        &graph,
        &nodes,
        &sinks,
        &master,
        #[cfg(feature = "modulation")]
        &shaping,
    );

    // **The change detection, as one comparison.** The dirty gate above only
    // decides whether it is worth asking; this decides whether anything actually
    // moved. An edit that cancels out — a declaration rewritten to what it
    // already was — reaches here and stops, where a per-port diff would have
    // walked every port to discover the same thing.
    //
    // It cannot be fooled the way a revision counter can. A revision is
    // monotone but not a function of the graph, so it can order two states
    // and cannot identify one; `want == live` is structural equality, so two
    // graphs compare equal exactly when they are the same graph.
    //
    // # Why `rebound` is an exception and not a redundancy
    //
    // A value's [`NodeKey`] is an `Entity`, deliberately: that is what lets a
    // crossfade replace the unit behind a node without moving a wire. The
    // graph keys the same node by its `AudioNode`, which an entity binds, and
    // that binding is not the value's. (It survived the move off `Net`, doc
    // 013 PR 13: a node goes into the graph before any entity is bound to it,
    // and nodes with no entity at all are allowed, so the graph cannot key by
    // entity.) The consequence is
    // that the value **cannot see a re-bind** — `insert`ing a different
    // `AudioNode` on the same entity changes which `NodeId` the declaration
    // resolves to while leaving the entity, and therefore the key, alone. If the
    // replacement has the same shape (two mono oscillators do), the two values
    // are equal and every edge naming that entity would keep pointing at the
    // retired node — silently, since nothing renders it.
    //
    // The entity→`NodeId` mapping is engine state the value does not carry, so
    // it takes an engine-side signal to notice it moved. `Changed<AudioNode>` is
    // exactly that signal, and it is why this early return is skipped rather
    // than the value being taught to carry a `NodeId` — carrying one would
    // reintroduce the stale-id problem `PortSource::Node(Entity)` exists to
    // remove, and would make a crossfade look like a topology change.
    let rebound_this_frame = !rebound.is_empty();
    if !rebound_this_frame && live.as_ref().is_some_and(|live| *live.topology() == want) {
        return;
    }

    // A declaration wider than the root widens the root — it is not silently
    // truncated. Clamping instead would drop a 6-entry `MasterSources` down to a
    // stereo root's channels 0-1 with no warning and nothing in the ECS to
    // inspect, and the clamp would read as a bound rather than a policy.
    //
    // **Widen only, never narrow.** A *shorter* declaration silences the
    // channels past its length (`topology::build`) and leaves the root at its
    // width: the root is sized to the device (`engine::build::root_width`),
    // and narrowing it under the device would make the engine fold channels
    // the host never asked to lose. Narrowing needs its own explicit API, not
    // an inference from a `Vec`'s length.
    //
    // Global output arity has none of the per-vertex hazard that makes node
    // arity a respawn: global outputs are sinks, so shrinking cannot dangle a
    // reference. The width is part of the spec the next commit compiles, and
    // the engine renders each plan at its own output count, so the RT side
    // needs nothing here.
    //
    // Driven by the declaration rather than by `want`, and that is deliberate:
    // `topology::build` clamps its outputs to the arity the root *has*, so a
    // value asked to widen the root would be reporting the outcome of a
    // widening that has not happened yet. The arity is a property of the
    // runtime the value is compiled into, not of the graph.
    if master.is_changed() {
        let declared = master.0.len().clamp(1, MAX_ROOT_CHANNELS);
        if master.0.len() > MAX_ROOT_CHANNELS {
            bevy_log::warn!(
                declared = master.0.len(),
                max = MAX_ROOT_CHANNELS,
                "MasterSources declares more channels than the render scratch \
                 holds; the excess will not be rendered"
            );
        }
        if declared > graph.outputs() {
            graph.widen_outputs(declared);
            dirty.0 = true;
        }
    }

    // The widening above may have changed the root's arity, which is one of the
    // two inputs `build` clamps against. Re-derive so the value that gets
    // applied — and stored as `live` — describes the graph that now exists,
    // rather than the one that did a moment ago.
    let want = if master.is_changed() {
        topology::build(
            &graph,
            &nodes,
            &sinks,
            &master,
            #[cfg(feature = "modulation")]
            &shaping,
        )
    } else {
        want
    };

    if topology::apply(&want, &mut graph, &nodes) {
        dirty.0 = true;
    }

    // `apply` is meant to have made these agree. `debug_assert` rather than a
    // log, so a divergence is a test failure and never a dropout — a release
    // build pays for the value and the writes, and none of the check.
    debug_assert_eq!(
        topology::disagreements(&want, &graph, &nodes),
        Vec::<String>::new(),
        "the engine does not match the value that was just applied to it"
    );

    // Last, and only after the writes landed: `live` is what the engine now
    // holds, so storing it before `apply` would record an intention rather than
    // an outcome — and a write that silently failed would then compare equal
    // next frame and never be retried.
    if let Some(mut live) = live {
        live.set(want);
    }
}

/// Silence the ports a removed declaration was claiming.
///
/// `On<Remove, PortSources>` fires at command-flush with the component value
/// still readable, mirroring
/// [`reconcile_node_despawn`](super::reconcile_node_despawn). Without this the
/// ports would keep their last-written sources forever: the entity leaves
/// [`rebuild`]'s query, so the diff never visits it again. This is the case that
/// is unsolvable imperatively without every call site remembering what it wired.
///
/// **Only the declared ports.** It reads the outgoing `PortSources` and clamps
/// to its length, exactly as [`rebuild`] does when writing. Zeroing every input
/// port instead would break the same contract the write path keeps — that a port
/// this layer never declared belongs to whoever did wire it, and is not ours to
/// silence on the way out.
pub fn unwire_removed_sources(
    remove: On<Remove, PortSources>,
    nodes: Query<&AudioNode>,
    declarations: Query<&PortSources>,
    graph: Option<ResMut<AudioGraphRes>>,
    // `Option` to match `graph`: this observer is registered by `GraphWirePlugin`
    // while `GraphDirty` is inserted by `GraphReconcilePlugin`, and both are
    // `pub`. An observer has no run condition to hide behind, so the only guard
    // is the signature.
    dirty: Option<ResMut<GraphDirty>>,
) {
    let entity = remove.event_target();
    let Ok(node) = nodes.get(entity) else { return };
    let Ok(declared) = declarations.get(entity) else {
        return;
    };
    let Some(mut graph) = graph else { return };
    let Some(mut dirty) = dirty else { return };
    if !graph.contains(*node) {
        return;
    }
    let claimed = declared.0.len().min(graph.node_inputs(*node));
    for port in 0..claimed {
        if graph.source(*node, port) != GraphSource::Silence {
            graph.set_source(*node, port, GraphSource::Silence);
            dirty.0 = true;
        }
    }
}

/// Declared graph wiring: the sink declarations, the master bus, and the
/// rebuild that compiles them.
///
/// [`rebuild`] runs after `Spawn` (a node must be in the graph before it can be
/// wired) and before `Compensate` (PDC is computed from topology, so wiring
/// after it would compensate last frame's graph). It sets [`GraphDirty`] and
/// never commits — `commit_graph` coalesces.
pub struct GraphWirePlugin;

impl Plugin for GraphWirePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<MasterSources>();
        app.init_resource::<LiveGraph>();
        app.add_observer(unwire_removed_sources);
        app.add_systems(
            Update,
            rebuild
                .after(GraphReconcileSystems::Spawn)
                .before(GraphReconcileSystems::Compensate)
                .run_if(engine_ready),
        );
    }
}
