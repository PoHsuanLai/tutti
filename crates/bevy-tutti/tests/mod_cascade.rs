//! One LFO modulating another LFO's rate, declared entirely in the ECS.
//!
//! A modulation source is not a graph node, so the ordinary resolver path —
//! `AudioNode` then downcast — could never serve a route onto a source's own
//! rate. These tests cover the second path: a source carries a live rate cell,
//! and the accumulator that drives it mirrors into that same cell.
//!
//! Sharing exactly one cell is the load-bearing part, and it is the part that
//! fails *silently* — a second cell type-checks, runs, and modulates nothing —
//! so the assertions below read the value the downstream source actually runs
//! at rather than any bookkeeping about it.

#![cfg(feature = "modulation")]

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;

use bevy_tutti::graph::{AudioGraphRes, GraphReconcilePlugin, TransportRes};
use bevy_tutti::modulation::{
    LfoShape, ModParamRange, ModRateCell, ModRoute, ModSource, ModSourceRate, ModTargetRegistry,
    ModulationMatrix, TuttiModulationPlugin,
};
use bevy_tutti::AudioEngineState;
use tutti_core::dsp::Net;
use tutti_core::transport::Transport;
use tutti_core::AudioNode;
use tutti_types::{Depth, Hz, ParamAddr, UnitParam};
use tutti_nodes::DistortionNode;

/// The rate the modulated LFO is authored at, and the floor of its range.
const CARRIER_RATE: f32 = 2.0;

fn app_with_graph() -> (App, Entity) {
    let mut app = App::new();

    let mut net = Net::new(0, 1);
    let node = net.push(Box::new(DistortionNode::new(
        tutti_nodes::ShapeKind::Tanh,
        1.0,
    )));
    net.pipe_output(node);

    app.insert_resource(AudioGraphRes(net));
    app.insert_resource(TransportRes(Transport::new(48_000.0)));
    app.insert_resource(AudioEngineState::Running);
    app.add_plugins((GraphReconcilePlugin, TuttiModulationPlugin));
    app.world_mut()
        .resource_mut::<ModTargetRegistry>()
        .register::<DistortionNode>();

    let target = app.world_mut().spawn(AudioNode(node)).id();
    (app, target)
}

fn advance_transport(app: &mut App, samples: i64) {
    let transport = app.world().resource::<TransportRes>().clone();
    let current = transport.settings.steady_time();
    transport
        .settings
        .steady_time
        .store(current + samples, std::sync::atomic::Ordering::Relaxed);
}

/// Spawn a source whose rate is driven by another source, and return both.
///
/// `carrier` is the LFO whose rate moves; `modulator` drives it. The carrier
/// declares `Rate` modulatable over `[2, 10]` Hz — the range is the *host's* to
/// declare here exactly as it is for a node's param.
fn spawn_cascade(app: &mut App) -> (Entity, Entity) {
    let carrier = app
        .world_mut()
        .spawn((
            ModSource::new(LfoShape::Sine),
            ModSourceRate::free_running(Hz(CARRIER_RATE)),
            ModParamRange::default().with(
                ParamAddr::Unit(UnitParam::Rate),
                CARRIER_RATE,
                CARRIER_RATE,
                10.0,
            ),
        ))
        .id();

    let modulator = app
        .world_mut()
        .spawn((
            ModSource::new(LfoShape::Sine),
            ModSourceRate::free_running(Hz(1.0)),
        ))
        .id();

    app.world_mut().spawn(
        ModRoute::new(modulator, carrier, ParamAddr::Unit(UnitParam::Rate)).with_depth(Depth::FULL),
    );

    (carrier, modulator)
}

/// The cell exists only where a route asks for one — it is not spawned onto
/// every source just in case.
#[test]
fn only_a_routed_source_gets_a_rate_cell() {
    let (mut app, _) = app_with_graph();
    let (carrier, modulator) = spawn_cascade(&mut app);
    app.update();

    assert!(
        app.world().get::<ModRateCell>(carrier).is_some(),
        "the routed-to source should have been given a rate cell"
    );
    assert!(
        app.world().get::<ModRateCell>(modulator).is_none(),
        "a source nothing routes at should not carry one"
    );
}

/// The route resolves against a source entity, which carries no `AudioNode` at
/// all — so this can only have gone through the rate path.
#[test]
fn a_route_onto_a_sources_rate_resolves() {
    let (mut app, _) = app_with_graph();
    let (carrier, _) = spawn_cascade(&mut app);
    app.update();

    assert!(
        app.world().get::<AudioNode>(carrier).is_none(),
        "a modulation source is not a graph node — the node path cannot serve it"
    );
    assert!(
        app.world()
            .resource::<ModulationMatrix>()
            .is_modulated(carrier, ParamAddr::Unit(UnitParam::Rate)),
        "the carrier's rate should be claimed by the matrix"
    );
}

/// The end-to-end claim: the driver actually moves the cell the carrier reads.
///
/// Asserting on the cell rather than on some downstream audible effect keeps
/// the failure legible — if this moves, the cascade is wired; if it does not,
/// the two halves are looking at different cells.
#[test]
fn the_modulator_moves_the_carriers_live_rate() {
    let (mut app, _) = app_with_graph();
    let (carrier, _) = spawn_cascade(&mut app);
    app.update();

    let seeded = app
        .world()
        .get::<ModRateCell>(carrier)
        .expect("carrier has a cell")
        .frequency();
    assert!(
        (seeded.get() - CARRIER_RATE).abs() < 1e-5,
        "the cell should start at the authored rate, got {seeded:?}"
    );

    // Sweep the modulating sine; its positive half must push the carrier's rate
    // above the base it was seeded with.
    let mut peak = seeded.get();
    for _ in 0..40 {
        advance_transport(&mut app, 480);
        app.update();
        let live = app
            .world()
            .get::<ModRateCell>(carrier)
            .expect("carrier keeps its cell")
            .frequency();
        peak = peak.max(live.get());
    }

    assert!(
        peak > CARRIER_RATE + 0.1,
        "the modulator should have driven the carrier's rate above {CARRIER_RATE}, peaked at {peak}"
    );
}

/// The cascade must reach the *downstream source's phase*, not merely the cell.
///
/// This is the assertion that catches the failure mode the whole design exists
/// to prevent: build the `Sourced` reading a cell other than the one the
/// accumulator writes, and every cell-level check above still passes — the
/// accumulator moves its cell correctly, it just isn't the cell anyone reads.
/// Only the carrier's own output can tell the difference.
///
/// So the carrier is pointed at a node's `Drive` and its waveform sampled. A
/// carrier whose rate is being driven sweeps at a different speed than one at a
/// fixed 2 Hz, so the two visit measurably different value sets.
#[test]
fn a_driven_rate_changes_the_carriers_own_output() {
    /// Sample the drive a `carrier`-driven node reads over a fixed window.
    ///
    /// `cascaded` decides whether the carrier's rate is itself modulated; both
    /// arms are otherwise identical, so any divergence is the cascade.
    fn drive_trace(cascaded: bool) -> Vec<f32> {
        let (mut app, target) = app_with_graph();

        let carrier = app
            .world_mut()
            .spawn((
                ModSource::new(LfoShape::Sine),
                ModSourceRate::free_running(Hz(CARRIER_RATE)),
                ModParamRange::default().with(
                    ParamAddr::Unit(UnitParam::Rate),
                    CARRIER_RATE,
                    CARRIER_RATE,
                    10.0,
                ),
            ))
            .id();

        // The carrier drives a node param, so its phase is observable.
        app.world_mut()
            .entity_mut(target)
            .insert(ModParamRange::default().with(
                ParamAddr::Unit(UnitParam::Drive),
                5.0,
                0.0,
                10.0,
            ));
        app.world_mut().spawn(
            ModRoute::new(carrier, target, ParamAddr::Unit(UnitParam::Drive))
                .with_depth(Depth(0.4)),
        );

        if cascaded {
            let modulator = app
                .world_mut()
                .spawn((
                    ModSource::new(LfoShape::Sine),
                    ModSourceRate::free_running(Hz(1.0)),
                ))
                .id();
            app.world_mut().spawn(
                ModRoute::new(modulator, carrier, ParamAddr::Unit(UnitParam::Rate))
                    .with_depth(Depth::FULL),
            );
        }

        let mut trace = Vec::new();
        for _ in 0..60 {
            advance_transport(&mut app, 480);
            app.update();
            let node = app.world().get::<AudioNode>(target).unwrap().0;
            trace.push(
                app.world()
                    .resource::<AudioGraphRes>()
                    .0
                    .node_as::<DistortionNode>(node)
                    .unwrap()
                    .drive()
                    .load(std::sync::atomic::Ordering::Acquire),
            );
        }
        trace
    }

    let plain = drive_trace(false);
    let cascaded = drive_trace(true);

    // Both must actually be moving, or "they differ" would be vacuous.
    let spread = |t: &[f32]| {
        t.iter().copied().fold(f32::MIN, f32::max) - t.iter().copied().fold(f32::MAX, f32::min)
    };
    assert!(
        spread(&plain) > 0.1 && spread(&cascaded) > 0.1,
        "both carriers should be sweeping: {} vs {}",
        spread(&plain),
        spread(&cascaded)
    );

    let diverged = plain
        .iter()
        .zip(&cascaded)
        .filter(|(a, b)| (*a - *b).abs() > 1e-3)
        .count();
    assert!(
        diverged > 5,
        "a driven rate must change when the carrier reaches each phase — \
         only {diverged}/60 samples differed, so the carrier is still running \
         at its fixed rate and is reading a cell nobody drives"
    );
}

/// A cascade must survive a rebuild. `collect` reconstructs every `Sourced` when
/// the declaration changes, so a cell minted during the build would be replaced
/// and the accumulator left writing an orphan — the exact bug the component
/// exists to prevent, and one that only shows up on the *second* build.
#[test]
fn the_cascade_survives_a_rebuild() {
    let (mut app, target) = app_with_graph();
    let (carrier, _) = spawn_cascade(&mut app);
    app.update();

    let before = app
        .world()
        .get::<ModRateCell>(carrier)
        .expect("carrier has a cell")
        .as_atomic();

    // Force a rebuild by declaring an unrelated route — enough to make the
    // whole source registry be rebuilt from scratch.
    app.world_mut()
        .entity_mut(target)
        .insert(ModParamRange::default().with(ParamAddr::Unit(UnitParam::Drive), 5.0, 0.0, 10.0));
    let other = app
        .world_mut()
        .spawn((
            ModSource::new(LfoShape::Triangle),
            ModSourceRate::free_running(Hz(3.0)),
        ))
        .id();
    app.world_mut().spawn(
        ModRoute::new(other, target, ParamAddr::Unit(UnitParam::Drive)).with_depth(Depth(0.2)),
    );
    app.update();

    let after = app
        .world()
        .get::<ModRateCell>(carrier)
        .expect("carrier keeps its cell")
        .as_atomic();
    assert!(
        std::sync::Arc::ptr_eq(&before, &after),
        "the rate cell must be the same allocation across a rebuild"
    );

    // And it must still be driven after that rebuild.
    let mut peak = 0.0_f32;
    for _ in 0..40 {
        advance_transport(&mut app, 480);
        app.update();
        peak = peak.max(
            app.world()
                .get::<ModRateCell>(carrier)
                .expect("carrier keeps its cell")
                .frequency()
                .get(),
        );
    }
    assert!(
        peak > CARRIER_RATE + 0.1,
        "the cascade should still drive the rate after a rebuild, peaked at {peak}"
    );
}
