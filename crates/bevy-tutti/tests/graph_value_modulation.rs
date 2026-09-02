//! The audio-rate modulation chain, asserted as a **value**.
//!
//! `mod_audio_rate.rs` asserts what the chain *builds* by reading the engine
//! back, and what it *computes* against the frame-rate path. This file asserts
//! the third thing neither can say without a value in hand: which shaper feeds
//! which sum port.
//!
//! That distinction is not academic. `ParamSumNode` is `base + Σ offsets` —
//! commutative, so a group whose shapers are *permuted* across the sum's ports
//! computes the identical result and every existing assertion holds. A group
//! whose shapers are *collapsed* onto one port does not, and it was invisible
//! too: the sum's arity is unchanged, the chain's `shapers` vector is unchanged,
//! and reading `Net::source` port by port only ever asks about one port at a
//! time. The whole edge set is what makes it visible, and that is a value.

#![cfg(feature = "modulation")]

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;

use bevy_tutti::graph::topology::{key_of, LiveGraph};
use bevy_tutti::graph::{AudioGraphRes, GraphReconcilePlugin};
use bevy_tutti::modulation::audio_rate::AudioRateChains;
use bevy_tutti::modulation::{
    ModParamRange, ModRoute, ModSource, ModSourceRate, ModTargetRegistry, TuttiModulationPlugin,
};
use bevy_tutti::AudioEngineState;
use tutti_core::dsp::Net;
use tutti_core::AudioNode;
use tutti_mod::LfoShape;
use tutti_nodes::{DistortionNode, ShapeKind};
use tutti_types::graph::{Edge, InPort, OutPort, Source, Topology};
use tutti_types::{Depth, Hz, ParamAddr, UnitParam};

/// An app with the engine's plugins and one ported distortion, ready to
/// modulate. Mirrors `mod_audio_rate.rs`'s fixture so the two files describe
/// the same graph.
fn app_with_target() -> (App, Entity) {
    let mut app = App::new();
    app.insert_resource(AudioGraphRes(Net::with_backend(2)));
    app.insert_resource(AudioEngineState::Running);
    app.add_plugins((GraphReconcilePlugin, TuttiModulationPlugin));
    app.world_mut()
        .resource_mut::<ModTargetRegistry>()
        .register::<DistortionNode>();

    let dist = DistortionNode::with_param_inputs(2, ShapeKind::Tanh, 5.0, true);
    // The port map is declared from the unit before it is boxed into the graph:
    // this is a direct `Net::add` site, so nothing else would record it and the
    // audio-rate route would fall back to per-frame.
    let ports = bevy_tutti::graph::ParamPortMap::of(&dist);
    let node = app.world_mut().resource_mut::<AudioGraphRes>().0.add(dist);
    let target = app
        .world_mut()
        .spawn((
            AudioNode(node),
            ports,
            ModParamRange::default().with(ParamAddr::Unit(UnitParam::Drive), 5.0, 0.0, 10.0),
        ))
        .id();
    (app, target)
}

fn spawn_lfo(app: &mut App) -> Entity {
    app.world_mut()
        .spawn((
            ModSource::new(LfoShape::Sine),
            ModSourceRate::free_running(Hz(2.0)),
        ))
        .id()
}

fn live(app: &App) -> Topology {
    app.world().resource::<LiveGraph>().topology().clone()
}

/// **Every shaper in a group occupies its own sum port.**
///
/// The hole this closes: collapsing all N shapers onto sum port 1 passes the
/// entire suite as it stood — verified by making exactly that edit and running
/// `--all-features`, 211/211 green. Nothing was asserting the *set* of edges
/// into the sum, only that the sum had the right arity and the chain the right
/// number of shapers, both of which a collapse leaves untouched.
///
/// The consequence of the collapse is silent and severe: two routes at depth
/// 0.25 each become one route at 0.25, because ports 2..N are left `Zero` and
/// only the last-written shaper reaches port 1. A user's second modulation
/// source simply stops arriving, with no error and a chain that inspects as
/// correct.
///
/// **Mutation note.** Changing `spawn_chain`'s
/// `sum_sources.with(i + 1, …)` to `.with(1, …)` fails this test on the
/// `distinct` assertion — that is the reviewer's mutation, and it is the one
/// that motivated the test. Permuting the shapers across their ports instead
/// (`.with(routes.len() - i, …)`) leaves this green on purpose: the sum is
/// commutative, so a permutation is not a defect and asserting a fixed
/// assignment would be over-specifying.
#[test]
fn every_shaper_in_a_group_gets_its_own_sum_port() {
    let (mut app, target) = app_with_target();
    let sources: Vec<Entity> = (0..3).map(|_| spawn_lfo(&mut app)).collect();

    for source in &sources {
        app.world_mut().spawn(
            ModRoute::new(*source, target, ParamAddr::Unit(UnitParam::Drive))
                .with_depth(Depth(0.25))
                .per_sample(),
        );
    }
    // Two updates: the first spawns source nodes and the chain, the second lets
    // the wire pass see the declarations they inserted.
    app.update();
    app.update();

    let chain = app
        .world()
        .resource::<AudioRateChains>()
        .get(target, ParamAddr::Unit(UnitParam::Drive))
        .expect("the group built a chain")
        .clone();
    assert_eq!(chain.shapers.len(), 3, "one shaper per route");

    let topology = live(&app);
    let sum = key_of(chain.sum);

    // Which sum port each shaper's output lands on, read off the value. This is
    // the question the per-port engine read cannot ask: it needs the whole edge
    // set at once, keyed by sink port.
    let mut ports: Vec<u16> = Vec::new();
    for shaper in &chain.shapers {
        let want = Source::Node(OutPort {
            node: key_of(*shaper),
            port: 0,
        });
        let port = topology
            .edges
            .iter()
            .find(|(at, edge)| at.node == sum && **edge == Edge::Direct(want))
            .map(|(at, _)| at.port)
            .unwrap_or_else(|| panic!("shaper {shaper:?} feeds no sum port at all"));
        ports.push(port);
    }

    let mut distinct = ports.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(
        distinct.len(),
        ports.len(),
        "each shaper occupies its own sum port; got {ports:?}. A collapse leaves \
         the sum's arity and the chain's shaper count untouched, so this is the \
         only assertion that sees it."
    );

    // And none of them is the base port, which is reserved.
    assert!(
        !ports.contains(&0),
        "port 0 is the base — an offset landing there would overwrite the \
         authored value, not add to it; got {ports:?}"
    );

    // The base itself is where it should be, which is what makes "port 0 is
    // reserved" a fact about the graph rather than a convention.
    assert_eq!(
        topology.edges.get(&InPort { node: sum, port: 0 }),
        Some(&Edge::Direct(Source::Node(OutPort {
            node: key_of(chain.base),
            port: 0
        }))),
        "the base feeds port 0"
    );
}

/// Despawning a route's **source** retires its half of the chain rather than
/// leaving the sum fed by a node that no longer exists.
///
/// The value is what makes the assertion possible without a device: "is any sum
/// port still fed by a despawned entity" is a question about the whole edge set,
/// and a stale edge is `Invalid::UnknownNode` — a fault the value reports and a
/// port-by-port engine read has no vocabulary for, since `Net::remove` leaves
/// `Zero` behind and `Zero` is a legal value.
///
/// **Mutation note.** Removing `RemovedComponents<AudioNode>` from `rebuild`'s
/// dirty gate fails this: the pass would not re-run after the despawn, and the
/// value would keep the retired shaper's edge. Making `build` skip its
/// `graph.0.contains` guard fails the `validate` assertion, since the value
/// would carry a node the engine dropped.
#[test]
fn despawning_a_mod_source_leaves_no_stale_edge_in_the_value() {
    let (mut app, target) = app_with_target();
    let (a, b) = (spawn_lfo(&mut app), spawn_lfo(&mut app));

    for source in [a, b] {
        app.world_mut().spawn(
            ModRoute::new(source, target, ParamAddr::Unit(UnitParam::Drive))
                .with_depth(Depth(0.25))
                .per_sample(),
        );
    }
    app.update();
    app.update();

    let before = live(&app);
    before
        .validate()
        .expect("the chain the reconciler built is structurally sound");
    let edges_before = before.edges.len();

    // Take one source's *node* out. Its shaper keeps its declaration, so the
    // value must drop the edge rather than carry one naming an absent key.
    let source_node = app
        .world()
        .get::<bevy_tutti::modulation::audio_rate::ModSourceNode>(a)
        .expect("the audio-rate tier gave the source a node")
        .0;
    app.world_mut().entity_mut(source_node).despawn();
    app.update();

    let after = live(&app);
    after
        .validate()
        .expect("no edge names a node the engine no longer holds");
    assert!(
        after.edges.len() < edges_before,
        "the retired source's edge left the value: {edges_before} -> {}",
        after.edges.len()
    );
    assert!(
        !after
            .nodes
            .contains_key(&tutti_types::graph::NodeKey(source_node.to_bits())),
        "and so did its node"
    );
}
