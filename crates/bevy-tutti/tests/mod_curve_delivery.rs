//! A route delivered as a beat-evaluated curve rather than a per-frame scalar.
//!
//! The sink here is defined *in the test*: a `LayeredCurve` that accepts curve
//! layers, standing in for the kind of accumulator a plugin's per-block
//! parameter producer holds. bevy-tutti ships no such sink — `AtomicTarget`
//! collapses at a fixed beat and declines curves — so this also exercises
//! `ModTargetRegistry::insert_target`, the only route to a sink no `AudioUnit`
//! owns.
//!
//! What the delivery mode buys is *when* the value is decided, not what it is:
//! a scalar is computed once per frame and stored, a curve is stored as a
//! function and sampled by the sink at whatever rate it reads.

#![cfg(feature = "modulation")]

use std::sync::{Arc, Mutex};

use bevy_app::prelude::*;

use bevy_tutti::graph::{AudioGraphRes, GraphReconcilePlugin, TransportRes};
use bevy_tutti::modulation::{
    LfoShape, ModParamRange, ModRate, ModRoute, ModSource, ModTargetRegistry, TuttiModulationPlugin,
};
use bevy_tutti::AudioEngineState;
use tutti_core::dsp::Net;
use tutti_core::transport::Transport;
use tutti_mod::{Curve, LayerKey, LayeredCurve, ModTarget};
use tutti_types::{Beat, BeatDuration, Hz, ParamAddr, UnitParam};

/// A sink that takes curve layers — the shape a sub-block reader has.
///
/// `AtomicTarget` mirrors a frame scalar into an atomic; this keeps the whole
/// `LayeredCurve` so it can be evaluated at any beat, which is exactly what a
/// plugin's per-block producer does with `PluginParamTarget`.
struct BeatSink {
    layered: Mutex<LayeredCurve<f32>>,
}

impl BeatSink {
    fn new(base: f32, min: f32, max: f32) -> Self {
        Self {
            layered: Mutex::new(LayeredCurve::new(base, min, max)),
        }
    }

    /// The summed value at `beat` — what a per-block reader would sample.
    fn value_at(&self, beat: Beat) -> f32 {
        self.layered.lock().unwrap().value_at(beat).unwrap_or(0.0)
    }
}

impl ModTarget for BeatSink {
    fn range(&self) -> (f32, f32) {
        self.layered.lock().unwrap().range()
    }
    fn base(&self) -> f32 {
        self.layered.lock().unwrap().base()
    }
    fn set_base(&self, value: f32) {
        self.layered.lock().unwrap().set_base(value);
    }
    fn accumulate(&self, key: LayerKey, offset: f32) {
        self.layered.lock().unwrap().set_scalar_layer(key, offset);
    }
    fn clear(&self, key: LayerKey) {
        self.layered.lock().unwrap().clear_layer(key);
    }
    fn final_value(&self) -> f32 {
        self.value_at(Beat(0.0))
    }
    /// The whole point of this sink: it holds curves, so it accepts them.
    fn accumulate_curve(&self, key: LayerKey, curve: Arc<dyn Curve>) -> bool {
        self.layered.lock().unwrap().set_layer(key, curve);
        true
    }
}

const BASE: f32 = 5.0;

fn app() -> App {
    let mut app = App::new();
    let mut net = Net::new(0, 1);
    let out = net.push(Box::new(tutti_units::DistortionNode::new(
        tutti_units::ShapeKind::Tanh,
        1.0,
    )));
    net.pipe_output(out);
    app.insert_resource(AudioGraphRes(net));
    app.insert_resource(TransportRes(Transport::new(48_000.0)));
    app.insert_resource(AudioEngineState::Running);
    app.add_plugins((GraphReconcilePlugin, TuttiModulationPlugin));
    app
}

/// Wire one source into one host-supplied sink. `as_curve` picks the delivery.
///
/// The param is `Drive` on an entity with no `AudioNode` at all — nothing but
/// the supplied sink could serve it, so a resolution here proves the supplied
/// path is what answered.
fn wire(app: &mut App, as_curve: bool) -> Arc<BeatSink> {
    let sink = Arc::new(BeatSink::new(BASE, 0.0, 10.0));
    let param = ParamAddr::Unit(UnitParam::Drive);

    let target = app
        .world_mut()
        .spawn(ModParamRange::default().with(param, BASE, 0.0, 10.0))
        .id();
    app.world_mut()
        .resource_mut::<ModTargetRegistry>()
        .insert_target(target, param, Arc::clone(&sink) as Arc<dyn ModTarget>);

    // Beat-synced: a curve is clocked by the beat, so only a beat-synced rate
    // has a curve form at all.
    let source = app
        .world_mut()
        .spawn((
            ModSource::new(LfoShape::Sine),
            ModRate::beat_synced(BeatDuration(1.0)),
        ))
        .id();

    let route = ModRoute::new(source, target, param);
    app.world_mut()
        .spawn(if as_curve { route.per_block() } else { route });

    app.update();
    sink
}

/// A sink no `AudioUnit` owns is reachable at all — the gap `insert_target`
/// closes. Without it, resolution needs an `AudioNode` and a registered node
/// type, so this entity could never have been modulated.
#[test]
fn a_host_supplied_sink_resolves_without_a_graph_node() {
    let mut app = app();
    let sink = wire(&mut app, false);
    app.update();

    assert!(
        !sink.layered.lock().unwrap().is_unlayered(),
        "the route should have installed a layer on the supplied sink"
    );
}

/// The payoff: a curve layer varies *between* frames, so a reader sampling
/// faster than the frame rate sees motion a scalar cannot give it.
#[test]
fn a_curve_delivered_route_varies_within_a_frame() {
    let mut app = app();
    let sink = wire(&mut app, true);
    app.update();

    // Sample across one beat without running a single extra frame — exactly
    // what a per-block producer does inside one callback.
    let traced: Vec<f32> = (0..8)
        .map(|i| sink.value_at(Beat(i as f64 / 8.0)))
        .collect();
    let moved = traced
        .iter()
        .filter(|v| (*v - traced[0]).abs() > 1e-4)
        .count();
    assert!(
        moved >= 6,
        "a curve layer must trace across the beat with no frames run: {traced:?}"
    );
}

/// The same route scalar-delivered: one value per frame, frozen between them.
///
/// This is the control for the test above — without it, "the value varied"
/// could just mean the driver ran.
#[test]
fn a_scalar_delivered_route_holds_between_frames() {
    let mut app = app();
    let sink = wire(&mut app, false);
    app.update();

    let traced: Vec<f32> = (0..8)
        .map(|i| sink.value_at(Beat(i as f64 / 8.0)))
        .collect();
    assert!(
        traced.iter().all(|v| (v - traced[0]).abs() < 1e-6),
        "a scalar layer is beat-independent within a frame: {traced:?}"
    );
}

/// Asking for a curve on a sink that only takes scalars must still modulate.
///
/// `AtomicTarget` — every native node's accumulator — declines curves, so the
/// request has to degrade rather than fail. A route that silently stopped
/// working when its sink said no would be far worse than a coarser one.
#[test]
fn a_curve_request_falls_back_when_the_sink_declines() {
    let mut app = app();
    let param = ParamAddr::Unit(UnitParam::Drive);

    // A real graph node, whose `ModParams` hands back an `AtomicTarget`.
    let node = {
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        graph.0.push(Box::new(tutti_units::DistortionNode::new(
            tutti_units::ShapeKind::Tanh,
            BASE,
        )))
    };
    app.world_mut()
        .resource_mut::<ModTargetRegistry>()
        .register::<tutti_units::DistortionNode>();

    let target = app
        .world_mut()
        .spawn((
            tutti_core::AudioNode(node),
            ModParamRange::default().with(param, BASE, 0.0, 10.0),
        ))
        .id();
    let source = app
        .world_mut()
        .spawn((
            ModSource::new(LfoShape::Sine),
            ModRate::beat_synced(BeatDuration(1.0)),
        ))
        .id();
    // Asks for a curve; the atomic sink will decline.
    app.world_mut()
        .spawn(ModRoute::new(source, target, param).per_block());

    let drive = |app: &App| {
        app.world()
            .resource::<AudioGraphRes>()
            .0
            .node_as::<tutti_units::DistortionNode>(node)
            .unwrap()
            .drive()
            .load(std::sync::atomic::Ordering::Acquire)
    };

    // A beat-synced source derives its phase from the transport *beat*, which
    // is its own atomic — advancing `steady_time` alone leaves it at zero and
    // the source frozen.
    let mut seen: Vec<f32> = Vec::new();
    for i in 0..16 {
        let transport = app.world().resource::<TransportRes>().clone();
        transport.settings.set_beat(Beat(i as f64 / 8.0));
        app.update();
        let v = drive(&app);
        if !seen.iter().any(|s| (s - v).abs() < 1e-3) {
            seen.push(v);
        }
    }

    assert!(
        seen.len() > 2,
        "a declined curve must fall back to scalar delivery and still modulate, saw {seen:?}"
    );
}
