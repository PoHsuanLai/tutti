//! Binding a loaded plugin to the rest of the engine: the meter, and
//! parameter automation / modulation.
//!
//! # What binding no longer has to do
//!
//! **The transport.** The plugin node reads it from each block's `Env` — the
//! engine's own playhead, with every start, seek, tempo and loop edit on its
//! frame — so a freshly promoted plugin has the right transport from its
//! first block, and there is nothing to install (doc 013, Verdicts:
//! `TransportSource`). It used to receive a stopped default until a system
//! here installed a transport reader.
//!
//! **Reaching the node.** Binding into the graph is a typestate transition at
//! insert (`PluginClient::bind`, in `AudioGraphRes::insert_plugin`); the host
//! keeps the node's [`PluginControls`], captured before it went in, and never
//! needs the node again.
//!
//! **MIDI.** The plugin load captures the plugin's MIDI port as its
//! `MidiTarget` (`CapturedControls::for_plugin`), so the shared MIDI resolver
//! finds it like any other target.
//!
//! # Steady-state, not `Added`
//!
//! Every system here queries `(With<PluginEmitter>, Without<…Bound>)` rather
//! than `Added<PluginEmitter>`. A plugin can finish loading before the
//! metronome exists — on project load, plugins instantiate while the engine is
//! still coming up — and `Added` fires exactly once. A pass over the not-yet-
//! bound converges whenever the missing half turns up; a one-shot leaves that
//! plugin at 4/4 forever. This is the rule `midi/registration.rs` states at
//! length.
//!
//! Binding is idempotent anyway (installing a meter replaces the previous
//! one), so the marker is an optimisation, not a correctness device.
//!
//! # Through the shadow, never the graph
//!
//! Every system here drives the plugin through its [`PluginShadow`] — the
//! node's [`PluginControls`], captured when the plugin was loaded. Those are
//! the node's input slots, meter, latency and tail cells and sample rate, each
//! shared with the node, so an install through the shadow is seen live by the
//! node the audio thread runs, lock-free and with no commit.

use bevy_ecs::prelude::*;
#[cfg(feature = "modulation")]
use bevy_log::warn;

use tutti_core::dsp::NodeId;
use tutti_core::AudioNode;
use tutti_plugin::handles::PluginControls;

use crate::graph::MetronomeRes;
#[cfg(feature = "modulation")]
use crate::graph::TransportRes;
use crate::plugin_host::editor::PluginEmitter;

/// A loaded plugin's [`PluginControls`], captured from its node before the node
/// went into the graph.
///
/// Inserted by the plugin load
/// ([`CapturedControls::for_plugin`](crate::graph::CapturedControls::for_plugin)).
/// Meter and param binding install through it, and the latency poll reads
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

/// "This plugin has the project meter installed."
///
/// Carries nothing: it is a latch, and the meter it stands for is reachable
/// from `MetronomeRes`. Storing a copy of anything here would be a second
/// source of truth for state the metronome already owns.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PluginMeterBound;

/// Query filter for the steady-state meter binding: a loaded plugin that has
/// not been bound yet.
type MeterUnbound = (With<PluginEmitter>, Without<PluginMeterBound>);

/// Give every plugin that lacks it the project meter.
///
/// The plugin's per-block transport — tempo, playhead, loop, recording — comes
/// from the graph's `Env` and needs nothing installed; the time signature and
/// bar come from the meter, which is a layer over the timeline rather than
/// transport state. Installed as the very cell the metronome publishes into,
/// shared not snapshotted, so a later meter edit reaches a running plugin with
/// nothing reinstalled. The send is gated inside the engine on
/// `Features::TRANSPORT`, so a plugin that never asked for transport simply
/// never reads it.
///
/// # The resource is optional, deliberately
///
/// `MetronomeRes` comes from `engine::build_into`, and `engine_ready` only
/// reads `AudioEngineState` — a value a host can insert on its own, as this
/// crate's own tests do. Waiting is the right answer: the steady-state query
/// binds a plugin as soon as the metronome turns up, where a hard `Res` turns
/// "not yet" into a panicked schedule. Until then the plugin reads 4/4 from
/// bar 0.
pub fn plugin_bind_meter(
    mut commands: Commands,
    metronome: Option<Res<MetronomeRes>>,
    unbound: Query<(Entity, &AudioNode, &PluginShadow), MeterUnbound>,
) {
    if unbound.is_empty() {
        return;
    }
    let Some(metronome) = metronome else {
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
        controls.set_meter(meter.clone());
        commands.entity(entity).insert(PluginMeterBound);
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
/// answering on `ParamAddr::Id` (it no longer compiles only because a plugin is
/// a native node now, not an `AudioUnit`, and `register` asks for the latter).
/// It would be wrong anyway.
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
