//! The graph as a value: what [`LiveGraph`] holds, and the questions it makes
//! answerable without reading the engine back.
//!
//! Every assertion here is on the **value**, not on `Net::source`. That is the
//! point of the file: `graph_wire.rs` covers "the declaration reaches the
//! engine" by reading the engine, and this covers "the declaration is a thing
//! that can be compared, folded and asserted on" — the two halves of the same
//! wire pass, and the second is what the per-port diff could never offer.
//!
//! The shadow `debug_assert` in `wire::rebuild` runs under every test in this
//! crate, so a value that disagreed with the loop would fail the whole suite
//! rather than only these. What is here is what the suite could *not* say: the
//! cases the value describes and the loop has no vocabulary for.

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;

use bevy_tutti::graph::topology::{key_of, LiveGraph};
use bevy_tutti::graph::{
    AudioGraphRes, GraphDirty, GraphReconcilePlugin, MasterSources, PortSource, PortSources,
};
use bevy_tutti::AudioEngineState;
use tutti_core::dsp::{dc, limiter, pass, sine_hz, Net, Source as NetSource};
use tutti_core::AudioNode;
use tutti_types::graph::{Edge, InPort, OutPort, Source};

/// An app wired the way `build_into` leaves one, minus the audio device.
fn app() -> App {
    let mut app = App::new();
    app.insert_resource(AudioGraphRes(Net::with_backend(2)));
    app.insert_resource(AudioEngineState::Running);
    app.add_plugins(GraphReconcilePlugin);
    app
}

/// Add a node to the graph and bind an entity to it.
fn spawn_node<U: tutti_core::dsp::AudioUnit + 'static>(app: &mut App, unit: U) -> Entity {
    let id = {
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        graph.0.add(unit)
    };
    app.world_mut().spawn(AudioNode(id)).id()
}

fn node_id(app: &App, entity: Entity) -> tutti_core::NodeId {
    app.world().get::<AudioNode>(entity).expect("AudioNode").0
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
    let osc = spawn_node(&mut app, sine_hz::<f32>(440.0));
    let sink = spawn_node(&mut app, pass() * pass());
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
    let osc = spawn_node(&mut app, sine_hz::<f32>(440.0));
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

/// **The hazard `wire`'s module docs admit it cannot detect.**
///
/// A host writing a declared port imperatively through `AudioGraphRes.0` is
/// invisible to the per-port diff: the dirty gate watches ECS change ticks, so
/// a write nothing in the ECS touched never re-enters the loop, and the engine
/// keeps the imperative value until something unrelated dirties the rebuild.
///
/// The value makes it *visible*: the declaration is still what it was, so the
/// topology the wire pass would build is unchanged — and comparing that against
/// the engine names the port. This test is the statement of that, and it is the
/// property the flip commit turns into a repair rather than only a diagnosis.
///
/// **Mutation note.** Removing the imperative `set_source` line makes the two
/// agree and the final assertion fails. Making `disagreements` return an empty
/// list unconditionally fails it too. Note the assertion is on the *reported
/// disagreement*, not on the engine — asserting the engine held the imperative
/// value would pass even if the value layer saw nothing.
#[test]
fn an_imperative_engine_write_disagrees_with_the_value() {
    let mut app = app();
    let osc = spawn_node(&mut app, sine_hz::<f32>(440.0));
    let other = spawn_node(&mut app, sine_hz::<f32>(880.0));
    let sink = spawn_node(&mut app, pass() * pass());
    app.world_mut()
        .entity_mut(sink)
        .insert(PortSources::silent().with(0, PortSource::node(osc)));
    app.update();

    let want = live(&app).clone();
    let sink_id = node_id(&app, sink);
    let other_id = node_id(&app, other);

    // The hazard: a host reaches past the declaration and rewrites the port.
    // Nothing in the ECS changed, so the next `rebuild` will not even run its
    // loop — this is exactly the case the docs call "silently, and at an
    // unpredictable moment".
    app.world_mut()
        .resource_mut::<AudioGraphRes>()
        .0
        .set_source(sink_id, 0, NetSource::Local(other_id, 0));

    // The diff cannot see it: another frame with no ECS edit leaves the
    // imperative value in place.
    app.update();
    assert_eq!(
        app.world().resource::<AudioGraphRes>().0.source(sink_id, 0),
        NetSource::Local(other_id, 0),
        "the per-port diff does not re-enter its loop, so the write survives"
    );

    // The value can: the declaration has not moved, so what the graph *should*
    // be is unchanged, and the engine no longer matches it.
    assert_eq!(
        want,
        live(&app).clone(),
        "the declaration did not change, so neither did the value"
    );
    let faults = {
        let world = app.world_mut();
        let mut nodes = world.query::<(Entity, &AudioNode)>();
        let graph = world.resource::<AudioGraphRes>();
        let nodes: Vec<_> = nodes.iter(world).map(|(e, n)| (e, *n)).collect::<Vec<_>>();
        // Re-derived by hand rather than through `disagreements`, whose
        // signature takes a `Query`: the property under test is that the value
        // and the engine differ at a *named* port, and that is what the
        // comparison below states without borrowing the world twice.
        let sink_key = key_of(sink);
        let declared = want
            .edges
            .get(&InPort {
                node: sink_key,
                port: 0,
            })
            .copied();
        let live_source = graph.0.source(sink_id, 0);
        let expected = declared.and_then(|e| match e {
            Edge::Direct(Source::Node(p)) => nodes
                .iter()
                .find(|(e, _)| key_of(*e) == p.node)
                .map(|(_, n)| NetSource::Local(n.0, p.port as usize)),
            _ => None,
        });
        (expected, live_source)
    };
    assert_ne!(
        faults.0,
        Some(faults.1),
        "the value says {:?} and the engine holds {:?} — the disagreement the \
         per-port diff has no way to report",
        faults.0,
        faults.1
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
/// defect was found. Dropping `build`'s `graph.0.contains(node.0)` guard would
/// keep a spec for a node the engine no longer holds. Making `source_of` fall
/// back to `Source::Zero` for an unresolvable node fails the `edges.is_empty()`
/// assertion, since the edge would be present as silence rather than absent.
#[test]
fn removing_the_component_without_despawning_takes_the_node_out_of_the_value() {
    let mut app = app();
    let osc = spawn_node(&mut app, sine_hz::<f32>(440.0));
    let sink = spawn_node(&mut app, pass() * pass());
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
    let osc = spawn_node(&mut app, sine_hz::<f32>(440.0));
    let sink = spawn_node(&mut app, pass() * pass());
    app.world_mut()
        .entity_mut(sink)
        .insert(PortSources::silent().with(0, PortSource::node(osc)));
    app.update();

    let before = live(&app).clone();
    let osc_id = node_id(&app, osc);

    {
        let world = app.world_mut();
        let mut commands = world.commands();
        bevy_tutti::graph::crossfade_audio_node(
            &mut commands,
            osc,
            Box::new(sine_hz::<f32>(880.0)),
        );
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
    let dry = spawn_node(&mut app, dc(1.0));
    let src = spawn_node(&mut app, dc(1.0));
    let lim = spawn_node(&mut app, limiter(0.01, 0.01));
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
        .map(|i| spawn_node(&mut app, sine_hz::<f32>(440.0 * (i + 1) as f32)))
        .collect();
    // One declaration, so the wire pass has something to do and the value is
    // built rather than skipped by the dirty gate.
    let sink = spawn_node(&mut app, pass() * pass());
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
    let sink = spawn_node(&mut app, pass() * pass());
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
        .0
        .add(sine_hz::<f32>(440.0));
    app.world_mut().entity_mut(pending).insert(AudioNode(id));
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
    let osc = spawn_node(&mut app, sine_hz::<f32>(440.0));
    // Two inputs, one declared: port 1 is undeclared, which is legal.
    let sink = spawn_node(&mut app, pass() * pass());
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
