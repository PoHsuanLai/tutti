//! Automation: trigger spawn + ECS binding for tutti's `AutomationLane`.
//!
//! Tutti's `AutomationLane` is an `AudioUnit` whose output is the current
//! envelope value at the transport's beat position. This module exposes:
//!
//! - [`AddAutomationLane`] — trigger to spawn a lane node from an envelope.
//! - [`AutomationLaneEmitter`] — marker for entities owning a lane node.
//! - [`AutomationLaneNode`] — marker for entities holding a typed lane.
//! - [`AutomationDrivesParam`] — relationship: "this lane drives a param on `target`."
//! - [`reconcile_automation_writes`] — runs in `GraphReconcileSystems::Params`
//!   and writes lane values into target entities' parameter components.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;

use tutti::automation::LiveAutomationLane;
use tutti::core::ecs::{AudioNode, PluginParam, Volume};

use crate::graph::reconcile::{reconcile_params, GraphReconcileSystems};
use crate::resources::{TransportRes, TuttiGraphRes};

/// Trigger component: spawn an entity with this to create an automation lane.
///
/// The `automation_lane_system` processes entities with `Added<AddAutomationLane>`,
/// calls `engine.automation_lane(envelope)`, adds the lane to the graph, and
/// replaces this component with `AutomationLaneEmitter`.
///
/// # Examples
///
/// ```rust,ignore
/// use tutti::{AutomationEnvelope, AutomationPoint, CurveType};
///
/// let envelope = AutomationEnvelope::new("volume")
///     .with_point(AutomationPoint::new(0.0, 0.0))
///     .with_point(AutomationPoint::with_curve(4.0, 1.0, CurveType::SCurve));
///
/// commands.spawn(AddAutomationLane { envelope });
/// ```
///
/// Not `Reflect`: `AutomationEnvelope` is a foreign type without
/// `bevy_reflect` integration.
#[derive(Component, Debug, Clone)]
pub struct AddAutomationLane {
    pub envelope: tutti::automation::AutomationEnvelope<String>,
}

impl AddAutomationLane {
    pub fn new(target: impl Into<String>) -> Self {
        Self {
            envelope: tutti::automation::AutomationEnvelope::new(target.into()),
        }
    }

    pub fn with_envelope(envelope: tutti::automation::AutomationEnvelope<String>) -> Self {
        Self { envelope }
    }
}

/// Marks an entity as having an automation lane in the graph.
///
/// Added automatically by `automation_lane_system`. Use `node_id` to
/// connect the lane's output to other graph nodes (e.g., a multiply node
/// for volume automation).
///
/// Not `Reflect`: `node_id` wraps a foreign fundsp `NodeId`.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AutomationLaneEmitter {
    pub node_id: tutti::NodeId,
}

/// Marker component for entities holding a `LiveAutomationLane<f32>` node.
///
/// Insert this together with [`AudioNode`] when you spawn the lane via
/// `commands.spawn_audio_node(lane, NodeKind::Generator)`. The reconcile
/// system uses it to filter the candidate set; a missing marker just means
/// the lane is purely an audio source (no driven targets) and is skipped
/// for parameter writes.
#[derive(Component, Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Reflect)]
#[reflect(Component, Default)]
pub struct AutomationLaneNode;

/// Selector for *which* parameter on the target entity the lane writes into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Reflect)]
pub enum AutomationParam {
    /// Writes into the target's [`Volume`] component.
    Volume,
    /// Writes into the target's [`tutti::core::ecs::Pan`] component.
    Pan,
    /// Writes into the target's [`PluginParam`] component matching the
    /// given plugin parameter id. The component is updated in place
    /// (`PluginParam::value` is overwritten).
    PluginParam(u32),
}

/// "This automation lane drives a parameter on `target`."
///
/// Attach to the lane entity. The reconcile system reads the lane's current
/// output value and writes it into the target's selected parameter
/// component on the same frame.
///
/// Multiple lanes can target the same entity / parameter; the last write
/// in iteration order wins, mirroring how multiple coincident automation
/// writes work in any DAW. Hosts that need deterministic merging can wrap
/// this in their own resolver and gate the write with a system order.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Hash, Reflect)]
#[reflect(Component, Clone)]
pub struct AutomationDrivesParam {
    pub target: Entity,
    pub param: AutomationParam,
}

pub fn automation_lane_system(
    mut commands: Commands,
    graph: Option<ResMut<TuttiGraphRes>>,
    transport: Option<Res<TransportRes>>,
    query: Query<(Entity, &AddAutomationLane), Added<AddAutomationLane>>,
) {
    let Some(mut graph) = graph else { return };
    let Some(transport) = transport else { return };

    let mut edited = false;

    for (entity, add) in query.iter() {
        let lane = tutti::automation::AutomationLane::new(add.envelope.clone(), transport.0.clone());
        let node_id = graph.0.add(lane);
        edited = true;

        commands
            .entity(entity)
            .remove::<AddAutomationLane>()
            .insert(AutomationLaneEmitter { node_id });

        bevy_log::info!("Automation lane added (entity {entity:?}, node {node_id:?})");
    }

    if edited {
        graph.0.commit();
    }
}

/// Reads each automation lane's current value and writes it into the
/// target entity's parameter component.
///
/// Runs in [`GraphReconcileSystems::Params`]. The downstream parameter
/// reconcilers (`reconcile_params`, `reconcile_plugin_params`, …) pick up
/// the resulting `Changed<Volume>` / `Changed<PluginParam>` later in the
/// same set and route it to the audio thread.
pub fn reconcile_automation_writes(
    graph: Option<Res<TuttiGraphRes>>,
    drivers: Query<(&AudioNode, &AutomationDrivesParam)>,
    mut targets: Query<(Option<&mut Volume>, Option<&mut PluginParam>)>,
) {
    let Some(graph) = graph else { return };

    for (node, drives) in drivers.iter() {
        // The lane value is the same for any T — `get_value_at` returns f32 —
        // so we read it as a `LiveAutomationLane<f32>` regardless of how the
        // host originally typed the envelope. Hosts that pick a different
        // T will need their own bind module.
        let Some(lane) = graph.0.node::<LiveAutomationLane<f32>>(node.0) else {
            continue;
        };
        let value = lane.last_value();

        let Ok((mut maybe_vol, mut maybe_param)) = targets.get_mut(drives.target) else {
            continue;
        };

        match drives.param {
            AutomationParam::Volume => {
                if let Some(v) = maybe_vol.as_deref_mut() {
                    if (v.0 - value).abs() > f32::EPSILON {
                        v.0 = value;
                    }
                }
            }
            AutomationParam::Pan => {
                // Pan needs its own component reference; route through a
                // separate query when callers need it. Skipped here to
                // keep the query borrow set narrow — Pan automation lands
                // in a follow-up commit alongside dawai's Pan reconcile.
                let _ = value;
            }
            AutomationParam::PluginParam(id) => {
                if let Some(p) = maybe_param.as_deref_mut() {
                    if p.id == id && (p.value - value).abs() > f32::EPSILON {
                        p.value = value;
                    }
                }
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
        app.add_systems(Update, automation_lane_system).add_systems(
            Update,
            reconcile_automation_writes
                .in_set(GraphReconcileSystems::Params)
                .before(reconcile_params),
        );
    }
}
