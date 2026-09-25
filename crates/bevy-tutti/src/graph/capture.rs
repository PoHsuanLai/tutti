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
//! **That guard is `NodeId` equality and nothing more.** It catches a different
//! node bound to the entity; it cannot see a unit replaced *under the same id*.
//! `crossfade_audio_node` is such a replacement and re-captures, so it is safe;
//! a host that calls [`AudioGraphRes::replace`](crate::graph::AudioGraphRes::replace)
//! directly bypasses both the capture and the guard, and the entity keeps
//! driving the outgoing unit's controls. Go through `crossfade_audio_node`, or
//! re-run [`CapturedControls::capture`] and [`bind`](CapturedControls::bind)
//! yourself.
//!
//! When the node goes (its `AudioNode` is removed, or the entity despawned) the
//! captured components go with it — `drop_captured` runs from the removal
//! observer. They are not free to keep: a `ModParamsHandle` holds a whole clone
//! of the unit, which for a convolver or a synth is its IR or its voices.
//!
//! Every node-insertion path in this crate runs the capture:
//! [`spawn_audio_node`](crate::graph::SpawnAudioNode),
//! [`insert_audio_node`](crate::graph::InsertAudioNode),
//! [`crossfade_audio_node`](crate::graph::crossfade_audio_node) (which replaces
//! the unit, so re-captures), the soundfont promotion and the plugin load. A
//! host that pushes a unit into the graph itself and binds `AudioNode` by hand
//! does the same with [`CapturedControls::capture`],
//! [`AudioGraphRes::insert_with`](crate::graph::AudioGraphRes::insert_with) and
//! [`bind`](CapturedControls::bind). `insert_with` is what lets a native
//! export's fork of a MIDI unit play its clip; a unit with a captured port
//! pushed with the plain `insert` refuses such an export by name.

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
    /// How a fork of the unit carries the clip on `midi`, for
    /// [`AudioGraphRes::insert_with`](crate::graph::AudioGraphRes::insert_with)
    /// to hand the native graph; taken there.
    #[cfg(feature = "midi")]
    // Behind a `Mutex` only to be `Sync`: controls wait in `PendingCrossfades`,
    // a resource, and a fork source is `Send` alone. Never contended.
    fork: Option<std::sync::Mutex<crate::midi::MidiFork>>,
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
        #[cfg(feature = "midi")]
        let (midi, fork) = midi.and_then(|r| r.capture_forking(unit)).unzip();
        Self {
            #[cfg(feature = "midi")]
            midi,
            #[cfg(feature = "midi")]
            fork: fork.map(std::sync::Mutex::new),
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

    /// How a fork of the unit carries its MIDI clip, taken out for the graph
    /// the unit goes into. `None` for a unit with no captured port, and after
    /// the first take.
    #[cfg(feature = "midi")]
    pub(crate) fn take_fork(&mut self) -> Option<crate::midi::MidiFork> {
        self.fork.take().map(|m| {
            m.into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        })
    }

    /// Put back a fork [`take_fork`](Self::take_fork) took, for a unit the
    /// graph handed back (a refused replace).
    #[cfg(feature = "midi")]
    pub(crate) fn put_fork(&mut self, fork: crate::midi::MidiFork) {
        self.fork = Some(std::sync::Mutex::new(fork));
    }

    /// Bind `entity` to `node`: insert [`AudioNode`] and every captured control.
    ///
    /// The whole binding in one step, which is how every insertion path in this
    /// crate forms it.
    pub fn bind(self, entity: &mut EntityWorldMut, node: AudioNode) {
        entity.insert(node);
        self.replace(entity, node);
    }

    /// Replace the entity's captured controls with these, keeping its
    /// [`AudioNode`] — the crossfade case, where the unit changes under a
    /// surviving handle.
    ///
    /// A control this unit does not have is **removed**: the old unit's port or
    /// params would otherwise stay reachable under the new unit's node id.
    ///
    /// The plugin binding latches are cleared too. `PluginTransportBound` and
    /// `PluginParamsBound` say "*this* plugin has its transport and automation
    /// installed", and the incoming `PluginClient` has neither: left in place
    /// they would stop the binding systems from ever installing them.
    /// `CompensatedLatency` records what PDC was last planned against for the
    /// outgoing node; clearing it makes the latency poll re-plan for the
    /// incoming one on its next pass.
    pub(crate) fn replace(self, entity: &mut EntityWorldMut, node: AudioNode) {
        let node: NodeId = node.0;
        let _ = (&entity, node);
        #[cfg(feature = "plugin")]
        {
            entity.remove::<(
                crate::plugin_host::PluginTransportBound,
                crate::plugin_host::CompensatedLatency,
            )>();
            #[cfg(feature = "modulation")]
            entity.remove::<crate::plugin_host::PluginParamsBound>();
        }
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

/// Remove every captured control from `entity`, for when its node is gone.
///
/// Called from the `On<Remove, AudioNode>` observer
/// ([`reconcile_node_despawn`](crate::graph::reconcile_node_despawn)), which
/// covers a despawn, a host taking `AudioNode` off, and the dead-plugin
/// teardown in `plugin_host::health`. `try_remove`, because on a despawn the
/// entity is gone by the time the command applies, and that is fine.
pub(crate) fn drop_captured(commands: &mut Commands, entity: Entity) {
    let Ok(mut e) = commands.get_entity(entity) else {
        return;
    };
    let _ = &mut e;
    #[cfg(feature = "midi")]
    e.try_remove::<crate::midi::MidiTarget>();
    #[cfg(feature = "modulation")]
    e.try_remove::<crate::modulation::ModParamsHandle>();
    #[cfg(feature = "plugin")]
    e.try_remove::<crate::plugin_host::PluginShadow>();
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
