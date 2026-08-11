//! Declaring what feeds what.
//!
//! A node added to the graph is unwired and renders nothing. What reaches its
//! input ports is declared with [`AudioSources`] on the sink entity; what
//! reaches the speakers is declared with the [`MasterSources`] resource.
//!
//! ```rust
//! use bevy_app::prelude::*;
//! use bevy_ecs::prelude::*;
//! use bevy_tutti::prelude::*;
//! use tutti_core::dsp::{lowpass_hz, sine_hz, split, Net, Source, U2};
//!
//! fn build(mut commands: Commands) {
//!     let osc = commands.spawn_audio_node(sine_hz::<f32>(440.0)).id();
//!     // Stereo out, so `MasterSources::from` has two output ports to take.
//!     let filt = commands
//!         .spawn_audio_node(lowpass_hz(1000.0f32, 1.0) >> split::<U2>())
//!         .insert(AudioSources::from(osc))
//!         .id();
//!     commands.insert_resource(MasterSources::from(filt));
//! }
//!
//! let mut app = App::new();
//! app.insert_resource(AudioGraphRes(Net::with_backend(2)));
//! app.insert_resource(AudioEngineState::Running);
//! app.add_plugins(GraphReconcilePlugin);
//! app.add_systems(Startup, build);
//! app.update();
//!
//! // Read the edges back off the engine. This layer keeps no shadow state, so
//! // the engine is the only thing worth asserting on.
//! let graph = app.world().resource::<AudioGraphRes>();
//! assert!(matches!(graph.0.output_source(0), Source::Local(_, 0)));
//! assert!(matches!(graph.0.output_source(1), Source::Local(_, 1)));
//! ```
//!
//! # Why the sink owns the declaration
//!
//! A [`Net`](tutti_core::dsp::Net) graph is a *total function from input port to
//! source*: every port — `(node, channel)` and `(global, channel)` — holds
//! exactly one [`Source`], defaulting to `Zero`. There is no fan-in and no
//! partial state.
//!
//! So the declaration is keyed the way the engine is keyed. [`AudioSources`] is
//! a component (one per entity, enforced by the ECS) whose index *i* is input
//! port *i* (one slot, enforced by the type). **Two sources into one port cannot
//! be expressed.** An edge-entity model could express it, and would resolve it
//! by archetype iteration order — a silent, nondeterministic last-write-wins.
//! Summing is a node's job: `Net` has no summing bus and this layer must not
//! invent one.
//!
//! An edge-component model is also ruled out. Such a component needs a tracked
//! `HashMap<Entity, (NodeId, PortIndex)>` to know what to disconnect — adapter
//! shadow state mirroring the engine. This layer keeps none:
//! [`Net::source`](tutti_core::dsp::Net::source) and
//! [`output_source`](tutti_core::dsp::Net::output_source) read every port back,
//! so [`rebuild`] diffs against the engine and remembers nothing.
//!
//! # One writer per declared port
//!
//! A port named by an [`AudioSources`] belongs to that declaration. Writing it
//! imperatively through `AudioGraphRes.0` as well is a bug in the host, and one
//! this layer **cannot detect**: [`rebuild`]'s dirty gate watches ECS change
//! ticks, so an engine-side write nothing in the ECS touched does not re-enter
//! the loop. The imperative value simply stays until something unrelated
//! dirties the rebuild, at which point the declaration wins — silently, and at
//! an unpredictable moment.
//!
//! Declare the port, or own it — not both.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;

// `inputs`/`outputs` on `Net` are `AudioUnit` methods — the graph's own arity,
// as opposed to `inputs_in`/`outputs_in`, which are a contained node's.
use tutti_core::dsp::{AudioUnit as _, Source};
use tutti_core::node::AudioNode;
use tutti_core::{engine::MAX_ROOT_CHANNELS, ChannelLayout};

use super::{engine_ready, AudioGraphRes, GraphDirty, GraphReconcileSystems};

/// Where one input port's signal comes from.
///
/// [`tutti_core::dsp::Source`] with the node named by *entity* rather than
/// `NodeId`. That one difference is load-bearing: a
/// [`crossfade`](super::crossfade_audio_node) keeps a node's `NodeId` but an id
/// stored on an entity is stale the moment anything else replaces the node, so
/// the declaration names the entity and [`rebuild`] re-derives the id every
/// time. It also means a host never handles an engine id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioSource {
    /// Output `port` of the node bound to `entity`. Mirrors [`Source::Local`].
    Node {
        /// The entity carrying the source node. Resolved to a `NodeId` on every
        /// rebuild, so a crossfade cannot strand it.
        entity: Entity,
        /// Which of that node's **output** ports to take.
        port: usize,
    },
    /// Global network input `port` — a hardware or host input channel.
    /// Mirrors [`Source::Global`].
    Input {
        /// Index into the graph's global inputs. Out of range resolves to
        /// nothing rather than to silence.
        port: usize,
    },
    /// Silence. Mirrors [`Source::Zero`], and what an unlisted port gets.
    #[default]
    Silence,
}

impl AudioSource {
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
/// [`AudioSource::Silence`].
#[derive(Component, Debug, Clone, Default, PartialEq)]
pub struct AudioSources(pub Vec<AudioSource>);

impl AudioSources {
    /// Declares nothing. Add ports with [`with`](Self::with).
    pub fn silent() -> Self {
        Self(Vec::new())
    }

    /// Port 0 from `entity`'s port 0.
    pub fn from(entity: Entity) -> Self {
        Self(vec![AudioSource::node(entity)])
    }

    /// Ports 0 and 1 from `entity`'s ports 0 and 1.
    pub fn stereo_from(entity: Entity) -> Self {
        Self(vec![
            AudioSource::Node { entity, port: 0 },
            AudioSource::Node { entity, port: 1 },
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
                .map(|port| AudioSource::Node { entity, port })
                .collect(),
        )
    }

    /// Set one port, growing with [`AudioSource::Silence`] to reach it.
    ///
    /// # Audio-rate param ports belong here too
    ///
    /// A node's audio-rate *param* port (a filter's cutoff, a distortion's
    /// drive) is an ordinary input port that happens to sit after the audio
    /// inputs — get its index from the unit's
    /// [`ParamPorts::param_port`](tutti_units::ParamPorts) before boxing it into
    /// the graph, then name it here like any other port.
    ///
    /// Declaring it here is not a stylistic preference. Wiring a param port
    /// imperatively is *fragile*:
    /// [`Net::pipe_input`](tutti_core::dsp::Net::pipe_input) walks **every**
    /// input port of a node, so a later "wire the audio in" call silently
    /// overwrites a param edge with a global input — no error, no warning, the
    /// modulation just stops arriving.
    ///
    /// One `AudioSources` per entity (the ECS enforces that) and one index per
    /// port makes that clobber unrepresentable: [`rebuild`] writes the whole
    /// declared range from a single `Vec`. A sibling `ParamSources` component
    /// would put two writers back in one port space and let archetype iteration
    /// order pick the winner — the silent last-write-wins this module refuses
    /// for audio fan-in.
    pub fn with(mut self, port: usize, source: AudioSource) -> Self {
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
    pub fn set(&mut self, port: usize, source: AudioSource) {
        if self.0.len() <= port {
            self.0.resize(port + 1, AudioSource::Silence);
        }
        self.0[port] = source;
    }
}

/// What feeds each global output channel. Index = channel.
///
/// Empty — the default — declares nothing, so a host that never writes this
/// keeps whatever it wired through `AudioGraphRes.0` itself. Once written, it is
/// the single declaration of what reaches the speakers, which is what makes "two
/// nodes both own the master" unrepresentable rather than a race.
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct MasterSources(pub Vec<AudioSource>);

impl MasterSources {
    /// A **stereo** source on both output channels: port 0 to channel 0, port 1
    /// to channel 1.
    ///
    /// For a source with fewer outputs, say so explicitly — a mono node feeding
    /// both channels is [`mono_from`](Self::mono_from). There is no wrapping
    /// here on purpose. `Net::pipe_output` wraps with `channel % node_outputs`,
    /// which silently turns "route this" into "route this, duplicated", and the
    /// arity it wraps against is the *node's*, which this constructor cannot see
    /// — it has an `Entity`, not a graph. Guessing wrong leaves a channel
    /// unresolvable, which [`rebuild`] skips, which strands whatever the channel
    /// held before.
    pub fn from(entity: Entity) -> Self {
        Self(vec![
            AudioSource::Node { entity, port: 0 },
            AudioSource::Node { entity, port: 1 },
        ])
    }

    /// A **mono** source on both output channels, from its port 0.
    pub fn mono_from(entity: Entity) -> Self {
        Self(vec![
            AudioSource::Node { entity, port: 0 },
            AudioSource::Node { entity, port: 0 },
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
                .map(|port| AudioSource::Node { entity, port })
                .collect(),
        )
    }

    /// One channel from one of `entity`'s ports.
    pub fn with(mut self, channel: usize, source: AudioSource) -> Self {
        if self.0.len() <= channel {
            self.0.resize(channel + 1, AudioSource::Silence);
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
/// reason [`AudioSource::Node`] holds an `Entity` rather than a `NodeId`.
///
/// The gate is `Changed<AudioNode>`, not `Added`: a replacement `insert` on an
/// entity that already has the component fires `Changed` but **not** `Added`, so
/// an `Added` gate leaves a re-bound entity's wires pointing at the retired node
/// forever. `Added` is a subset of `Changed`, so this covers arrival too.
///
/// # Why a diff rather than a wholesale replace
///
/// The MIDI routing table and the modulation matrix both rebuild wholesale,
/// because their engines offer no incremental edit. `Net` offers *only*
/// incremental edits and already has `commit()` for atomicity, so writing every
/// port every rebuild would invalidate the topological order for ports that did
/// not change. Reading the engine back is what makes the diff possible without
/// this layer remembering anything.
#[allow(
    clippy::too_many_arguments,
    reason = "Bevy systems declare their data access as parameters; each one here \
              is a distinct query or resource the rebuild genuinely needs"
)]
pub fn rebuild(
    graph: Option<ResMut<AudioGraphRes>>,
    dirty: Option<ResMut<GraphDirty>>,
    nodes: Query<&AudioNode>,
    sinks: Query<(Entity, &AudioSources)>,
    master: Res<MasterSources>,
    changed: Query<(), Changed<AudioSources>>,
    rebound: Query<(), Changed<AudioNode>>,
    mut removed: RemovedComponents<AudioSources>,
) {
    let is_dirty =
        !changed.is_empty() || !removed.is_empty() || !rebound.is_empty() || master.is_changed();
    // An event reader: draining is what marks this frame's removals as seen, so
    // it happens whether or not a rebuild follows.
    removed.clear();
    if !is_dirty {
        return;
    }
    // `build_into`'s graph and `GraphReconcilePlugin`'s flag; `engine_ready`
    // guarantees neither, and a wire rebuild with no graph is a no-op. Taken
    // after the dirty gate so the removal drain above still happens.
    let (Some(mut graph), Some(mut dirty)) = (graph, dirty) else {
        return;
    };

    for (sink_entity, declared) in sinks.iter() {
        let Ok(sink) = nodes.get(sink_entity) else {
            continue; // No node yet — retry next frame.
        };
        if !graph.0.contains(sink.0) {
            continue;
        }
        // Only the ports the declaration names — see the master loop below for
        // why a short `Vec` means "undeclared" rather than "silent".
        let arity = declared.0.len().min(graph.0.inputs_in(sink.0));
        for port in 0..arity {
            let want = declared.0[port];
            let Some(want) = resolve(want, sink.0, &nodes, &graph) else {
                continue;
            };
            if graph.0.source(sink.0, port) != want {
                graph.0.set_source(sink.0, port, want);
                dirty.0 = true;
            }
        }
    }

    // A declaration wider than the root widens the root — it is not silently
    // truncated. Clamping instead would drop a 6-entry `MasterSources` down to a
    // stereo root's channels 0-1 with no warning and nothing in the ECS to
    // inspect, and the clamp would read as a bound rather than a policy.
    //
    // **Widen only, never narrow.** A *shorter* declaration means undeclared
    // (see the comment below), so narrowing on it would tear down channels the
    // host may own imperatively — the same violation `unwire_removed_sources`
    // refuses. Narrowing needs its own explicit API, not an inference from a
    // `Vec`'s length.
    //
    // Global output arity has none of the per-vertex hazard that makes node
    // arity a respawn: global outputs are sinks, so shrinking cannot dangle a
    // reference. `commit_graph` uses the arity-permitting commit, and
    // `Engine::process_segment` re-reads `backend.outputs()` after `pump()`
    // every block, so the RT side needs nothing here.
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
        if declared > graph.0.outputs() {
            graph.0.set_output_arity_live(declared);
            dirty.0 = true;
        }
    }

    // Only channels the resource actually names. An index past the end is
    // *undeclared*, not "declared silent": a host that has not written
    // `MasterSources` has said nothing about the bus, and a layer that answered
    // that silence by zeroing every channel would tear down whatever the host
    // wired itself. Declaring silence explicitly is `AudioSource::Silence`.
    //
    // The `.min` is redundant for a declaration the widening above satisfied,
    // but it is what makes an out-of-range channel unrepresentable when the
    // arity change is capped by `MAX_ROOT_CHANNELS` or does not happen at all.
    for channel in 0..master.0.len().min(graph.0.outputs()) {
        let want = master.0[channel];
        // `None` here means unresolvable, not silent — skip and retry.
        let Some(want) = resolve_master(want, &nodes, &graph) else {
            continue;
        };
        if graph.0.output_source(channel) != want {
            graph.0.set_output_source(channel, want);
            dirty.0 = true;
        }
    }
}

/// Turn a declaration into an engine [`Source`], or `None` if it cannot be
/// resolved *yet*.
///
/// `Silence` resolves to `Some(Source::Zero)` — deliberately distinct from
/// `None`. Collapsing the two would make "this entity has no node yet"
/// indistinguishable from "declared silent", and a port would be driven to zero
/// on the frame before its source appears, then never revisited.
fn resolve(
    source: AudioSource,
    sink: tutti_core::NodeId,
    nodes: &Query<&AudioNode>,
    graph: &AudioGraphRes,
) -> Option<Source> {
    match source {
        AudioSource::Silence => Some(Source::Zero),
        AudioSource::Input { port } => (port < graph.0.inputs()).then_some(Source::Global(port)),
        AudioSource::Node { entity, port } => {
            let node = nodes.get(entity).ok()?;
            if node.0 == sink {
                // `Net::set_source` asserts on this. A self-loop is a caller
                // mistake, not an engine failure — say so and skip.
                bevy_log::warn!(
                    "AudioSources on {entity:?} names itself as a source; skipping (a node \
                     cannot feed its own input)"
                );
                return None;
            }
            if !graph.0.contains(node.0) {
                return None;
            }
            warn_if_port_out_of_range(entity, node.0, port, graph)?;
            Some(Source::Local(node.0, port))
        }
    }
}

/// A port past the node's output count cannot resolve *ever*, unlike an entity
/// whose node has not spawned yet — so it warns rather than silently retrying
/// forever. Returns `None` in that case so the caller skips it.
fn warn_if_port_out_of_range(
    entity: Entity,
    node: tutti_core::NodeId,
    port: usize,
    graph: &AudioGraphRes,
) -> Option<()> {
    let outputs = graph.0.outputs_in(node);
    if outputs <= port {
        bevy_log::warn!(
            "declared source {entity:?} port {port}, but its node has only {outputs} output(s); \
             that port stays unwired. A mono node feeding both master channels is \
             `MasterSources::mono_from`."
        );
        return None;
    }
    Some(())
}

/// [`resolve`] for the global output bus, which has no sink node to compare
/// against — the master cannot feed itself.
fn resolve_master(
    source: AudioSource,
    nodes: &Query<&AudioNode>,
    graph: &AudioGraphRes,
) -> Option<Source> {
    match source {
        AudioSource::Silence => Some(Source::Zero),
        AudioSource::Input { port } => (port < graph.0.inputs()).then_some(Source::Global(port)),
        AudioSource::Node { entity, port } => {
            let node = nodes.get(entity).ok()?;
            if !graph.0.contains(node.0) {
                return None;
            }
            warn_if_port_out_of_range(entity, node.0, port, graph)?;
            Some(Source::Local(node.0, port))
        }
    }
}

/// Silence the ports a removed declaration was claiming.
///
/// `On<Remove, AudioSources>` fires at command-flush with the component value
/// still readable, mirroring
/// [`reconcile_node_despawn`](super::reconcile_node_despawn). Without this the
/// ports would keep their last-written sources forever: the entity leaves
/// [`rebuild`]'s query, so the diff never visits it again. This is the case that
/// is unsolvable imperatively without every call site remembering what it wired.
///
/// **Only the declared ports.** It reads the outgoing `AudioSources` and clamps
/// to its length, exactly as [`rebuild`] does when writing. Zeroing every input
/// port instead would break the same contract the write path keeps — that a port
/// this layer never declared belongs to whoever did wire it, and is not ours to
/// silence on the way out.
pub fn unwire_removed_sources(
    remove: On<Remove, AudioSources>,
    nodes: Query<&AudioNode>,
    declarations: Query<&AudioSources>,
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
    if !graph.0.contains(node.0) {
        return;
    }
    let claimed = declared.0.len().min(graph.0.inputs_in(node.0));
    for port in 0..claimed {
        if graph.0.source(node.0, port) != Source::Zero {
            graph.0.set_source(node.0, port, Source::Zero);
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
