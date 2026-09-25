//! The graph as a value: what [`LiveGraph`] holds, and what follows from the
//! value owning edges and outputs.
//!
//! `graph_wire.rs` asserts "the declaration reaches the engine" by reading the
//! engine, and that is still the right question for the declaration vocabulary.
//! This file asserts the things that only became *sayable* once a value stood
//! between the declaration and the runtime: that the graph can be compared as a
//! whole, folded for latency, validated, and — the two that are behaviour rather
//! than observation — that an imperative engine write is repaired from it, and
//! that a re-bind moves a wire the value itself cannot see.
//!
//! `wire::rebuild`'s `debug_assert` runs under every test in this crate, so an
//! engine that diverged from the value it was just handed would fail the whole
//! suite rather than only these.

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;

use bevy_tutti::graph::topology::{key_of, LiveGraph};
use bevy_tutti::graph::{
    AudioGraphRes, GraphDirty, GraphReconcilePlugin, GraphSource, MasterSources, PortSource,
    PortSources,
};
use bevy_tutti::AudioEngineState;
use tutti_core::AudioNode;
use tutti_core::{ChannelLayout, Db, Hz};
use tutti_nodes::testing::{Const, Osc};
use tutti_nodes::{ChannelSumNode, LimiterNode};
use tutti_types::graph::{Edge, InPort, OutPort, Source};

/// An app wired the way `build_into` leaves one, minus the audio device.
fn app() -> App {
    let mut app = App::new();
    app.insert_resource(AudioGraphRes::headless(0, 2));
    app.insert_resource(AudioEngineState::Running);
    app.add_plugins(GraphReconcilePlugin);
    app
}

/// Add a node to the graph and bind an entity to it.
fn spawn_node<U: tutti_core::AudioUnit + 'static>(app: &mut App, unit: U) -> Entity {
    let id = {
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        graph.insert(unit)
    };
    app.world_mut().spawn(id).id()
}

fn node_id(app: &App, entity: Entity) -> AudioNode {
    *app.world().get::<AudioNode>(entity).expect("AudioNode")
}

fn live(app: &App) -> &tutti_types::graph::Topology {
    app.world().resource::<LiveGraph>().topology()
}

/// The headline: a declaration becomes an edge in the value, keyed the way the
/// engine is keyed.
///
/// **Mutation note.** Changing `edge_of` to emit `Source::Zero` for a resolvable
/// node source fails this on the `Edge::Direct(Source::Node(..))` assertion;
/// dropping the sink-port index (writing port 0 for every port) fails it on the
/// `InPort` key, since the map would hold one edge where two are asserted.
#[test]
fn a_declaration_becomes_an_edge_in_the_value() {
    let mut app = app();
    let osc = spawn_node(&mut app, Osc::sine(Hz(440.0)));
    let sink = spawn_node(&mut app, ChannelSumNode::new(2, ChannelLayout::MONO));
    app.world_mut()
        .entity_mut(sink)
        .insert(PortSources::silent().with(0, PortSource::node(osc)));
    app.update();

    let topology = live(&app);
    assert_eq!(
        topology.edges.get(&InPort {
            node: key_of(sink),
            port: 0
        }),
        Some(&Edge::Direct(Source::Node(OutPort {
            node: key_of(osc),
            port: 0
        }))),
        "the value names the source entity's key and its output port"
    );
    assert!(
        topology.nodes.contains_key(&key_of(osc)) && topology.nodes.contains_key(&key_of(sink)),
        "both entity-bound nodes are in the value"
    );
}

/// `MasterSources` becomes the value's global outputs, in channel order.
///
/// **Mutation note.** Collecting the outputs in reverse, or dropping the
/// `min(root_channels)` clamp so an over-long declaration is carried whole,
/// both fail this.
#[test]
fn the_master_declaration_becomes_the_values_outputs() {
    let mut app = app();
    let osc = spawn_node(&mut app, Osc::sine(Hz(440.0)));
    app.world_mut()
        .insert_resource(MasterSources::mono_from(osc));
    app.update();

    assert_eq!(
        live(&app).outputs,
        vec![
            Source::Node(OutPort {
                node: key_of(osc),
                port: 0
            }),
            Source::Node(OutPort {
                node: key_of(osc),
                port: 0
            }),
        ],
        "both channels take port 0 — `mono_from` is a duplication, and the value says so"
    );
}

/// **The hazard `wire.rs`'s module docs used to call undetectable — now
/// repaired.**
///
/// A host writing a declared port imperatively through `AudioGraphRes::set_source` was
/// invisible to the old per-port loop: the dirty gate watches ECS change ticks,
/// so a write nothing in the ECS touched never re-entered the loop, and the
/// engine kept the imperative value "silently, and at an unpredictable moment".
///
/// With the value owning edges, the repair falls out of [`apply`]: it compares
/// every port the value names against the engine before writing, so a tampered
/// port is found and the declaration reasserted. That is a whole-graph sweep,
/// not a per-declaration one — the write can be on *any* declared port, and this
/// test tampers with a port belonging to a different sink from the one whose
/// edit provokes the rebuild.
///
/// # The residual, stated precisely
///
/// The repair needs a rebuild that gets past `rebuild`'s early return, and that
/// return fires when the derived value equals the stored one. The declaration
/// did not change, so **an imperative write alone will not provoke its own
/// repair** — something else must move the graph first. That is a real narrowing
/// of the old hazard rather than its removal: the old loop could not repair the
/// port at all without an unrelated edit *and* would then only revisit the ports
/// of declarations it happened to walk, whereas now the first rebuild of any
/// kind sweeps every declared port. Closing the gap completely would mean
/// deriving and comparing the value against the runtime every frame, which is
/// the per-frame cost the dirty gate exists to avoid.
///
/// Note what is asserted and in which order: the engine still holds the
/// imperative value immediately after the write (nothing has run since), and
/// holds the declared one again after the next rebuild. Asserting only the
/// second would pass even if the write had never landed.
///
/// **Mutation note.** Making `apply` skip its per-port comparison and trust the
/// `want == live` check that already ran — the tempting simplification, since
/// that check has just passed — fails the final assertion: the tampered port
/// belongs to a sink whose declaration did not move, so nothing would rewrite
/// it. Narrowing `apply` to visit only the sinks whose `PortSources` changed
/// this frame fails it for the same reason. Removing the imperative
/// `set_source` fails the middle assertion instead. All three verified.
#[test]
fn an_imperative_engine_write_is_repaired_from_the_value() {
    let mut app = app();
    let osc = spawn_node(&mut app, Osc::sine(Hz(440.0)));
    let other = spawn_node(&mut app, Osc::sine(Hz(880.0)));
    let tampered = spawn_node(&mut app, ChannelSumNode::new(2, ChannelLayout::MONO));
    // A second, entirely unrelated sink. Editing *this* one is what provokes the
    // rebuild, so the repair below is not the pass merely revisiting the
    // declaration it was asked about.
    let bystander = spawn_node(&mut app, ChannelSumNode::new(2, ChannelLayout::MONO));
    app.world_mut()
        .entity_mut(tampered)
        .insert(PortSources::silent().with(0, PortSource::node(osc)));
    app.world_mut()
        .entity_mut(bystander)
        .insert(PortSources::silent().with(0, PortSource::node(osc)));
    app.update();

    let tampered_id = node_id(&app, tampered);
    let osc_id = node_id(&app, osc);
    let other_id = node_id(&app, other);
    assert_eq!(
        app.world()
            .resource::<AudioGraphRes>()
            .source(tampered_id, 0),
        GraphSource::Node(osc_id, 0),
        "the declaration reached the engine to begin with"
    );

    // The hazard: a host reaches past the declaration and rewrites the port.
    // Nothing in the ECS changed.
    app.world_mut().resource_mut::<AudioGraphRes>().set_source(
        tampered_id,
        0,
        GraphSource::Node(other_id, 0),
    );
    assert_eq!(
        app.world()
            .resource::<AudioGraphRes>()
            .source(tampered_id, 0),
        GraphSource::Node(other_id, 0),
        "the imperative write landed — otherwise the repair below proves nothing"
    );

    // Move the *bystander's* declaration. That is a real change to the value, so
    // the rebuild gets past the early return — and `apply` then sweeps every
    // declared port, not only the one that moved.
    app.world_mut()
        .entity_mut(bystander)
        .insert(PortSources::silent().with(0, PortSource::node(other)));
    app.update();

    assert_eq!(
        app.world()
            .resource::<AudioGraphRes>()
            .source(tampered_id, 0),
        GraphSource::Node(osc_id, 0),
        "the value put the engine back on a port no declaration touched this \
         frame: `apply` compares every port the value names against the runtime, \
         so an imperative write cannot survive the next rebuild of any kind"
    );
    // And the edit that provoked it landed too, so the sweep did not simply
    // overwrite everything with the previous frame's value.
    let bystander_id = node_id(&app, bystander);
    assert_eq!(
        app.world()
            .resource::<AudioGraphRes>()
            .source(bystander_id, 0),
        GraphSource::Node(other_id, 0),
        "the declaration that changed reached the engine as well"
    );
}

/// `remove::<AudioNode>()` without despawning the entity takes the node out of
/// the value.
///
/// The removal path is an `On<Remove, AudioNode>` observer, so it fires for a
/// bare component removal exactly as it does for a despawn — but the *entity*
/// survives, keeping its `PortSources`. The value must drop both the node and
/// every edge naming it, or it would claim an edge to a key that is not in
/// `nodes` — which is `Invalid::UnknownNode`, not silence.
///
/// **This test found a real defect and is the reason it is fixed.** Before
/// `RemovedComponents<AudioNode>` joined `rebuild`'s dirty gate, none of its
/// five arms fired for a bare component removal — `Changed` does not report a
/// removal, the `PortSources` are untouched, and `MasterSources` did not move —
/// so the pass did not run at all. The *engine* was repaired (the
/// `On<Remove, AudioNode>` observer zeroes every edge), and the declaration side
/// was never re-derived. That was unobservable while the engine was the only
/// record of the graph; recording what the declarations mean is what exposed it.
///
/// **Mutation note.** Removing the `RemovedComponents<AudioNode>` arm from
/// `rebuild`'s gate fails the first assertion — verified, and it is how the
/// defect was found. Dropping `build`'s `graph.contains(node.0)` guard would
/// keep a spec for a node the engine no longer holds. Making `source_of` fall
/// back to `Source::Zero` for an unresolvable node fails the `edges.is_empty()`
/// assertion, since the edge would be present as silence rather than absent.
#[test]
fn removing_the_component_without_despawning_takes_the_node_out_of_the_value() {
    let mut app = app();
    let osc = spawn_node(&mut app, Osc::sine(Hz(440.0)));
    let sink = spawn_node(&mut app, ChannelSumNode::new(2, ChannelLayout::MONO));
    app.world_mut()
        .entity_mut(sink)
        .insert(PortSources::silent().with(0, PortSource::node(osc)));
    app.update();
    assert!(live(&app).nodes.contains_key(&key_of(osc)));

    // The component only — the entity stays, and so does the declaration
    // naming it as a source.
    app.world_mut().entity_mut(osc).remove::<AudioNode>();
    app.update();

    let topology = live(&app);
    assert!(
        !topology.nodes.contains_key(&key_of(osc)),
        "the node left the value with its component"
    );
    assert!(
        topology.edges.is_empty(),
        "and so did the edge naming it: an edge to an absent key is \
         Invalid::UnknownNode, not silence"
    );
    assert!(
        app.world().get_entity(osc).is_ok(),
        "the entity itself survives — this is the case despawn does not cover"
    );
}

/// A crossfade replaces the unit behind an entity and keeps the sink wired to
/// whatever node the entity now carries.
///
/// The declaration names an entity, so a replacement it does not observe cannot
/// strand it. In the value this is visible as an edge whose `NodeKey` is
/// **unchanged** — a crossfade is a node replacement at the same key, which is
/// exactly the identity `NodeKey` exists to provide and `NodeId` does not.
///
/// **Mutation note.** Keying the value on `AudioNode.0` (the engine's own id)
/// instead of the entity's bits fails this: `Net::crossfade` keeps the id
/// today, but nothing in the value would then survive the *rebind* case above,
/// and the edge assertion would name whichever id happened to win. Verified by
/// checking the key against `key_of(osc)` rather than against a captured value.
#[test]
fn a_crossfade_keeps_the_sink_wired_to_the_entitys_key() {
    let mut app = app();
    let osc = spawn_node(&mut app, Osc::sine(Hz(440.0)));
    let sink = spawn_node(&mut app, ChannelSumNode::new(2, ChannelLayout::MONO));
    app.world_mut()
        .entity_mut(sink)
        .insert(PortSources::silent().with(0, PortSource::node(osc)));
    app.update();

    let before = live(&app).clone();
    let osc_id = node_id(&app, osc);

    {
        let world = app.world_mut();
        let mut commands = world.commands();
        bevy_tutti::graph::crossfade_audio_node(&mut commands, osc, Box::new(Osc::sine(Hz(880.0))));
    }
    app.world_mut().flush();
    app.update();

    assert_eq!(
        node_id(&app, osc),
        osc_id,
        "the crossfade keeps the NodeId, so the entity's binding is untouched"
    );
    let after = live(&app);
    assert_eq!(
        after.edges.get(&InPort {
            node: key_of(sink),
            port: 0
        }),
        Some(&Edge::Direct(Source::Node(OutPort {
            node: key_of(osc),
            port: 0
        }))),
        "the sink still names the same key — the replacement is invisible to \
         the declaration, which is the property `PortSource::Node(Entity)` buys"
    );
    assert_eq!(
        before.edges, after.edges,
        "and the whole edge set is unchanged: a crossfade moves no wire"
    );
}

/// PDC shrinks when the latency-bearing node leaves.
///
/// The plan is a fold over the value, so this needs no device and no
/// compensation pass — `latency::plan` over `LiveGraph` is the same function
/// `compensate_graph` drives against the `Net`, and the figure it produces is
/// the one a DAW displays.
///
/// **Mutation note.** Reading `NodeSpec::latency` as `Samples::ZERO` for every
/// node (the tempting simplification, since only the shape is "topology") makes
/// the first `assert!(before > 0)` fail. Leaving a removed node in the value
/// keeps the compensation at its old figure and fails the shrink assertion.
#[test]
fn the_latency_plan_shrinks_when_the_latency_bearing_node_leaves() {
    let mut app = app();
    // Two paths into the master: one through a lookahead limiter, one dry. The
    // dry channel must pre-roll to match, which is the whole figure.
    let dry = spawn_node(&mut app, Const::mono(1.0));
    let src = spawn_node(&mut app, Const::mono(1.0));
    let lim = spawn_node(
        &mut app,
        LimiterNode::with_channels(ChannelLayout::MONO, Db(-1.0), Db(-0.3)),
    );
    app.world_mut()
        .entity_mut(lim)
        .insert(PortSources::from(src));
    app.world_mut().insert_resource(
        MasterSources::default()
            .with(0, PortSource::node(lim))
            .with(1, PortSource::node(dry)),
    );
    app.update();

    let before = tutti_types::latency::plan(live(&app));
    assert!(
        before.total().get() > 0,
        "the limiter reports latency, so the graph has a figure to display"
    );

    // Take the limiter out. The master channel it fed becomes unresolvable and
    // falls to `Zero`, which is what the engine holds for a channel whose
    // source left.
    app.world_mut().entity_mut(lim).despawn();
    app.update();

    let after = tutti_types::latency::plan(live(&app));
    assert!(
        after.total() < before.total(),
        "the plan shrank: {:?} -> {:?}",
        before.total(),
        after.total()
    );
    assert_eq!(
        after.total(),
        tutti_types::Samples(0),
        "and with nothing latency-bearing left, to nothing at all"
    );
}

/// N spawns in one frame end in **one** commit.
///
/// `GraphDirty` is the coalescing flag, and the property is that it is a flag
/// and not a counter: every spawn sets it, `commit_graph` clears it once. The
/// value is what makes the assertion cheap — all N nodes are in one topology,
/// so "did they all land in the same pass" is a length check rather than a
/// per-node engine read.
///
/// **Mutation note.** Moving `commit_graph`'s `dirty.0 = false` above the
/// commit, or committing per spawn inside `spawn_audio_node`, both leave the
/// flag observable; this asserts it is *clear* after the frame, which is the
/// statement that exactly one commit consumed all four edits. Removing the
/// `dirty.0 = true` from `spawn_audio_node` fails the `nodes.len()` assertion
/// instead, since the wire pass would never run.
#[test]
fn many_spawns_in_one_frame_coalesce_into_one_commit() {
    let mut app = app();
    let spawned: Vec<Entity> = (0..4)
        .map(|i| spawn_node(&mut app, Osc::sine(Hz(440.0 * (i + 1) as f32))))
        .collect();
    // One declaration, so the wire pass has something to do and the value is
    // built rather than skipped by the dirty gate.
    let sink = spawn_node(&mut app, ChannelSumNode::new(2, ChannelLayout::MONO));
    app.world_mut()
        .entity_mut(sink)
        .insert(PortSources::silent().with(0, PortSource::node(spawned[0])));

    app.update();

    assert!(
        !app.world().resource::<GraphDirty>().0,
        "one commit consumed the whole frame's edits"
    );
    assert_eq!(
        live(&app).nodes.len(),
        5,
        "and every node spawned this frame is in the same topology"
    );
    for e in &spawned {
        assert!(live(&app).nodes.contains_key(&key_of(*e)));
    }
}

/// An entity whose node has not arrived is **absent** from the value, not
/// present as a placeholder.
///
/// `insert_audio_node` lands as a deferred command, so a declaration routinely
/// names an entity a frame before its node exists. A placeholder spec would
/// make the value claim a node that does not exist, and the next frame's
/// comparison would read an arrival as a change.
///
/// **Mutation note.** Emitting a default `NodeSpec` for an unbound entity makes
/// the first `assert!(!contains_key)` fail; resolving its edge anyway (dropping
/// `source_of`'s `topology.nodes.get(&key)?`) makes the `edges.is_empty()`
/// assertion fail with an edge naming a key that is not in `nodes`.
#[test]
fn an_entity_whose_node_has_not_arrived_is_absent_rather_than_a_placeholder() {
    let mut app = app();
    let pending = app.world_mut().spawn_empty().id();
    let sink = spawn_node(&mut app, ChannelSumNode::new(2, ChannelLayout::MONO));
    app.world_mut()
        .entity_mut(sink)
        .insert(PortSources::silent().with(0, PortSource::node(pending)));
    app.update();

    let topology = live(&app);
    assert!(
        !topology.nodes.contains_key(&key_of(pending)),
        "absent, not a placeholder — the honest answer while the node is en route"
    );
    assert!(
        topology.edges.is_empty(),
        "and the edge naming it is absent too, rather than an UnknownNode fault"
    );

    // The node arrives; the value picks it up with no declaration change.
    let id = app
        .world_mut()
        .resource_mut::<AudioGraphRes>()
        .insert(Osc::sine(Hz(440.0)));
    app.world_mut().entity_mut(pending).insert(id);
    app.update();

    assert!(
        live(&app).nodes.contains_key(&key_of(pending)),
        "`Changed<AudioNode>` is in the dirty gate, so the arrival re-enters the pass"
    );
    assert_eq!(live(&app).edges.len(), 1, "and the edge resolves");
}

/// The value validates, and reports faults rather than panicking.
///
/// The check a type index would have performed, run over a graph the ECS
/// actually produced. `Unconnected` is reported but not fatal — a half-wired
/// sink reads silence, which is what a graph looks like mid-edit — so a graph
/// with an undeclared port still validates.
///
/// **Mutation note.** Making `build` emit an edge past a sink's declared input
/// width (dropping the `min(spec.inputs.count())` clamp) turns this `Ok` into
/// an `Err` carrying `SinkPortOutOfRange`.
#[test]
fn the_value_the_adapter_builds_validates() {
    let mut app = app();
    let osc = spawn_node(&mut app, Osc::sine(Hz(440.0)));
    // Two inputs, one declared: port 1 is undeclared, which is legal.
    let sink = spawn_node(&mut app, ChannelSumNode::new(2, ChannelLayout::MONO));
    app.world_mut()
        .entity_mut(sink)
        .insert(PortSources::silent().with(0, PortSource::node(osc)));
    app.world_mut()
        .insert_resource(MasterSources::mono_from(sink));
    app.update();

    let topology = live(&app);
    let valid = topology
        .validate()
        .expect("a graph the adapter built is structurally sound");
    assert_eq!(valid.get(), topology);
    assert_eq!(
        topology.unconnected().count(),
        1,
        "the undeclared port is reported, and is not a fault"
    );
}

/// Re-binding an entity to a different node moves the wire, **even though the
/// value does not change**.
///
/// The one case where `want == live` is true and a rebuild is still required.
/// A [`NodeKey`] is an `Entity` — deliberately, since that is what lets a
/// crossfade replace a unit without moving a wire — so inserting a different
/// `AudioNode` on the same entity changes which `NodeId` the declaration
/// resolves to while leaving the key alone. Two nodes of the same shape produce
/// equal values, so the early return would fire and every edge naming that
/// entity would keep pointing at the retired node, which nothing renders.
///
/// The entity→`NodeId` mapping is engine state the value does not carry, so it
/// takes an engine-side signal — `Changed<AudioNode>` — to notice. This test is
/// why that signal bypasses the value comparison rather than the value being
/// taught to carry a `NodeId`: carrying one would reintroduce the stale-id
/// problem `PortSource::Node(Entity)` exists to remove, and would make a
/// crossfade look like a topology change.
///
/// **Mutation note.** Removing `rebound_this_frame` from `rebuild`'s early
/// return fails the final assertion — verified, and it is how this was found
/// (`graph_wire::re_binding_an_entity_to_a_new_node_re_derives_the_wire` failed
/// first). Asserting only that the value is unchanged would pass with the bug
/// present, which is why the engine read is the assertion that matters here.
#[test]
fn a_rebind_moves_the_wire_though_the_value_is_unchanged() {
    let mut app = app();
    let osc = spawn_node(&mut app, Osc::sine(Hz(440.0)));
    let sink = spawn_node(&mut app, ChannelSumNode::new(2, ChannelLayout::MONO));
    app.world_mut()
        .entity_mut(sink)
        .insert(PortSources::silent().with(0, PortSource::node(osc)));
    app.update();

    let before = live(&app).clone();
    let sink_id = node_id(&app, sink);

    // Same entity, a different node of the same shape.
    let second = {
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        graph.insert(Osc::sine(Hz(880.0)))
    };
    app.world_mut().entity_mut(osc).insert(second);
    app.update();

    assert_eq!(
        *live(&app),
        before,
        "the value genuinely cannot see this: same entity, same shape, so the \
         topology is identical and the comparison alone would skip the frame"
    );
    assert_eq!(
        app.world().resource::<AudioGraphRes>().source(sink_id, 0),
        GraphSource::Node(second, 0),
        "and the wire moved anyway — `Changed<AudioNode>` is the engine-side \
         signal that the mapping the value resolves through has moved"
    );
}
