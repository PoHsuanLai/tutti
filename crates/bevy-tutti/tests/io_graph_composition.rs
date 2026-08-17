//! The live I/O edge composes with the graph reconciler.
//!
//! `crate::io` re-exports engine types without wrapping them, on the claim that
//! they are already the right shape for a host to hold directly. That claim is
//! only worth anything if a `MicMonitorNode` really does reconcile like any
//! other node — declared through `PortSources`/`MasterSources`, reached by
//! `Net::output_source`, and carrying audio once wired.
//!
//! These read the engine back rather than trusting the component, for the same
//! reason `graph_wire.rs` does: the diff this layer performs is only meaningful
//! if the engine is what gets compared against.

// `crate::io` only exists under `audio-io`; same gate `audio_pump.rs` carries.
#![cfg(feature = "audio-io")]

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;
use ringbuf::traits::{Producer as _, Split as _};

use bevy_tutti::graph::{AudioGraphRes, GraphReconcilePlugin, MasterSources, PortSources};
use bevy_tutti::io::{MicMonitorNode, MicRing};
use bevy_tutti::AudioEngineState;
use tutti_core::dsp::{pass, AudioUnit as _, Net, Source};
use tutti_core::AudioNode;

/// An app wired the way `build_into` leaves one, minus the audio device.
/// Same shape as `graph_wire.rs`'s harness — deliberately, so a difference in
/// outcome is about the node under test and not the scaffolding.
fn app() -> App {
    let mut app = App::new();
    app.insert_resource(AudioGraphRes(Net::with_backend(2)));
    app.insert_resource(AudioEngineState::Running);
    app.add_plugins(GraphReconcilePlugin);
    app
}

/// A monitor node plus the producer end of the ring feeding it, so a test can
/// play the part of the capture callback without a device.
fn monitor_with_feed(capacity: usize) -> (MicMonitorNode, ringbuf::HeapProd<[f32; 2]>) {
    let (prod, cons) = ringbuf::HeapRb::<[f32; 2]>::new(capacity).split();
    let ring: MicRing = tutti_io::share_mic_ring(cons);
    (MicMonitorNode::new(ring), prod)
}

fn node_id(app: &App, entity: Entity) -> tutti_core::NodeId {
    app.world().get::<AudioNode>(entity).expect("AudioNode").0
}

/// Render `frames` from the graph, per-sample.
///
/// `tick` rather than a `process` block for the reason `midi_soundfont_audio.rs`
/// gives: `BufferVec` holds one SIMD block per channel, so `process` needs
/// fundsp's buffer types rather than plain slices. `tick` polls the same units.
fn render(app: &mut App, frames: usize) -> Vec<[f32; 2]> {
    let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
    (0..frames)
        .map(|_| {
            let mut frame = [0.0f32; 2];
            graph.0.tick(&[], &mut frame);
            frame
        })
        .collect()
}

/// The composition claim, at its narrowest: a monitor node is declared to the
/// master exactly like any other node, and the reconciler wires it.
///
/// If this fails, `io`'s "no wrapper needed" premise is wrong — the type would
/// need adapter-side help to reach the graph at all.
#[test]
fn a_monitor_node_reaches_the_master_like_any_other_node() {
    let mut app = app();
    let (monitor, _prod) = monitor_with_feed(64);

    let id = {
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        graph.0.add(monitor)
    };
    let entity = app.world_mut().spawn(AudioNode(id)).id();
    app.insert_resource(MasterSources::from(entity));
    app.update();

    let mon_id = node_id(&app, entity);
    for ch in 0..2 {
        assert_eq!(
            app.world().resource::<AudioGraphRes>().0.output_source(ch),
            Source::Local(mon_id, ch),
            "master channel {ch} must read from the monitor node"
        );
    }
}

/// A monitor can sit *upstream of an effect* rather than only at the master —
/// the "through effects if you like" the mic docs promise.
///
/// Asserted through `Net::source` on the effect's input port, which is what
/// makes this about the fan-in declaration and not just about the master.
#[test]
fn a_monitor_node_can_feed_an_effect_chain() {
    let mut app = app();
    let (monitor, _prod) = monitor_with_feed(64);

    let (mon_id, fx_id) = {
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        (graph.0.add(monitor), graph.0.add(pass()))
    };
    let mon = app.world_mut().spawn(AudioNode(mon_id)).id();
    let fx = app.world_mut().spawn(AudioNode(fx_id)).id();

    app.world_mut()
        .entity_mut(fx)
        .insert(PortSources::from(mon));
    app.insert_resource(MasterSources::from(fx));
    app.update();

    assert_eq!(
        app.world().resource::<AudioGraphRes>().0.source(fx_id, 0),
        Source::Local(mon_id, 0),
        "the effect's input must read from the monitor"
    );
}

/// Frames pushed by a (simulated) capture callback come out of the graph.
///
/// The two tests above pin the *topology*; this one pins that the topology
/// carries audio. Without it, a monitor wired to a ring it never drains would
/// pass both of them while producing silence — which is exactly the trap
/// `io`'s module docs warn about, so it deserves an assertion rather than a
/// paragraph.
#[test]
fn frames_pushed_by_the_capture_callback_reach_the_graph_output() {
    let mut app = app();
    let (monitor, mut prod) = monitor_with_feed(512);

    let id = {
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        graph.0.add(monitor)
    };
    let entity = app.world_mut().spawn(AudioNode(id)).id();
    app.insert_resource(MasterSources::from(entity));
    app.update();

    // Play the capture callback: push a constant, distinguishable frame.
    for _ in 0..128 {
        prod.try_push([0.5, -0.5]).expect("ring has room");
    }

    let out = render(&mut app, 64);

    assert!(
        out.iter().all(|f| (f[0] - 0.5).abs() < 1e-6),
        "left must carry the pushed frames, got {:?}",
        &out[..4]
    );
    assert!(
        out.iter().all(|f| (f[1] + 0.5).abs() < 1e-6),
        "right must carry the pushed frames, got {:?}",
        &out[..4]
    );
}

/// An *undeclared* monitor is silently dropped — the first trap `io`'s docs
/// name, pinned as behaviour so the warning cannot quietly stop being true.
///
/// The node is in the graph but nothing reads it, so the master stays silent.
/// This is the failure mode that looks like a connected monitor and produces
/// nothing.
#[test]
fn an_undeclared_monitor_produces_silence_at_the_master() {
    let mut app = app();
    let (monitor, mut prod) = monitor_with_feed(512);

    {
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        // Added to the graph — but never declared to `MasterSources`, which is
        // the step a host forgets.
        let _ = graph.0.add(monitor);
    }
    app.update();

    for _ in 0..128 {
        prod.try_push([0.5, -0.5]).expect("ring has room");
    }

    let out = render(&mut app, 64);

    assert!(
        out.iter().all(|f| f[0] == 0.0 && f[1] == 0.0),
        "an undeclared monitor must not reach the master — if this fails the \
         trap documented in `io`'s module docs has changed shape"
    );
}
