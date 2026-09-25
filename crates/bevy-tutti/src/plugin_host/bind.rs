//! Binding a loaded plugin to the rest of the engine: transport, MIDI, and
//! parameter automation / modulation.
//!
//! A freshly promoted plugin is an audio node and nothing more. Until it is
//! bound it receives a **default transport snapshot** — stopped, beat 0, no
//! tempo — for the life of the session, which is silently wrong rather than
//! obviously broken: a delay plugin's tempo sync sits at whatever it defaults
//! to, and nothing logs.
//!
//! # Steady-state, not `Added`
//!
//! Every system here queries `(With<PluginEmitter>, Without<…Bound>)` rather
//! than `Added<PluginEmitter>`. A plugin can finish loading before the transport
//! exists — on project load, plugins instantiate while the engine is still
//! coming up — and `Added` fires exactly once. A pass over the not-yet-bound
//! converges whenever the missing half turns up; a one-shot leaves that plugin
//! transport-less forever. This is the rule `midi/registration.rs` states at
//! length, and the bug it describes is the one this module exists to avoid.
//!
//! Binding is idempotent anyway (installing a source replaces the previous one),
//! so the marker is an optimisation, not a correctness device.
//!
//! # Through the shadow, never the graph
//!
//! Every system here drives the plugin through its [`PluginShadow`] — the
//! node's [`PluginControls`], captured when the node was loaded. Those are the
//! node's input slots, latency and tail cells and sample rate, each shared
//! across the node's clones, so an install through the shadow is seen live by
//! whichever clone the audio thread runs, lock-free and with no commit.
//!
//! This used to reach the graph's frontend clone of the node by downcast. That
//! was sound for the same reason (the slots are shared) but tied the binding to
//! a graph that keeps a frontend clone at all; the shadow does not care which
//! graph the node is in.

use bevy_ecs::prelude::*;
#[cfg(feature = "modulation")]
use bevy_log::warn;

use tutti_core::dsp::NodeId;
use tutti_core::AudioNode;
use tutti_plugin::handles::{PluginClient, PluginControls};

use crate::graph::{MetronomeRes, TransportRes};
use crate::plugin_host::editor::PluginEmitter;

/// A loaded plugin's [`PluginControls`], captured from its node before the node
/// went into the graph.
///
/// Inserted by the plugin load (and by any insertion path that meets a
/// [`PluginClient`] — see [`CapturedControls`](crate::graph::CapturedControls)).
/// Transport and param binding install through it, and the latency poll reads
/// it; none of them touch the graph.
///
/// It records the node it came from, and [`controls_for`](Self::controls_for)
/// answers only while that is still the entity's [`AudioNode`], so a shadow
/// left behind by a node replaced by hand drives nothing.
#[derive(Component, Debug, Clone)]
pub struct PluginShadow {
    node: NodeId,
    controls: PluginControls,
}

impl PluginShadow {
    /// The controls captured from the plugin node that became `node`.
    pub fn new(node: NodeId, controls: PluginControls) -> Self {
        Self { node, controls }
    }

    /// The graph node these controls were captured from.
    pub fn node(&self) -> NodeId {
        self.node
    }

    /// The controls, if this shadow belongs to `node` — the entity's current
    /// [`AudioNode`].
    pub fn controls_for(&self, node: &AudioNode) -> Option<&PluginControls> {
        (self.node == node.0).then_some(&self.controls)
    }
}

/// "This plugin has the project transport installed."
///
/// Carries nothing: it is a latch, and the transport it stands for is reachable
/// from `TransportRes`. Storing a copy of anything here would be a second source
/// of truth for state the transport already owns.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PluginTransportBound;

/// Query filter for the steady-state transport binding: a loaded plugin that has
/// not been bound yet.
type TransportUnbound = (With<PluginEmitter>, Without<PluginTransportBound>);

/// Install the project transport on every plugin that lacks it.
///
/// The plugin then receives a live per-block `TransportInfo` — tempo, playhead,
/// bar, time signature, loop range — instead of a stopped default. The send is
/// gated inside the engine on `Features::TRANSPORT`, so a plugin that never
/// asked for transport simply never reads what is installed.
///
/// Both handles are **shared, not snapshotted**. The transport is cloned (its
/// state is behind `Arc`s, so a clone shares rather than copies), and the meter
/// rides alongside as the very cell the metronome publishes into — which is what
/// lets a later tempo-map edit reach a plugin that is already running, with
/// nothing reinstalled. Meter is deliberately not read off the transport: it is
/// a layer over the timeline, not transport state.
///
/// # Every resource here is optional, deliberately
///
/// Both come from `engine::build_into`, and `engine_ready` only reads
/// `AudioEngineState` — a value a host can insert on its own, as this crate's
/// own tests and examples do. Waiting is the right answer anyway: the
/// steady-state query means a plugin binds as soon as the missing half turns up,
/// where a hard `Res` turns "not yet" into a panicked schedule.
pub fn plugin_bind_transport(
    mut commands: Commands,
    transport: Option<Res<TransportRes>>,
    metronome: Option<Res<MetronomeRes>>,
    unbound: Query<(Entity, &AudioNode, &PluginShadow), TransportUnbound>,
) {
    if unbound.is_empty() {
        return;
    }
    let (Some(transport), Some(metronome)) = (transport, metronome) else {
        return;
    };
    let meter = metronome.0.meter_cell();

    for (entity, node, shadow) in unbound.iter() {
        // `None` means the shadow was captured for a node this entity no longer
        // carries — skip and retry next frame, the same "skip and retry" every
        // other resolver in this crate documents.
        let Some(controls) = shadow.controls_for(node) else {
            continue;
        };
        controls.set_transport_source(transport.0.clone(), meter.clone());
        commands.entity(entity).insert(PluginTransportBound);
    }
}

/// Register `PluginClient` with the MIDI registry.
///
/// The registry captures a node's port from the concrete unit when the node is
/// inserted, so a type nobody registered is unreachable — which is why a hosted
/// plugin could not receive MIDI regardless of how it was wired.
///
/// # MIDI needs nothing else
///
/// With the type registered, the plugin load captures its `MidiTarget` like any
/// other insertion, `register_midi_senders` picks the plugin up off it, and
/// `unregister_midi_sender` takes it off
/// the bus when that component goes. No plugin-specific system, and — more to
/// the point — no plugin-specific *path*: inventing one would be a second
/// lookup beside the shared one, and two lookups for one question drift.
///
/// # Modulation is registered here but resolved elsewhere
///
/// A plugin's parameters are **not** reachable through this registration, and
/// registering `PluginClient` for modulation would be actively wrong — see
/// [`plugin_bind_params`]. It is registered for MIDI only.
///
/// # Ordering
///
/// `init_resource` rather than `resource_mut`, so this does not depend on
/// `TuttiMidiPlugin` having been added first. The registry is a plain
/// `Default` map that both plugins contribute entries to; requiring an order
/// between two independent `add_plugins` calls is the kind of constraint nobody
/// reads until it panics, and hosting is perfectly coherent without MIDI (an
/// effect plugin never receives a note).
pub fn register_plugin_node_types(app: &mut bevy_app::App) {
    app.init_resource::<crate::midi::MidiTargetRegistry>();
    app.world_mut()
        .resource_mut::<crate::midi::MidiTargetRegistry>()
        .register::<PluginClient>();
}

/// `PluginClient`'s MIDI inbox, so the shared registration path can find it.
impl crate::midi::MidiNode for PluginClient {
    fn midi_port(&self) -> &tutti_midi_runtime::MidiInPort {
        // Fully-qualified, like the synth impls: the inherent method and the
        // trait method share a name, and method syntax would resolve back into
        // this impl — an infinite recursion the compiler does catch, but only as
        // a warning.
        PluginClient::midi_port(self)
    }
}

/// "This plugin's declared params have accumulators registered."
///
/// Rebuilt whenever the [`ModParamRange`](crate::modulation::ModParamRange)
/// declaration changes, so the targets never describe an older param set.
#[cfg(feature = "modulation")]
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PluginParamsBound;

/// What param binding reads off each entity.
#[cfg(feature = "modulation")]
type ParamBindItem = (
    Entity,
    &'static AudioNode,
    &'static PluginShadow,
    &'static crate::modulation::ModParamRange,
);

/// Query filter for param binding: a loaded plugin that has either never been
/// bound, or whose declaration has changed since it was.
///
/// The `Changed` arm is what keeps the accumulators honest — a host that edits
/// `ModParamRange` gets targets rebuilt for the new param set rather than
/// keeping ones that describe the old.
#[cfg(feature = "modulation")]
type ParamsNeedRebind = (
    With<PluginEmitter>,
    Or<(
        Without<PluginParamsBound>,
        Changed<crate::modulation::ModParamRange>,
    )>,
);

/// Give every param a plugin declares modulatable a per-block accumulator, and
/// feed those accumulators to the plugin as its automation source.
///
/// A host declares which params are modulatable with [`ModParamRange`](crate::modulation::ModParamRange), the same
/// component a native node uses — the difference is only that a plugin's entries
/// carry [`ParamAddr::Id`](tutti_types::ParamAddr::Id) (its own numeric id) where a native node's carry
/// `ParamAddr::Unit`. Ranges come from the host because reading them from the
/// plugin means `PluginHandle::parameters()`, a blocking IPC call with a
/// five-second timeout that has no business on the frame thread.
///
/// # Why `insert_target` and not `ModTargetRegistry::register::<PluginClient>`
///
/// `register` looks like it would work — `PluginClient` implements `ModParams`,
/// answering on `ParamAddr::Id` — and it compiles. It is wrong.
///
/// `register`'s handle is asked again on **every** modulation rebuild, and
/// `PluginControls::param_target` is a *constructor*: it returns a fresh
/// accumulator each call and stores nothing. So each rebuild would mint a new
/// `Arc`, hand it to the router, and leave the plugin reading the previous one —
/// the param would sit silently at its base while the modulation appeared to be
/// connected.
///
/// A native node survives this **not** because it returns an existing
/// accumulator — `atomic_target` also constructs a fresh `AtomicTarget` every
/// call — but because the accumulator it builds *mirrors into the node's own
/// `AtomicF32`*, which the node keeps reading. The `Arc` is new; the cell it
/// writes through is the same one. That is a narrower guarantee than it looks,
/// and it has one real consequence: see
/// [`rebuild`](crate::modulation::rebuild)'s note on a rebuild re-seeding the
/// base from `ModParamRange`.
///
/// So the target is built **once, here**, and supplied to the registry with
/// `insert_target` — checked before the node path, so the node's own
/// `ModParams` is never asked for these params. The same `Arc` goes to the plugin's automation source, which is
/// what makes accumulation visible to the per-block read.
///
/// `insert_target` is also the only route that accepts **curve** layers
/// (`AtomicTarget` declines them, since it collapses at a fixed beat), so a
/// plugin param modulated with `ModRoute::as_curve` traces a real ramp where a
/// native param would get a frame-rate staircase.
///
/// # Every resource here is optional, deliberately
///
/// `engine_ready` reads `AudioEngineState`, which says nothing about whether
/// `build_into` ran (graph, config, transport) or whether the host added
/// `TuttiModulationPlugin` (the registry). `TuttiPlugin` notably does **not**
/// add the modulation plugin, so under `--features plugin,modulation` this
/// system is scheduled by the hosting plugin while reading a resource only a
/// different plugin inserts — a hard `Res` would panic on frame one.
#[cfg(feature = "modulation")]
pub fn plugin_bind_params(
    mut commands: Commands,
    registry: Option<ResMut<crate::modulation::ModTargetRegistry>>,
    transport: Option<Res<TransportRes>>,
    changed: Query<ParamBindItem, ParamsNeedRebind>,
) {
    if changed.is_empty() {
        return;
    }
    let (Some(mut registry), Some(transport)) = (registry, transport) else {
        return;
    };

    for (entity, node, shadow, ranges) in changed.iter() {
        let Some(client) = shadow.controls_for(node) else {
            continue; // captured for another node — retried next frame
        };

        let mut timed: Vec<tutti_plugin::handles::TimedParam> = Vec::new();
        for range in &ranges.params {
            // A plugin speaks numeric ids only. A `Unit` entry on a plugin is a
            // host mistake, not something to paper over: it would resolve to
            // `None` and silently do nothing.
            let tutti_types::ParamAddr::Id(param_id) = range.param else {
                warn!(
                    "plugin entity {entity:?} declares a native UnitParam \
                     ({:?}) — hosted plugins address params by id, ignoring",
                    range.param
                );
                continue;
            };

            // One `Arc`, two roles: the accumulator modulation writes into, and
            // the curve the plugin's per-block producer reads. Keeping them the
            // same object is the whole contract.
            let target = client.param_target(param_id, range.base, range.min, range.max);
            registry.insert_target(entity, range.param, target.clone());
            timed.push(tutti_plugin::handles::TimedParam {
                // `Opaque` is the right `ParamAddress` arm: `Index` is VST2-only
                // and dense, while `ParamAddr::Id` is the app's plugin-chosen
                // handle — exactly what `Opaque` models.
                param_id: tutti_plugin::handles::ParamAddress::Opaque(param_id.into()),
                curve: target,
            });
        }

        if timed.is_empty() {
            client.clear_param_automation_source();
        } else {
            // The rate is the node's own — `set_param_automation_source`
            // supplies it and re-stamps the source on a device change, so
            // passing `config.sample_rate` here could only agree or be wrong.
            client.set_param_automation_source(timed, (**transport).clone());
        }

        commands.entity(entity).insert(PluginParamsBound);
    }
}
