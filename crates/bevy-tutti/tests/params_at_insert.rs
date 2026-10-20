//! Every insertion path addresses a node's params as the node goes in, so a
//! modulation lane's knob is the node's own cell.
//!
//! A lane finds a native param's knob through the node's `ParamSet`, which
//! the insertion keeps by key (`SessionNode::params`, `set_node_params`).
//! These pin it at the insertion paths themselves — `spawn_audio_node`,
//! `insert_audio_node`, `crossfade_audio_node` — rather than at a fixture
//! that addresses by hand, because a path that forgot to would leave every
//! hand-built suite green. The nodes are the suites' own
//! [`ProbeUnit`](common::probe::ProbeUnit) of `Drive`, a host's `ParamNode`
//! registered with `param_graph_node!`, and an engine node,
//! `DistortionNode`.
//!
//! Ported in doc 014 PR 5 from `capture_controls.rs`, which pinned the same
//! paths through the deleted `ModParamsHandle` (a capture the old driver
//! resolved routes on), on a `DriveUnit` that declared no param port (the
//! old driver wrote its cell; a lane needs the port, so the fixture went). Its `removing_the_node_drops_the_captured_handle`
//! and `a_handle_captured_for_another_node_resolves_to_nothing` went with
//! the handle: a lane resolves through the entity's current `AudioNode`
//! every apply (`mod_lanes.rs`'s
//! `an_entity_rebound_to_another_param_list_keeps_modulating`), and its
//! `a_crossfaded_synth_keeps_its_modulation` is
//! `a_crossfade_hands_the_lane_the_incoming_nodes_cell` below.
//!
//! Mutations (run, PR 5), each fails: `insert_and_bind` not addressing the
//! params (no `set_node_params`: the lane has no knob) → the three
//! insertion tests; `Swap::swap` not addressing the incoming node's params
//! → the crossfade test (the lane keeps the outgoing cell).

#![cfg(feature = "modulation")]

#[macro_use]
mod common;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;

use bevy_tutti::graph::{
    crossfade_audio_node, AudioGraphRes, GraphReconcilePlugin, InsertAudioNode, SpawnAudioNode,
    TransportRes,
};
use bevy_tutti::modulation::TuttiModulationPlugin;
use bevy_tutti::modulation::{Macro, ModRange, ModRoute, ModSource, ModulationState, ParamRef};
use bevy_tutti::AudioEngineState;
use tutti_core::transport::Transport;
use tutti_core::AtomicF32;
use tutti_nodes::{DistortionNode, ShapeKind};
use tutti_types::{Depth, UnitParam};

use common::probe::ProbeUnit;

const BASE: f32 = 5.0;

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(bevy_app::TaskPoolPlugin::default());
    app.insert_resource(AudioGraphRes::headless(0, 1));
    app.insert_resource(TransportRes(Transport::new(48_000.0)));
    app.insert_resource(AudioEngineState::Running);
    app.add_plugins((GraphReconcilePlugin, TuttiModulationPlugin));
    app
}

/// A macro on `entity`'s drive, `0..=10`, and frames until it applies.
fn modulate_drive(app: &mut App, entity: Entity) {
    let drive = ParamRef::unit(entity, UnitParam::Drive);
    let world = app.world_mut();
    world.spawn(ModRange::new(drive, 0.0, 10.0));
    let offset = world.spawn(ModSource::new(Macro(1.0))).id();
    world.spawn(ModRoute::new(offset, drive).with_depth(Depth(0.1)));
    settle(app);
}

fn settle(app: &mut App) {
    for _ in 0..4 {
        app.update();
    }
    let state = app.world().resource::<ModulationState>();
    assert_eq!(state.refusal(), None, "the declarations apply");
}

/// The knob of `entity`'s drive lane, `None` with no lane or no knob.
fn knob(app: &App, entity: Entity) -> Option<f32> {
    app.world()
        .resource::<ModulationState>()
        .lane(ParamRef::unit(entity, UnitParam::Drive))
        .and_then(|(_, lane)| lane.knob())
}

/// The lane's knob reads `cell`: what it holds, and a write into it.
fn assert_knob_is(app: &App, entity: Entity, cell: &Arc<AtomicF32>) {
    assert_eq!(knob(app, entity), Some(cell.load(Ordering::Acquire)));
    cell.store(7.5, Ordering::Release);
    assert_eq!(knob(app, entity), Some(7.5), "the knob is the node's cell");
}

#[test]
fn spawn_audio_node_addresses_a_hosts_param_node() {
    let mut app = app();
    let unit = ProbeUnit::of(UnitParam::Drive, BASE);
    let cell = unit.knob();
    let entity = app.world_mut().commands().spawn_audio_node(unit).id();
    app.world_mut().flush();
    modulate_drive(&mut app, entity);
    assert_knob_is(&app, entity, &cell);
}

#[test]
fn spawn_audio_node_addresses_an_engine_node() {
    let mut app = app();
    let node = DistortionNode::new(ShapeKind::Tanh, BASE);
    let cell = node.drive();
    let entity = app.world_mut().commands().spawn_audio_node(node).id();
    app.world_mut().flush();
    modulate_drive(&mut app, entity);
    assert_knob_is(&app, entity, &cell);
}

#[test]
fn insert_audio_node_addresses_the_params() {
    let mut app = app();
    let unit = ProbeUnit::of(UnitParam::Drive, BASE);
    let cell = unit.knob();
    let entity = app.world_mut().spawn_empty().id();
    app.world_mut()
        .commands()
        .entity(entity)
        .insert_audio_node(unit);
    app.world_mut().flush();
    modulate_drive(&mut app, entity);
    assert_knob_is(&app, entity, &cell);
}

/// After a crossfade the lane's knob is the **incoming** node's cell: a
/// write into it moves the knob, and one into the outgoing node's does not.
#[test]
fn a_crossfade_hands_the_lane_the_incoming_nodes_cell() {
    let mut app = app();
    let outgoing = ProbeUnit::of(UnitParam::Drive, BASE);
    let old = outgoing.knob();
    let entity = app.world_mut().commands().spawn_audio_node(outgoing).id();
    app.world_mut().flush();
    modulate_drive(&mut app, entity);
    assert_eq!(knob(&app, entity), Some(BASE), "precondition: the lane");

    let incoming = ProbeUnit::of(UnitParam::Drive, 3.0);
    let new = incoming.knob();
    crossfade_audio_node(&mut app.world_mut().commands(), entity, incoming);
    settle(&mut app);
    old.store(9.0, Ordering::Release);
    assert_eq!(knob(&app, entity), Some(3.0), "not the outgoing cell");
    assert_knob_is(&app, entity, &new);
}
