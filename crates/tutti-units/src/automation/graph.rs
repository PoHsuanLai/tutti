//! Automation: trigger spawn + ECS binding for tutti's `AutomationLane`.
//!
//! Tutti's `AutomationLane` is an `AudioUnit` whose output is the current
//! envelope value at the transport's beat position. This module exposes:
//!
//! - [`AddAutomationLane`] — trigger to spawn a lane node from an envelope.
//! - [`AutomationLaneEmitter`] — marker for entities owning a lane node.
//! - [`AutomationLaneNode`] — marker for entities holding a typed lane.
//! - [`AutomationDrivesParam`] — relationship: "this lane drives a param on `target`."
//! - [`UpdateAutomationEnvelope`] — push an envelope change to an existing lane node.
//! - [`reconcile_automation_writes`] — runs in `GraphReconcileSystems::Params`
//!   and writes lane values into target entities' parameter components.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;

use crate::automation::LiveAutomationLane;
use tutti_core::graph::{AudioNode, Pan, PluginParam, Volume};

use tutti_core::graph::{reconcile_params, GraphReconcileSystems};
use tutti_core::graph::{TransportRes, AudioGraphRes};

/// Trigger component: spawn an entity with this to create an automation lane.
///
/// The `automation_lane_system` processes entities with `Added<AddAutomationLane>`,
/// creates an `AutomationLane` node, adds it to the graph, and replaces this
/// component with [`AutomationLaneEmitter`] + [`AudioNode`].
#[derive(Component, Debug, Clone)]
pub struct AddAutomationLane {
    pub envelope: crate::automation::AutomationEnvelope<f32>,
}

/// Marks an entity as having an automation lane in the graph.
///
/// Added automatically by `automation_lane_system`. The entity also
/// receives [`AudioNode`] with the same `NodeId` so it participates
/// in the standard graph-reconcile queries.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AutomationLaneEmitter {
    pub node_id: tutti_core::NodeId,
}

/// Marker component for entities holding a `LiveAutomationLane<f32>` node.
#[derive(Component, Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Reflect)]
#[reflect(Component, Default)]
pub struct AutomationLaneNode;

/// Selector for *which* parameter on the target entity the lane writes into.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Reflect)]
pub enum AutomationParam {
    Volume,
    Pan,
    /// Hosted plugin parameter by id.
    PluginParam(u32),
    /// Named effect parameter (e.g. "frequency", "wet"). Handled by
    /// the host's own reconcile system, not by [`reconcile_automation_writes`].
    EffectParam(String),
    /// Named synth parameter (e.g. "volume", "unison_detune"). Handled
    /// by the host's own reconcile system via direct graph node mutation.
    SynthParam(String),
}

/// "This automation lane drives a parameter on `target`."
///
/// Attach to the lane entity. The reconcile system reads the lane's current
/// output value and writes it into the target's selected parameter
/// component on the same frame.
#[derive(Component, Debug, Clone, PartialEq, Eq, Hash, Reflect)]
#[reflect(Component, Clone)]
pub struct AutomationDrivesParam {
    pub target: Entity,
    pub param: AutomationParam,
}

/// Attach to an entity that already has [`AutomationLaneEmitter`] to
/// push an updated envelope into the graph node. The system consumes
/// this component after applying the update.
#[derive(Component, Debug, Clone)]
pub struct UpdateAutomationEnvelope {
    pub envelope: crate::automation::AutomationEnvelope<f32>,
}

pub fn automation_lane_system(
    mut commands: Commands,
    mut graph: ResMut<AudioGraphRes>,
    transport: Res<TransportRes>,
    mut dirty: ResMut<tutti_core::graph::GraphDirty>,
    query: Query<(Entity, &AddAutomationLane), Added<AddAutomationLane>>,
) {
    let mut edited = false;

    for (entity, add) in query.iter() {
        let lane = crate::automation::AutomationLane::new(add.envelope.clone(), transport.0.clone());
        let node_id = graph.0.add(lane);
        edited = true;

        commands
            .entity(entity)
            .remove::<AddAutomationLane>()
            .insert((
                AutomationLaneEmitter { node_id },
                AudioNode(node_id),
            ));

        bevy_log::info!("Automation lane added (entity {entity:?}, node {node_id:?})");
    }

    // Stage only; the Commit-phase `commit_graph` coalesces (this system is
    // anchored before that phase).
    if edited {
        dirty.0 = true;
    }
}

/// Apply pending envelope updates to existing graph nodes.
pub fn update_automation_envelope_system(
    mut commands: Commands,
    mut graph: ResMut<AudioGraphRes>,
    mut dirty: ResMut<tutti_core::graph::GraphDirty>,
    query: Query<(Entity, &AutomationLaneEmitter, &UpdateAutomationEnvelope)>,
) {
    let mut edited = false;

    for (entity, emitter, update) in query.iter() {
        if let Some(lane) = graph.0.node_mut::<LiveAutomationLane<f32>>(emitter.node_id) {
            lane.set_envelope(update.envelope.clone());
            edited = true;
        }
        commands.entity(entity).remove::<UpdateAutomationEnvelope>();
    }

    // Stage only; the Commit-phase `commit_graph` coalesces (this system is
    // anchored before that phase).
    if edited {
        dirty.0 = true;
    }
}

/// Reads each automation lane's current value and writes it into the
/// target entity's parameter component.
///
/// Handles [`AutomationParam::Volume`], [`AutomationParam::Pan`], and
/// [`AutomationParam::PluginParam`]. The [`AutomationParam::EffectParam`]
/// variant is skipped here — the host provides its own reconciler for
/// typed effect-parameter components.
pub fn reconcile_automation_writes(
    graph: Res<AudioGraphRes>,
    drivers: Query<(&AudioNode, &AutomationDrivesParam)>,
    mut vol_pan_targets: Query<(Option<&mut Volume>, Option<&mut Pan>)>,
    mut plugin_targets: Query<&mut PluginParam>,
) {
    for (node, drives) in drivers.iter() {
        let Some(lane) = graph.0.node::<LiveAutomationLane<f32>>(node.0) else {
            continue;
        };
        let value = lane.last_value();

        match &drives.param {
            AutomationParam::Volume => {
                if let Ok((mut maybe_vol, _)) = vol_pan_targets.get_mut(drives.target) {
                    if let Some(v) = maybe_vol.as_deref_mut() {
                        if (v.0 - value).abs() > f32::EPSILON {
                            v.0 = value;
                        }
                    }
                }
            }
            AutomationParam::Pan => {
                if let Ok((_, mut maybe_pan)) = vol_pan_targets.get_mut(drives.target) {
                    if let Some(p) = maybe_pan.as_deref_mut() {
                        if (p.0 - value).abs() > f32::EPSILON {
                            p.0 = value;
                        }
                    }
                }
            }
            AutomationParam::PluginParam(id) => {
                if let Ok(mut p) = plugin_targets.get_mut(drives.target) {
                    if p.id == *id && (p.value - value).abs() > f32::EPSILON {
                        p.value = value;
                    }
                }
            }
            AutomationParam::EffectParam(_) | AutomationParam::SynthParam(_) => {
                // Handled by the host's own typed-param reconciler.
            }
        }
    }
}

/// Bevy plugin: automation lane spawn + parameter binding.
pub struct TuttiAutomationPlugin;

impl Plugin for TuttiAutomationPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<AutomationLaneNode>()
            .register_type::<AutomationDrivesParam>()
            .register_type::<AutomationParam>();
        // Both stage graph edits + set GraphDirty; anchor before the Commit
        // phase so `commit_graph` coalesces (they no longer commit inline).
        app.add_systems(
            Update,
            (automation_lane_system, update_automation_envelope_system)
                .before(GraphReconcileSystems::Commit)
                .run_if(tutti_core::graph::engine_ready),
        )
        .add_systems(
            Update,
            reconcile_automation_writes
                .in_set(GraphReconcileSystems::Params)
                .before(reconcile_params)
                .run_if(tutti_core::graph::engine_ready),
        );
    }
}
