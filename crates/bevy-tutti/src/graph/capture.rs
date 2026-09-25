//! Capturing a unit's controls on the way into the graph.
//!
//! Some of what a host does to a node is not audio routing and not a scalar
//! param: addressing MIDI to a synth, minting a modulation accumulator over a
//! filter's cutoff, installing a transport reader in a hosted plugin. Each needs
//! the *concrete* node, and the graph only holds `Box<dyn AudioUnit>`.
//!
//! These used to be answered by downcasting the graph's copy of the node on
//! every use. That ties every such call site to one graph implementation — a
//! graph that owns its nodes outright has no copy to hand out — and it read a
//! clone the audio thread never runs, which is sound only for state shared
//! across clones.
//!
//! So the question is asked **once, of the owned unit, before it is inserted**,
//! and the answer is kept on the entity:
//!
//! | Component | Captured from | Read by |
//! |---|---|---|
//! | `MidiTarget` (`midi`) | `MidiTargetRegistry` | MIDI registration, routing, sequencing |
//! | `ModParamsHandle` (`modulation`) | `ModTargetRegistry` | the modulation resolver |
//! | `PluginShadow` (`plugin`) | a `PluginClient` unit | plugin bind and latency poll |
//!
//! Every one holds only state the node shares with its clones (see each type),
//! so the component reaches the running node without the graph. Each also
//! records the [`NodeId`] it was captured for, and its reader ignores it when
//! that is not the entity's current [`AudioNode`] — a leftover from a node that
//! was replaced by hand is inert rather than stale.
//!
//! Every node-insertion path in this crate runs the capture:
//! [`spawn_audio_node`](crate::graph::SpawnAudioNode),
//! [`insert_audio_node`](crate::graph::InsertAudioNode),
//! [`crossfade_audio_node`](crate::graph::crossfade_audio_node) (which replaces
//! the unit, so re-captures), the soundfont promotion and the plugin load. A
//! host that pushes a unit into the graph itself and binds `AudioNode` by hand
//! does the same with [`CapturedControls::capture`] and
//! [`bind`](CapturedControls::bind).

use bevy_ecs::prelude::*;
use bevy_ecs::system::SystemParam;

use tutti_core::dsp::NodeId;
use tutti_core::{AudioNode, AudioUnit};

/// The controls captured from one unit, before it moved into the graph.
///
/// Empty for a unit no registry recognises, which is most of them (an
/// oscillator, a sum, a gain).
#[must_use = "captured controls do nothing until they are bound to the entity"]
#[derive(Default)]
pub struct CapturedControls {
    #[cfg(feature = "midi")]
    midi: Option<tutti_midi_runtime::MidiInPort>,
    #[cfg(feature = "modulation")]
    params: Option<std::sync::Arc<dyn tutti_mod::ModParams + Send + Sync>>,
    #[cfg(feature = "plugin")]
    plugin: Option<tutti_plugin::handles::PluginControls>,
}

impl CapturedControls {
    /// Run every capture against `unit`, reading the registries from `world`.
    ///
    /// A registry that is absent captures nothing. Call this **before** the unit
    /// moves into the graph — afterwards it is out of reach.
    pub fn capture(world: &World, unit: &dyn AudioUnit) -> Self {
        // `world` is unused only in the build with no registry-backed capture.
        let _ = world;
        Self::from_registries(
            #[cfg(feature = "midi")]
            world.get_resource::<crate::midi::MidiTargetRegistry>(),
            #[cfg(feature = "modulation")]
            world.get_resource::<crate::modulation::ModTargetRegistry>(),
            unit,
        )
    }

    fn from_registries(
        #[cfg(feature = "midi")] midi: Option<&crate::midi::MidiTargetRegistry>,
        #[cfg(feature = "modulation")] mods: Option<&crate::modulation::ModTargetRegistry>,
        unit: &dyn AudioUnit,
    ) -> Self {
        // `unit` is unused only in the build with no capturing feature at all.
        let _ = unit;
        Self {
            #[cfg(feature = "midi")]
            midi: midi.and_then(|r| r.capture(unit)),
            #[cfg(feature = "modulation")]
            params: mods.and_then(|r| r.capture(unit)),
            // Not a registry: `PluginClient` is this crate's own dependency, so
            // there is nothing for a host to register. An in-process plugin
            // node (VST2) is another type and captures nothing, as before —
            // binding never reached it.
            #[cfg(feature = "plugin")]
            plugin: unit
                .as_any()
                .downcast_ref::<tutti_plugin::handles::PluginClient>()
                .map(|client| client.controls()),
        }
    }

    /// Bind `entity` to `node`: insert [`AudioNode`] and every captured control.
    ///
    /// The whole binding in one step, which is how every insertion path in this
    /// crate forms it.
    pub fn bind(self, entity: &mut EntityWorldMut, node: NodeId) {
        entity.insert(AudioNode(node));
        self.replace(entity, node);
    }

    /// Replace the entity's captured controls with these, keeping its
    /// [`AudioNode`] — the crossfade case, where the unit changes under a
    /// surviving `NodeId`.
    ///
    /// A control this unit does not have is **removed**: the old unit's port or
    /// params would otherwise stay reachable under the new unit's node id.
    pub(crate) fn replace(self, entity: &mut EntityWorldMut, node: NodeId) {
        let _ = (&entity, node);
        #[cfg(feature = "midi")]
        match self.midi {
            Some(port) => {
                entity.insert(crate::midi::MidiTarget::new(node, port));
            }
            None => {
                entity.remove::<crate::midi::MidiTarget>();
            }
        }
        #[cfg(feature = "modulation")]
        match self.params {
            Some(params) => {
                entity.insert(crate::modulation::ModParamsHandle::new(node, params));
            }
            None => {
                entity.remove::<crate::modulation::ModParamsHandle>();
            }
        }
        #[cfg(feature = "plugin")]
        match self.plugin {
            Some(controls) => {
                entity.insert(crate::plugin_host::PluginShadow::new(node, controls));
            }
            None => {
                entity.remove::<crate::plugin_host::PluginShadow>();
            }
        }
    }
}

/// The registries a system needs to capture controls, for insertion paths that
/// run as systems rather than with the whole `World`.
///
/// Both registries are optional: a build or an app without the subsystem simply
/// captures nothing for it.
#[derive(SystemParam)]
pub struct ControlCapture<'w> {
    #[cfg(feature = "midi")]
    midi: Option<Res<'w, crate::midi::MidiTargetRegistry>>,
    #[cfg(feature = "modulation")]
    mods: Option<Res<'w, crate::modulation::ModTargetRegistry>>,
    _world: std::marker::PhantomData<&'w ()>,
}

impl ControlCapture<'_> {
    /// Run every capture against `unit`. See [`CapturedControls::capture`].
    pub fn capture(&self, unit: &dyn AudioUnit) -> CapturedControls {
        CapturedControls::from_registries(
            #[cfg(feature = "midi")]
            self.midi.as_deref(),
            #[cfg(feature = "modulation")]
            self.mods.as_deref(),
            unit,
        )
    }
}
