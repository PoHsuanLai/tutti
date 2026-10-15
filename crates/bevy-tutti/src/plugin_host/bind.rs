//! Binding a loaded plugin to the rest of the engine: the meter. (Its
//! parameters are modulated by control lanes on its event input,
//! `modulation::ParamRef::plugin`; nothing is bound for them.)
//!
//! # What binding does not have to do
//!
//! **The transport.** The plugin node reads it from each block's `Env` — the
//! engine's own playhead, with every start, seek, tempo and loop edit on its
//! frame — so a freshly promoted plugin has the right transport from its
//! first block, and there is nothing to install.
//!
//! **Reaching the node.** Binding into the graph is a typestate transition at
//! insert (`PluginClient::bind`, in `AudioGraphRes::insert_plugin`); the host
//! keeps the node's [`PluginControls`], captured before it went in, and never
//! needs the node again.
//!
//! **MIDI.** A plugin takes MIDI on its node's event input and sends it on
//! its event output: route rules, clips, keyboards and `EventSources` wire it
//! like any other node.
//!
//! # Steady-state, not `Added`
//!
//! Every system here queries `(With<PluginEmitter>, Without<…Bound>)` rather
//! than `Added<PluginEmitter>`. A plugin can finish loading before the
//! metronome exists — on project load, plugins instantiate while the engine is
//! still coming up — and `Added` fires exactly once. A pass over the not-yet-
//! bound converges whenever the missing half turns up; a one-shot leaves that
//! plugin at 4/4 forever.
//!
//! Binding is idempotent anyway (installing a meter replaces the previous
//! one), so the marker is an optimisation, not a correctness device.
//!
//! # Through the shadow, never the graph
//!
//! Every system here drives the plugin through its [`PluginShadow`] — the
//! node's [`PluginControls`] (an in-process VST2 node's [`PluginMeter`]),
//! captured when the plugin was loaded. Those are the node's meter, latency
//! and tail cells and sample rate, each shared with the node, so an install through the shadow is seen live by the
//! node the audio thread runs, lock-free and with no commit.

use bevy_ecs::prelude::*;

use tutti_core::{AudioNode, NodeKey};
use tutti_plugin::handles::{PluginControls, PluginMeter};

use crate::graph::MetronomeRes;
use crate::plugin_host::editor::PluginEmitter;

/// A loaded plugin's controls, captured from its node before the node went
/// into the graph: an out-of-process plugin's [`PluginControls`], or an
/// in-process VST2 plugin's one control, its [`PluginMeter`].
///
/// Inserted by the plugin load
/// ([`CapturedControls::for_plugin`](crate::graph::CapturedControls::for_plugin),
/// [`CapturedControls::for_plugin_meter`](crate::graph::CapturedControls::for_plugin_meter)).
/// The meter binding installs through it (either backend); the latency poll
/// reads the [`PluginControls`] (out-of-process only); neither touches the
/// graph.
///
/// It records the node it came from, and [`controls_for`](Self::controls_for)
/// and [`meter_for`](Self::meter_for) answer only while that is still the
/// entity's [`AudioNode`], so a shadow left behind by a node replaced by hand
/// drives nothing.
#[derive(Component, Debug, Clone)]
pub struct PluginShadow {
    node: NodeKey,
    controls: Option<PluginControls>,
    meter: PluginMeter,
}

impl PluginShadow {
    /// The controls captured from the out-of-process plugin node that became
    /// `node`.
    pub fn new(node: NodeKey, controls: PluginControls) -> Self {
        let meter = controls.meter();
        Self {
            node,
            controls: Some(controls),
            meter,
        }
    }

    /// The meter captured from the in-process VST2 plugin node that became
    /// `node`: the only control that node has.
    pub fn meter_only(node: NodeKey, meter: PluginMeter) -> Self {
        Self {
            node,
            controls: None,
            meter,
        }
    }

    /// The graph node these controls were captured from.
    pub fn node(&self) -> NodeKey {
        self.node
    }

    /// The controls, if this shadow belongs to `node` — the entity's current
    /// [`AudioNode`] — and is an out-of-process plugin's.
    pub fn controls_for(&self, node: &AudioNode) -> Option<&PluginControls> {
        self.controls.as_ref().filter(|_| self.node == node.0)
    }

    /// The node's meter cell, if this shadow belongs to `node`: either
    /// backend's.
    pub fn meter_for(&self, node: &AudioNode) -> Option<&PluginMeter> {
        (self.node == node.0).then_some(&self.meter)
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

/// Gives every plugin that lacks it the project meter.
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
        let Some(cell) = shadow.meter_for(node) else {
            continue;
        };
        cell.set_meter(meter.clone());
        commands.entity(entity).insert(PluginMeterBound);
    }
}
