//! Declaring what feeds what.
//!
//! A node added to the graph is unwired and renders nothing. What reaches its
//! input ports is declared with [`AudioSources`] on the sink entity; what
//! reaches the speakers is declared with the [`MasterSources`] resource.
//!
//! ```rust,ignore
//! let osc = commands.spawn_audio_node(sine_hz::<f32>(440.0)).id();
//! let filt = commands
//!     .spawn_audio_node(lowpass_hz(1000.0, 1.0))
//!     .insert(AudioSources::from(osc))
//!     .id();
//! commands.insert_resource(MasterSources::from(filt));
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
//! That is also why the old `AudioFeedsTo` edge component is not coming back. It
//! kept a tracked `HashMap<Entity, (NodeId, PortIndex)>` to know what to
//! disconnect — adapter shadow state mirroring the engine. This layer keeps
//! none: [`Net::source`](tutti_core::dsp::Net::source) and
//! [`output_source`](tutti_core::dsp::Net::output_source) read every port back,
//! so [`rebuild`] diffs against the engine and remembers nothing.
//!
//! # One writer per declared port
//!
//! A port named by an [`AudioSources`] belongs to that declaration. A host that
//! also writes it imperatively through `AudioGraphRes.0` will see its write
//! reverted by the next diff — the same situation
//! [`graph::param`](super::param) already documents for modulated params, where
//! a plain write "would be reverted within a frame and the fader would look
//! stuck". The diff detects it for free and warns once.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;

// `inputs`/`outputs` on `Net` are `AudioUnit` methods — the graph's own arity,
// as opposed to `inputs_in`/`outputs_in`, which are a contained node's.
use tutti_core::dsp::{AudioUnit as _, Source};
use tutti_core::node::AudioNode;

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
    Node { entity: Entity, port: usize },
    /// Global network input `port` — a hardware or host input channel.
    /// Mirrors [`Source::Global`].
    Input { port: usize },
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

    /// Set one port, growing with [`AudioSource::Silence`] to reach it.
    pub fn with(mut self, port: usize, source: AudioSource) -> Self {
        if self.0.len() <= port {
            self.0.resize(port + 1, AudioSource::Silence);
        }
        self.0[port] = source;
        self
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
    /// Every output channel from `entity`, wrapping if it has fewer outputs
    /// than the bus has channels. The declarative spelling of the old
    /// `Net::pipe_output`.
    pub fn from(entity: Entity) -> Self {
        Self(vec![
            AudioSource::Node { entity, port: 0 },
            AudioSource::Node { entity, port: 1 },
        ])
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
/// A declaration edit, a removal, a [`MasterSources`] edit — and a *new
/// [`AudioNode`]*, which is the non-obvious one. A declaration routinely names
/// an entity whose node arrives a frame later; nothing about the declaration
/// changes when it does, so without watching for that the wire would never form.
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
    mut graph: ResMut<AudioGraphRes>,
    mut dirty: ResMut<GraphDirty>,
    nodes: Query<&AudioNode>,
    sinks: Query<(Entity, &AudioSources)>,
    master: Res<MasterSources>,
    changed: Query<(), Changed<AudioSources>>,
    arrived: Query<(), Added<AudioNode>>,
    mut removed: RemovedComponents<AudioSources>,
) {
    let is_dirty = !changed.is_empty()
        || !removed.is_empty()
        || !arrived.is_empty()
        || master.is_changed();
    // An event reader: draining is what marks this frame's removals as seen, so
    // it happens whether or not a rebuild follows.
    removed.clear();
    if !is_dirty {
        return;
    }

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

    // Only channels the resource actually names. An index past the end is
    // *undeclared*, not "declared silent": a host that has not written
    // `MasterSources` has said nothing about the bus, and a layer that answered
    // that silence by zeroing every channel would tear down whatever the host
    // wired itself. Declaring silence explicitly is `AudioSource::Silence`.
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
        AudioSource::Input { port } => {
            (port < graph.0.inputs()).then_some(Source::Global(port))
        }
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
            if !graph.0.contains(node.0) || graph.0.outputs_in(node.0) <= port {
                return None;
            }
            Some(Source::Local(node.0, port))
        }
    }
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
        AudioSource::Input { port } => {
            (port < graph.0.inputs()).then_some(Source::Global(port))
        }
        AudioSource::Node { entity, port } => {
            let node = nodes.get(entity).ok()?;
            if !graph.0.contains(node.0) || graph.0.outputs_in(node.0) <= port {
                return None;
            }
            Some(Source::Local(node.0, port))
        }
    }
}

/// Silence every input port of a node whose declaration was removed.
///
/// `On<Remove, AudioSources>` fires at command-flush with the entity still
/// intact, mirroring [`reconcile_node_despawn`](super::reconcile_node_despawn).
/// Without this the ports would keep their last-written sources forever: the
/// entity leaves [`rebuild`]'s query, so the diff never visits it again.
///
/// This is the case that is unsolvable imperatively without every call site
/// remembering what it wired.
pub fn unwire_removed_sources(
    remove: On<Remove, AudioSources>,
    nodes: Query<&AudioNode>,
    graph: Option<ResMut<AudioGraphRes>>,
    mut dirty: ResMut<GraphDirty>,
) {
    let entity = remove.event_target();
    let Ok(node) = nodes.get(entity) else { return };
    let Some(mut graph) = graph else { return };
    if !graph.0.contains(node.0) {
        return;
    }
    for port in 0..graph.0.inputs_in(node.0) {
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
