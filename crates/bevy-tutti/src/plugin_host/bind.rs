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
//! # Why `node_as_mut` is safe here
//!
//! `Net`'s frontend holds *clones* of the vertices, so a mutation through
//! `node_as_mut` normally reaches the audio thread only via the next commit —
//! and anything stored by value is discarded outright. The plugin input slots
//! are built for exactly this: each is an `Arc<ArcSwapOption<..>>` **shared
//! across clones**, so an install on any clone is seen live by whichever clone
//! is running, lock-free and with no commit. Installing a source is therefore
//! sound through this accessor; storing a plain value would not be.

use bevy_ecs::prelude::*;
#[cfg(feature = "modulation")]
use bevy_log::warn;

use tutti_plugin::handles::PluginClient;

use crate::graph::{AudioGraphRes, MetronomeRes, TransportRes};
use crate::plugin_host::editor::PluginEmitter;

/// "This plugin has the project transport installed."
///
/// Carries nothing: it is a latch, and the transport it stands for is reachable
/// from `TransportRes`. Storing a copy of anything here would be a second source
/// of truth for state the transport already owns.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PluginTransportBound;

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
pub fn plugin_bind_transport(
    mut commands: Commands,
    mut graph: ResMut<AudioGraphRes>,
    transport: Res<TransportRes>,
    metronome: Res<MetronomeRes>,
    unbound: Query<
        (Entity, &tutti_core::AudioNode),
        (With<PluginEmitter>, Without<PluginTransportBound>),
    >,
) {
    if unbound.is_empty() {
        return;
    }
    let meter = metronome.0.meter_cell();

    for (entity, node) in unbound.iter() {
        // `None` means the node is not (yet) a `PluginClient` in the graph —
        // skip and retry next frame, the same "skip and retry" every other
        // resolver in this crate documents.
        let Some(client) = graph.0.node_as_mut::<PluginClient>(node.0) else {
            continue;
        };
        client.set_transport_source(transport.0.clone(), meter.clone());
        commands.entity(entity).insert(PluginTransportBound);
    }
}

/// Register `PluginClient` with the MIDI and modulation registries.
///
/// Both registries resolve a target by downcasting the graph node to a concrete
/// type, so a type nobody registered is unreachable — which is why a hosted
/// plugin could not receive MIDI regardless of how it was wired.
///
/// # MIDI needs nothing else
///
/// With the type registered, `register_midi_senders` picks a plugin up off its
/// `AudioNode` like any other unit, and `unregister_midi_sender` takes it off
/// the bus when that component goes. No plugin-specific system, and — more to
/// the point — no plugin-specific *path*: inventing one would be the second
/// lookup that `midi/target.rs` deleted the last time this crate had two.
///
/// # Modulation is registered here but resolved elsewhere
///
/// A plugin's parameters are **not** reachable through this registration, and
/// registering `PluginClient` for modulation would be actively wrong — see
/// [`plugin_bind_params`]. It is registered for MIDI only.
pub fn register_plugin_node_types(app: &mut bevy_app::App) {
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

/// Give every param a plugin declares modulatable a per-block accumulator, and
/// feed those accumulators to the plugin as its automation source.
///
/// A host declares which params are modulatable with [`ModParamRange`], the same
/// component a native node uses — the difference is only that a plugin's entries
/// carry [`ParamAddr::Id`] (its own numeric id) where a native node's carry
/// `ParamAddr::Unit`. Ranges come from the host because reading them from the
/// plugin means `PluginHandle::parameters()`, a blocking IPC call with a
/// five-second timeout that has no business on the frame thread.
///
/// # Why `insert_target` and not `ModTargetRegistry::register::<PluginClient>`
///
/// `register` looks like it would work — `PluginClient` implements `ModParams`,
/// answering on `ParamAddr::Id` — and it compiles. It is wrong.
///
/// `register` re-runs the downcast on **every** modulation rebuild, and
/// `PluginClient::param_target` is a *constructor*: it returns a fresh
/// accumulator each call and stores nothing. So each rebuild would mint a new
/// `Arc`, hand it to the router, and leave the plugin reading the previous one —
/// the param would sit silently at its base while the modulation appeared to be
/// connected. That is harmless for a native node, whose `mod_target` returns its
/// *existing* `AtomicTarget`, which is exactly why the trap is invisible.
///
/// So the target is built **once, here**, and supplied to the registry with
/// `insert_target` — checked before the node path, so no downcast ever runs for
/// these params. The same `Arc` goes to the plugin's automation source, which is
/// what makes accumulation visible to the per-block read.
///
/// `insert_target` is also the only route that accepts **curve** layers
/// (`AtomicTarget` declines them, since it collapses at a fixed beat), so a
/// plugin param modulated with `ModRoute::as_curve` traces a real ramp where a
/// native param would get a frame-rate staircase.
#[cfg(feature = "modulation")]
pub fn plugin_bind_params(
    mut commands: Commands,
    mut graph: ResMut<AudioGraphRes>,
    mut registry: ResMut<crate::modulation::ModTargetRegistry>,
    config: Res<crate::graph::AudioConfig>,
    transport: Res<TransportRes>,
    changed: Query<
        (
            Entity,
            &tutti_core::AudioNode,
            &crate::modulation::ModParamRange,
        ),
        (
            With<PluginEmitter>,
            Or<(
                Without<PluginParamsBound>,
                Changed<crate::modulation::ModParamRange>,
            )>,
        ),
    >,
) {
    for (entity, node, ranges) in changed.iter() {
        let Some(client) = graph.0.node_as_mut::<PluginClient>(node.0) else {
            continue; // not resolvable yet — retried next frame
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
                param_id,
                curve: target,
            });
        }

        if timed.is_empty() {
            client.clear_param_automation_source();
        } else {
            client.set_param_automation_source(std::sync::Arc::new(
                tutti_plugin::handles::ParamAutomationSource::new(
                    timed,
                    transport.transport_state(),
                    config.sample_rate,
                ),
            ));
        }

        commands.entity(entity).insert(PluginParamsBound);
    }
}
