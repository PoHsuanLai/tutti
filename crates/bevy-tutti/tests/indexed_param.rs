//! Indexed param addresses in the Bevy adapter (doc 013, "Param
//! addresses"): `AudioParam<U, P>` with a packed `P` of index 1 writes the
//! second param of a kind on a node, and a `ModRoute` on
//! `ParamRef::unit_at` modulates it, each leaving the index-0 param alone.
//!
//! The node is a host's `ParamNode` with two `Threshold`s (index 0 and 1),
//! registered with `param_graph_node!`. The render figures for a route to
//! an indexed param are `tutti-session`'s `tests/indexed_params.rs`; this
//! suite pins the adapter's own parts: the component's address, its
//! reconciler, and the declaration's target.

#![cfg(feature = "modulation")]

use std::sync::Arc;
use std::sync::atomic::Ordering;

use bevy_app::prelude::*;

use bevy_tutti::AudioEngineState;
use bevy_tutti::graph::{
    AudioGraphRes, AudioParam, AudioParamAppExt, GraphReconcilePlugin, SpawnAudioNode, TransportRes,
};
use bevy_tutti::modulation::{
    Macro, ModRange, ModRoute, ModSource, ParamRef, TuttiModulationPlugin,
};
use tutti_graph::{
    Cx, IntoNode, Io, Node, NodeParts, ParamInput, ParamNode, ParamSet, Prepare, Shape, Status,
};
use tutti_transport::Transport;
use tutti_types::{AtomicF32, ChannelLayout};
use tutti_types::{Db, Depth, UnitAddr, UnitParam};

const BAND0: UnitAddr = UnitAddr::new(UnitParam::Threshold, 0);
const BAND1: UnitAddr = UnitAddr::new(UnitParam::Threshold, 1);

/// Band 0 spelled as the plain discriminant (the index-0 idiom), band 1 as
/// a packed address.
type Band0 = AudioParam<Db, { UnitParam::Threshold as u16 }>;
type Band1 = AudioParam<Db, { UnitAddr::new(UnitParam::Threshold, 1).to_u16() }>;

/// Two thresholds, each a cell, each a param port. Writes what its ports
/// read to its two outputs.
#[derive(Clone)]
struct Bands {
    knobs: [Arc<AtomicF32>; 2],
}

impl Bands {
    fn new(t0: f32, t1: f32) -> Self {
        Self {
            knobs: [Arc::new(AtomicF32::new(t0)), Arc::new(AtomicF32::new(t1))],
        }
    }
}

impl Node for Bands {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::STEREO).with_param_addrs(&[BAND0, BAND1])
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for k in 0..2 {
            match io.param(k) {
                ParamInput::Frames(v) => io.output(k).copy_from_slice(v),
                ParamInput::Base => {
                    let base = self.knobs[k].load(Ordering::Acquire);
                    io.output(k).fill(base);
                }
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f32> {
        self.knobs.get(port).map(|k| k.load(Ordering::Acquire))
    }
}

impl ParamNode for Bands {
    fn param_set(&self) -> ParamSet {
        ParamSet::builder()
            .param(BAND0, Arc::clone(&self.knobs[0]))
            .param(BAND1, Arc::clone(&self.knobs[1]))
            .build()
    }
    fn fork_fresh(&self) -> Self {
        Self::new(
            self.knobs[0].load(Ordering::Acquire),
            self.knobs[1].load(Ordering::Acquire),
        )
    }
}

impl IntoNode for Bands {
    type Controls = ParamSet;
    fn into_parts(self) -> NodeParts<ParamSet> {
        tutti_graph::param_parts(self)
    }
}

bevy_tutti::param_graph_node!(Bands);

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(bevy_app::TaskPoolPlugin::default());
    app.insert_resource(AudioGraphRes::headless(0, 1));
    app.insert_resource(TransportRes(Transport::new(48_000.0)));
    app.insert_resource(AudioEngineState::Running);
    app.add_plugins((GraphReconcilePlugin, TuttiModulationPlugin));
    app.add_audio_param::<Db, { UnitParam::Threshold as u16 }>();
    app.add_audio_param::<Db, { UnitAddr::new(UnitParam::Threshold, 1).to_u16() }>();
    app
}

fn cell(set: &ParamSet, a: UnitAddr) -> f32 {
    set.get(a).expect("the node has the param")
}

/// The packed `P` of an `AudioParam` decodes to the address it names, the
/// plain discriminant idiom is index 0, and a `P` that names no kind is none.
///
/// Mutation (run): `AudioParam::addr` decoding with `UnitParam::try_from(P)`
/// (the strict, index-free decode) → index 1's `P` is `None` → fails.
#[test]
fn a_packed_p_names_its_kind_and_index() {
    assert_eq!(Band0::new(Db(0.0)).addr(), Some(BAND0));
    assert_eq!(Band1::new(Db(0.0)).addr(), Some(BAND1));
    assert_eq!(Band1::new(Db(0.0)).kind(), Some(UnitParam::Threshold));
    assert_eq!(AudioParam::<Db, 0x00FF>::new(Db(0.0)).addr(), None);
    assert_eq!(AudioParam::<Db, 0x01FF>::new(Db(0.0)).kind(), None);
}

/// **`AudioParam` with an indexed `P` writes the cell of its own index**, and
/// the index-0 component the cell of index 0: both on one entity, each
/// write reaching its own cell and no other, and a change to one component
/// leaves the other's cell alone.
///
/// Mutations (run), each fails:
/// - `reconcile_audio_param` decoding `P` with `UnitParam::try_from(P)` and
///   skipping the component when that fails → the index-1 cell never moves;
/// - `reconcile_audio_param` writing `UnitAddr::first(param.param)` (the
///   index dropped) → the index-1 value lands on band 0 and band 1 stays;
/// - `add_audio_param`'s registry key ignoring `P`'s high byte
///   (`P & 0xFF`) → the index-1 reconciler is "already registered" and
///   never scheduled → fails.
#[test]
fn an_indexed_audio_param_writes_its_own_cell() {
    let mut app = app();
    let node = Bands::new(-10.0, -20.0);
    let entity = app
        .world_mut()
        .commands()
        .spawn_audio_node(node.clone())
        .insert((Band0::new(Db(-3.0)), Band1::new(Db(-6.0))))
        .id();
    app.world_mut().flush();
    app.update();

    let set = node.param_set();
    assert_eq!(cell(&set, BAND0), -3.0, "index 0 took its component");
    assert_eq!(cell(&set, BAND1), -6.0, "index 1 took its component");

    // Only band 1 changes: band 0's cell is not rewritten.
    set.set(BAND0, -99.0);
    app.world_mut()
        .entity_mut(entity)
        .get_mut::<Band1>()
        .unwrap()
        .value = Db(-12.0);
    app.update();
    assert_eq!(cell(&set, BAND1), -12.0);
    assert_eq!(cell(&set, BAND0), -99.0, "band 0 was not touched");
}

/// **A route to `ParamRef::unit_at(.., Threshold, 1)` resolves to index 1's
/// lane**: the lane's knob is the node's cell of index 1 (and follows a
/// write to it), the route has no lane on index 0, and a route on index 0
/// has the other cell.
///
/// Mutations (run), each fails:
/// - `ParamRef::unit_at` ignoring the index → the lane is on index 0 and
///   its knob is band 0's cell;
/// - `ModHostOver::target` (the session's) taking the knob from the kind
///   alone → band 1's lane reads band 0's cell.
#[test]
fn a_route_to_index_1_has_index_1s_lane_and_knob() {
    let mut app = app();
    let node = Bands::new(-10.0, -20.0);
    let entity = app
        .world_mut()
        .commands()
        .spawn_audio_node(node.clone())
        .id();
    app.world_mut().flush();

    let band1 = ParamRef::unit_at(entity, UnitParam::Threshold, 1);
    assert_eq!(band1, ParamRef::new(entity, BAND1));
    assert_eq!(
        ParamRef::unit_at(entity, UnitParam::Threshold, 0),
        ParamRef::unit(entity, UnitParam::Threshold)
    );
    let world = app.world_mut();
    world.spawn(ModRange::new(band1, -60.0, 0.0));
    let offset = world.spawn(ModSource::new(Macro(1.0))).id();
    world.spawn(ModRoute::new(offset, band1).with_depth(Depth(0.1)));
    for _ in 0..4 {
        app.update();
    }
    let graph = app.world().resource::<AudioGraphRes>();
    assert_eq!(graph.modulation().refusal(), None, "the declarations apply");

    let knob = |r: ParamRef| {
        graph
            .modulation()
            .lane(r)
            .map(|(_, lane)| lane.knob().expect("a native lane has a knob"))
    };
    assert_eq!(knob(band1), Some(-20.0), "index 1's own cell");
    assert_eq!(
        knob(ParamRef::unit(entity, UnitParam::Threshold)),
        None,
        "no lane on index 0"
    );
    let set = node.param_set();
    set.set(BAND1, -33.0);
    assert_eq!(knob(band1), Some(-33.0), "the knob is the node's cell");
    assert_eq!(cell(&set, BAND0), -10.0);
}

/// Registering an `AudioParam` whose `P` names no kind fails fast in a debug
/// build, instead of never reaching the graph.
///
/// Mutation (run): drop the `debug_assert!` from `add_audio_param` → no
/// panic → fails.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "not a UnitParam kind")]
fn registering_a_p_that_names_no_kind_panics() {
    let mut app = App::new();
    app.add_audio_param::<Db, 0x01FF>();
}
