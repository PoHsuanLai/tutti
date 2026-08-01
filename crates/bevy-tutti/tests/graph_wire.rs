//! Wiring declared in the ECS reaches the graph, and stops reaching it when the
//! declaration goes away.
//!
//! Every assertion reads the engine back through `Net::source` /
//! `Net::output_source` rather than trusting the component, because the diff
//! this layer performs is only meaningful if the engine is the thing being
//! compared against.

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;

use bevy_tutti::graph::{
    AudioGraphRes, AudioSource, AudioSources, GraphDirty, GraphReconcilePlugin,
    GraphReconcileSystems, MasterSources,
};
use bevy_tutti::AudioEngineState;
// `outputs()` on `Net` is an `AudioUnit` method — the graph's own arity.
use tutti_core::dsp::{pass, sine_hz, AudioUnit as _, Net, Source};
use tutti_core::AudioNode;

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

/// The headline claim: a declaration on the sink reaches the engine.
#[test]
fn a_declared_source_reaches_the_graph() {
    let mut app = app();
    let osc = spawn_node(&mut app, sine_hz::<f32>(440.0));
    let filt = spawn_node(&mut app, pass());

    app.world_mut()
        .entity_mut(filt)
        .insert(AudioSources::from(osc));
    app.update();

    let (osc_id, filt_id) = (node_id(&app, osc), node_id(&app, filt));
    assert_eq!(
        app.world().resource::<AudioGraphRes>().0.source(filt_id, 0),
        Source::Local(osc_id, 0)
    );
}

/// The master bus is one declaration with one value per channel, so two nodes
/// cannot both claim it.
///
/// This is the inverse of `master_bus.rs`'s
/// `a_second_pipe_output_silently_replaces_the_first`: there, the second caller
/// silently won. Here there is no second caller to have — a resource holds one
/// value, and a channel holds one source.
#[test]
fn the_master_bus_has_one_declaration_not_a_race() {
    let mut app = app();
    let a = spawn_node(&mut app, sine_hz::<f32>(440.0));
    let b = spawn_node(&mut app, sine_hz::<f32>(880.0));

    // Both nodes exist and both want the master. Only a declaration decides.
    app.world_mut()
        .insert_resource(MasterSources::from(a).with(1, AudioSource::node(b)));
    app.update();

    let graph = app.world().resource::<AudioGraphRes>();
    assert_eq!(graph.0.output_source(0), Source::Local(node_id(&app, a), 0));
    assert_eq!(graph.0.output_source(1), Source::Local(node_id(&app, b), 0));
}

/// Removing the declaration silences the ports it claimed.
///
/// The case that is unsolvable imperatively without every call site remembering
/// what it wired: the entity leaves the rebuild's query, so only the removal
/// observer can zero those ports.
#[test]
fn removing_the_declaration_silences_the_ports() {
    let mut app = app();
    let osc = spawn_node(&mut app, sine_hz::<f32>(440.0));
    let filt = spawn_node(&mut app, pass());
    let filt_id = node_id(&app, filt);

    app.world_mut()
        .entity_mut(filt)
        .insert(AudioSources::from(osc));
    app.update();
    assert_ne!(
        app.world().resource::<AudioGraphRes>().0.source(filt_id, 0),
        Source::Zero
    );

    app.world_mut().entity_mut(filt).remove::<AudioSources>();
    app.update();

    assert_eq!(
        app.world().resource::<AudioGraphRes>().0.source(filt_id, 0),
        Source::Zero,
        "a removed declaration must not leave its last wiring behind"
    );
}

/// A declaration naming an entity whose node arrives later is skipped, then
/// picked up — without anything about the declaration changing.
///
/// This is what `Added<AudioNode>` in the rebuild's dirty gate is for. Gating on
/// `Changed<AudioSources>` alone would leave the wire unformed forever.
#[test]
fn an_unresolvable_source_is_skipped_then_picked_up() {
    let mut app = app();
    let filt = spawn_node(&mut app, pass());
    let filt_id = node_id(&app, filt);

    // An entity with no node yet.
    let pending = app.world_mut().spawn_empty().id();
    app.world_mut()
        .entity_mut(filt)
        .insert(AudioSources::from(pending));
    app.update();
    assert_eq!(
        app.world().resource::<AudioGraphRes>().0.source(filt_id, 0),
        Source::Zero,
        "nothing to resolve yet, and no panic"
    );

    // The node turns up. The declaration is untouched.
    let id = {
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        graph.0.add(sine_hz::<f32>(440.0))
    };
    app.world_mut().entity_mut(pending).insert(AudioNode(id));
    app.update();

    assert_eq!(
        app.world().resource::<AudioGraphRes>().0.source(filt_id, 0),
        Source::Local(id, 0),
        "the wire forms once the node exists — nothing about the declaration \
         changed, so a gate on `Changed<AudioSources>` alone would miss it"
    );
}

/// A node naming itself is skipped with a warning, not a panic.
///
/// `Net::set_source` asserts on a self-connection, and an assert inside a
/// reconcile system takes the app down over a caller's typo.
#[test]
fn a_self_connection_is_skipped_not_panicked_on() {
    let mut app = app();
    let filt = spawn_node(&mut app, pass());

    app.world_mut()
        .entity_mut(filt)
        .insert(AudioSources::from(filt));
    app.update();

    assert_eq!(
        app.world()
            .resource::<AudioGraphRes>()
            .0
            .source(node_id(&app, filt), 0),
        Source::Zero
    );
}

/// A source port past the node's output count is skipped rather than asserting.
#[test]
fn an_out_of_range_source_port_is_skipped() {
    let mut app = app();
    let mono = spawn_node(&mut app, sine_hz::<f32>(440.0));
    let filt = spawn_node(&mut app, pass());

    app.world_mut()
        .entity_mut(filt)
        .insert(AudioSources::silent().with(
            0,
            AudioSource::Node {
                entity: mono,
                port: 7,
            },
        ));
    app.update();

    assert_eq!(
        app.world()
            .resource::<AudioGraphRes>()
            .0
            .source(node_id(&app, filt), 0),
        Source::Zero
    );
}

/// Re-binding an entity to a different node re-derives every wire naming it.
///
/// This is the whole reason `AudioSource::Node` holds an `Entity` rather than a
/// `NodeId` — and it did not work: the dirty gate was `Added<AudioNode>`, but a
/// replacement `insert` on an entity that already has the component fires
/// `Changed` without `Added`. Wires kept pointing at the retired node forever.
#[test]
fn re_binding_an_entity_to_a_new_node_re_derives_the_wire() {
    let mut app = app();
    let osc = spawn_node(&mut app, sine_hz::<f32>(440.0));
    let filt = spawn_node(&mut app, pass());
    let filt_id = node_id(&app, filt);

    app.world_mut()
        .entity_mut(filt)
        .insert(AudioSources::from(osc));
    app.update();
    let first = node_id(&app, osc);
    assert_eq!(
        app.world().resource::<AudioGraphRes>().0.source(filt_id, 0),
        Source::Local(first, 0)
    );

    // Same entity, different node.
    let second = {
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        graph.0.add(sine_hz::<f32>(880.0))
    };
    app.world_mut().entity_mut(osc).insert(AudioNode(second));
    app.update();

    assert_eq!(
        app.world().resource::<AudioGraphRes>().0.source(filt_id, 0),
        Source::Local(second, 0),
        "the declaration names an entity, so re-binding that entity must move \
         the wire — otherwise it points at a node nothing renders"
    );
}

/// Removing a declaration silences only the ports it claimed.
///
/// The write path clamps to the declared length, so the removal path must too.
/// Zeroing every input port instead reaches into wiring this layer never made.
#[test]
fn removing_a_declaration_leaves_undeclared_ports_alone() {
    let mut app = app();
    let declared_src = spawn_node(&mut app, sine_hz::<f32>(440.0));
    let foreign_src = spawn_node(&mut app, sine_hz::<f32>(880.0));
    // Two inputs; the declaration will claim only port 0.
    let sink = spawn_node(&mut app, pass() | pass());
    let (sink_id, foreign_id) = (node_id(&app, sink), node_id(&app, foreign_src));

    app.world_mut()
        .entity_mut(sink)
        .insert(AudioSources::from(declared_src));
    // Port 1 wired by hand — undeclared, so this layer must not own it.
    {
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        graph.0.set_source(sink_id, 1, Source::Local(foreign_id, 0));
    }
    app.update();

    app.world_mut().entity_mut(sink).remove::<AudioSources>();
    app.update();

    let graph = app.world().resource::<AudioGraphRes>();
    assert_eq!(
        graph.0.source(sink_id, 0),
        Source::Zero,
        "the declared port is released"
    );
    assert_eq!(
        graph.0.source(sink_id, 1),
        Source::Local(foreign_id, 0),
        "but an undeclared port belongs to whoever wired it"
    );
}

/// A mono node reaches both master channels — via the constructor that says so.
///
/// `MasterSources::from` names ports 0 and 1, which is correct for a stereo
/// source and unresolvable for a mono one. Its doc used to promise `pipe_output`'s
/// modulo wrapping, which it never did: channel 1 was skipped, leaving whatever
/// the channel previously held still audible.
#[test]
fn a_mono_source_can_claim_both_master_channels() {
    let mut app = app();
    let mono = spawn_node(&mut app, sine_hz::<f32>(440.0));

    app.world_mut()
        .insert_resource(MasterSources::mono_from(mono));
    app.update();

    let mono_id = node_id(&app, mono);
    let graph = app.world().resource::<AudioGraphRes>();
    assert_eq!(graph.0.output_source(0), Source::Local(mono_id, 0));
    assert_eq!(
        graph.0.output_source(1),
        Source::Local(mono_id, 0),
        "both channels take the mono node's only port"
    );
}

/// A stereo master claim replaced by a mono one must not strand the old node.
///
/// The failure this guards: `from` on a mono node leaves channel 1 unresolvable,
/// the rebuild skips it, and the *previous* master keeps rendering on the right
/// — two nodes owning the bus, which is the defect the declarative layer exists
/// to make impossible.
#[test]
fn replacing_a_stereo_master_with_a_mono_one_releases_both_channels() {
    let mut app = app();
    let stereo = spawn_node(&mut app, pass() | pass());
    let mono = spawn_node(&mut app, sine_hz::<f32>(440.0));

    app.world_mut().insert_resource(MasterSources::from(stereo));
    app.update();
    let stereo_id = node_id(&app, stereo);
    assert_eq!(
        app.world().resource::<AudioGraphRes>().0.output_source(1),
        Source::Local(stereo_id, 1)
    );

    app.world_mut()
        .insert_resource(MasterSources::mono_from(mono));
    app.update();

    let mono_id = node_id(&app, mono);
    let graph = app.world().resource::<AudioGraphRes>();
    assert_eq!(graph.0.output_source(0), Source::Local(mono_id, 0));
    assert_ne!(
        graph.0.output_source(1),
        Source::Local(stereo_id, 1),
        "the retracted node must not still be feeding a channel"
    );
    assert_eq!(graph.0.output_source(1), Source::Local(mono_id, 0));
}

/// A rebuild that finds the engine already agreeing writes nothing — it does not
/// re-set ports that already hold the declared source.
///
/// This is what the diff buys. `Net::set_source` calls `invalidate_order()`,
/// throwing away the cached topological sort, so re-writing an unchanged port is
/// not free; and marking `GraphDirty` forces a commit the frame did not need.
///
/// Driven by adding a *second, unrelated* node, which dirties the rebuild via
/// `Added<AudioNode>` without changing any existing declaration. Without the
/// diff, every already-correct port is rewritten and the frame is dirtied.
///
/// The flag has to be sampled **between** the rebuild and `commit_graph`, which
/// clears it in the `Commit` set — reading it after `update()` returns shows
/// `false` whether or not the rebuild wrote, which is how a first version of
/// this test passed against a deliberately un-diffed rebuild.
#[test]
fn a_rebuild_that_changes_nothing_writes_nothing() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let mut app = app();
    let osc = spawn_node(&mut app, sine_hz::<f32>(440.0));
    let filt = spawn_node(&mut app, pass());
    app.world_mut()
        .entity_mut(filt)
        .insert(AudioSources::from(osc));
    app.update();

    // Sample GraphDirty after the rebuild and before the commit clears it.
    let dirtied = Arc::new(AtomicBool::new(false));
    let probe = dirtied.clone();
    app.add_systems(
        Update,
        (move |dirty: Res<GraphDirty>| {
            if dirty.0 {
                probe.store(true, Ordering::SeqCst);
            }
        })
        .after(GraphReconcileSystems::Compensate)
        .before(GraphReconcileSystems::Commit),
    );

    // A new node elsewhere: the rebuild runs, but nothing it already wired has
    // changed.
    spawn_node(&mut app, sine_hz::<f32>(880.0));
    app.update();

    assert!(
        !dirtied.load(Ordering::SeqCst),
        "a rebuild whose declarations all match the engine must not dirty the \
         graph — re-writing a correct port discards the cached node order and \
         forces a commit the frame did not need"
    );
}

// ---------------------------------------------------------------------------
// A master declaration wider than the root widens the root.
// ---------------------------------------------------------------------------

/// A 6-channel `MasterSources` on a stereo root must widen the root, not be
/// truncated to it.
///
/// Before this, `rebuild` clamped the loop to `graph.0.outputs()`, so channels
/// 2-5 were dropped with no warning and nothing in the ECS to inspect. The
/// second half of this test is what makes it a real regression: a fix that
/// widens the root but leaves the loop clamped passes the arity assertion and
/// still never wires channel 5.
#[test]
fn a_wider_master_declaration_widens_the_root() {
    let mut app = app();
    let wide = spawn_node(
        &mut app,
        tutti_core::dsp::multipass::<tutti_core::dsp::U6>(),
    );

    app.insert_resource(MasterSources::from_node_at_width(
        wide,
        tutti_core::ChannelLayout::from_count(6),
    ));
    app.update();

    let graph = app.world().resource::<AudioGraphRes>();
    assert_eq!(graph.0.outputs(), 6, "the declaration widened the root");

    let id = node_id(&app, wide);
    for channel in 0..6 {
        assert_eq!(
            graph.0.output_source(channel),
            Source::Local(id, channel),
            "channel {channel} must actually be wired, not merely reachable"
        );
    }
}

/// Widening must survive the real commit path. A plain `Net::commit` panics on
/// an arity change, so this is what proves the arity-permitting commit is
/// genuinely the one reached.
#[test]
fn a_widened_root_survives_a_real_commit() {
    let mut app = app();
    let wide = spawn_node(
        &mut app,
        tutti_core::dsp::multipass::<tutti_core::dsp::U6>(),
    );

    app.insert_resource(MasterSources::from_node_at_width(
        wide,
        tutti_core::ChannelLayout::from_count(6),
    ));
    app.update();
    // A second frame: the commit runs in `Commit`, after `rebuild`'s widening.
    app.update();

    let graph = app.world().resource::<AudioGraphRes>();
    assert_eq!(graph.0.outputs(), 6);
    assert!(
        !app.world().resource::<GraphDirty>().0,
        "the commit consumed the dirty flag rather than panicking on the arity change"
    );
}

/// A *shorter* declaration means undeclared, not "narrow the root".
///
/// Narrowing on a shortened `Vec` would tear down channels the host may own
/// imperatively — the same violation `unwire_removed_sources` refuses. It needs
/// its own explicit API, not an inference from a length.
#[test]
fn a_shorter_master_declaration_does_not_narrow_the_root() {
    let mut app = app();
    let wide = spawn_node(
        &mut app,
        tutti_core::dsp::multipass::<tutti_core::dsp::U6>(),
    );

    app.insert_resource(MasterSources::from_node_at_width(
        wide,
        tutti_core::ChannelLayout::from_count(6),
    ));
    app.update();
    assert_eq!(app.world().resource::<AudioGraphRes>().0.outputs(), 6);

    // Now declare only two channels.
    app.insert_resource(MasterSources::from(wide));
    app.update();

    assert_eq!(
        app.world().resource::<AudioGraphRes>().0.outputs(),
        6,
        "a shorter declaration is undeclared, not a narrowing instruction"
    );
}

/// Channels past `MAX_ROOT_CHANNELS` are refused at the clamp, not silently
/// dropped one layer down — the render scratch is bounded, so a root wider than
/// it would report channels that are declarable but never rendered.
#[test]
fn a_master_declaration_cannot_exceed_the_render_scratch() {
    let mut app = app();
    let node = spawn_node(&mut app, sine_hz::<f32>(440.0));

    let mut sources = MasterSources::default();
    for channel in 0..32 {
        sources = sources.with(channel, AudioSource::node(node));
    }
    app.insert_resource(sources);
    app.update();

    assert!(
        app.world().resource::<AudioGraphRes>().0.outputs() <= 8,
        "the root must stay within the render scratch"
    );
}

// ---------------------------------------------------------------------------
// The N-wide constructors.
// ---------------------------------------------------------------------------

/// `from_node_at_width` is the identity mapping — every channel straight
/// through, no wrap, no fold.
#[test]
fn from_node_at_width_maps_every_channel_straight_through() {
    let mut app = app();
    let sink = spawn_node(
        &mut app,
        tutti_core::dsp::multipass::<tutti_core::dsp::U6>(),
    );
    let src = spawn_node(
        &mut app,
        tutti_core::dsp::multipass::<tutti_core::dsp::U6>(),
    );

    app.world_mut()
        .entity_mut(sink)
        .insert(AudioSources::from_node_at_width(
            src,
            tutti_core::ChannelLayout::from_count(6),
        ));
    app.update();

    let graph = app.world().resource::<AudioGraphRes>();
    let (sink_id, src_id) = (node_id(&app, sink), node_id(&app, src));
    for port in 0..6 {
        assert_eq!(
            graph.0.source(sink_id, port),
            Source::Local(src_id, port),
            "port {port} must come from the matching source port"
        );
    }
}

/// At stereo it is exactly `stereo_from`. That equivalence is what lets a
/// reviewer trust every existing call site is unaffected by the new
/// constructor's arrival.
#[test]
fn from_node_at_width_at_stereo_is_stereo_from() {
    let mut app = app();
    let e = spawn_node(&mut app, pass());

    assert_eq!(
        AudioSources::from_node_at_width(e, tutti_core::ChannelLayout::STEREO),
        AudioSources::stereo_from(e)
    );
}
