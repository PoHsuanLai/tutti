//! A hosted plugin's latency is the session's, and a restart onto another
//! callback size re-declares it (design 015 §4.3), over the reference CLAP
//! plugin in a real `plugin-server`.
//!
//! These are the session halves of `bevy-tutti`'s `plugin_capture` (the
//! latency half of `a_loaded_plugin_is_bound_through_its_shadow_and_its_latency_is_the_sessions`,
//! `a_crossfaded_plugin_is_bound_again`) and of its
//! `a_restart_to_another_callback_size_re_declares_a_plugins_latency`, moved
//! here when the adapter was parked. What stayed with the adapter is
//! entity-shaped: the `PluginShadow`, the `PluginMeterBound` marker, the
//! meter installed on a crossfade's incoming client (the adapter's
//! `plugin_bind_meter`; `Session::crossfade_plugin` installs none).
//!
//! **Build `plugin-server` first** (`cargo build -p tutti-plugin-server`):
//! it is not a dev-dependency (a cycle through `tutti-plugin`), and without it
//! every test here fails, naming that command. They must fail rather than
//! skip.

mod support;

use std::time::Instant;

use support::{clap_probe, plugin_server};
use tutti_cpal::{ManualStreamDriver, OutputSpec};
use tutti_plugin::catalog::PluginId;
use tutti_plugin::handles::PluginClient;
use tutti_session::{
    Crossfade, GraphSource, PHASES, Phase, PhaseCx, PluginLoadRequest, Session, SessionEvent,
};
use tutti_types::{ChannelLayout, SampleRate, Samples};

const RATE: SampleRate = SampleRate(48_000.0);
const BLOCK: usize = 1_024;

/// SAFETY: nextest runs each test in its own process, and the server's path
/// is set before the first server is spawned, from the test's own thread.
fn set_server() {
    unsafe { std::env::set_var("TUTTI_PLUGIN_SERVER", plugin_server()) };
}

fn main_thread() -> tutti_plugin_types::MainThread {
    tutti_plugin_types::MainThread::claim().expect("one test, one process")
}

/// An offline stereo session at 48 kHz whose block is the plugin's pipeline
/// chunk ([`BLOCK`]: with no device callback size known, a plugin ships its
/// chunk at the graph's `MaxBlock`).
fn offline() -> Session {
    set_server();
    Session::builder()
        .build_offline(RATE, ChannelLayout::STEREO, Samples(BLOCK))
        .expect("an offline session builds")
}

fn add_probe_on_outputs(s: &mut Session) -> tutti_graph::NodeKey {
    let node = s
        .add_plugin_blocking(
            &main_thread(),
            PluginLoadRequest {
                id: PluginId::from_path(clap_probe()),
                state: None,
            },
        )
        .expect("the reference CLAP plugin loads");
    s.graph().set_output_source(0, GraphSource::Node(node, 0));
    s.update();
    node
}

/// **The graph plans PDC against a plugin's declared latency, and a change
/// of it is the session's phase's.** The node's latency is the plugin's own
/// figure plus the one chunk its out-of-process pipeline holds; the plugin
/// takes MIDI and automation on one event input. Idle updates leave the graph
/// clean (the phase compares the plugin's declared figure with the editor's
/// and, while they agree, edits nothing: no recompile and no republish frame
/// after frame). A latency the plugin changes (what `latency.changed` writes
/// into its cell) reaches the graph through `Phase::Plugins`, with a
/// `PluginLatencyChanged`; a figure past what PDC compensates is clamped, not
/// refused.
///
/// Mutations (each run, each failed): handing the editor the plugin's own
/// figure instead of its declared one (`PluginControls::latency`, which drops
/// the pipeline chunk) fails the first latency assertion; the session's
/// `refresh_node_latency` marking the graph edited before it compares the
/// figures fails the clean-graph assertion; `Phase::Plugins` not calling
/// `refresh_node_latency` leaves the graph at the load-time figure; dropping
/// the clamp in `clamp_latency` makes the editor refuse the figure, and the
/// graph keeps 300 + chunk.
#[test]
fn a_plugins_latency_is_the_sessions_and_follows_the_plugin() {
    let mut s = offline();
    let node = add_probe_on_outputs(&mut s);

    assert_eq!(
        s.graph_ref().node_event_inputs(node),
        1,
        "a plugin takes MIDI (and automation) on its node's event input"
    );
    assert_eq!(
        s.graph_ref().node_latency(node),
        Samples(tutti_clap_test_plugin::REPORTED_LATENCY_SAMPLES as usize + BLOCK),
        "the graph plans PDC against the node's whole latency"
    );

    // Idle updates leave the graph clean: observed between the plugins phase
    // and the commit (which clears the edit flag).
    let now = Instant::now();
    for _ in 0..5 {
        let mut out = Vec::new();
        for &phase in PHASES {
            s.run_phase(phase, PhaseCx::at(now), &mut out);
            if phase == Phase::Plugins {
                assert!(
                    !s.graph_ref().is_edited(),
                    "an unchanged plugin latency marked the graph edited"
                );
            }
        }
        assert!(
            !out.iter()
                .any(|e| matches!(e, SessionEvent::PluginLatencyChanged { .. })),
            "{out:?}"
        );
    }

    let controls = s
        .plugin(node)
        .expect("hosted")
        .controls()
        .expect("out of process")
        .clone();
    controls.set_latency(Samples(300));
    let events = s.update();
    assert_eq!(
        s.graph_ref().node_latency(node),
        Samples(300 + BLOCK),
        "a changed plugin latency reaches the graph"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            SessionEvent::PluginLatencyChanged { node: n, latency }
                if *n == node && *latency == Samples(300 + BLOCK)
        )),
        "{events:?}"
    );

    controls.set_latency(Samples(10_000_000));
    s.update();
    assert_eq!(
        s.graph_ref().node_latency(node),
        tutti_graph::MAX_NODE_LATENCY,
        "clamped to what PDC compensates"
    );
}

/// **A plugin crossfaded in is the one the latency phase reads.** The
/// incoming client is told another latency, so the figure says which client
/// the session's record follows: after `crossfade_plugin` the graph plans
/// against the incoming plugin's figure plus the chunk, and a later change of
/// the incoming plugin's latency is the one the phase hands the graph (its
/// record moved to the incoming plugin). The node keeps its key.
///
/// Mutation (run, failed): `Graph::replace_plugin` not moving its record to
/// the incoming plugin (`HostedPlugin::replace` skipped): the phase keeps
/// reading the outgoing client's cell, and both assertions read the
/// outgoing figure.
#[test]
fn a_crossfaded_plugins_latency_is_the_incoming_ones() {
    const INCOMING: usize = 211;
    let mut s = offline();
    let node = add_probe_on_outputs(&mut s);
    let outgoing = s.graph_ref().node_latency(node);

    let incoming = PluginClient::new(tutti_plugin::BridgeConfig::default(), clap_probe(), RATE)
        .expect("load a second instance of the reference plugin");
    incoming.set_latency(Samples(INCOMING));
    let controls = incoming.controls();
    let landed = s
        .graph()
        .crossfade_plugin(
            node,
            Box::new(incoming),
            tutti_types::Seconds(0.005),
            tutti_graph::CrossfadeCurve::EqualAmplitude,
        )
        .expect("the crossfade is accepted");
    assert!(matches!(landed, Crossfade::Landed(())), "nothing waits");
    s.update();
    s.update();

    assert!(s.graph_ref().contains(node), "the node keeps its key");
    assert_ne!(
        s.graph_ref().node_latency(node),
        outgoing,
        "precondition: the figures differ"
    );
    assert_eq!(
        s.graph_ref().node_latency(node),
        Samples(INCOMING + BLOCK),
        "the graph plans against the incoming plugin's declared latency"
    );
    controls.set_latency(Samples(INCOMING + 7));
    s.update();
    assert_eq!(
        s.graph_ref().node_latency(node),
        Samples(INCOMING + 7 + BLOCK),
        "the latency phase must read the incoming plugin"
    );
}

/// **A restart onto a device with another callback size re-declares a hosted
/// plugin's latency, and PDC follows.** A live plugin ships one device
/// callback per chunk to its server, so its declared latency is its own 137
/// frames plus the callback: 137 + 512 on the first device, 137 + 256 once the
/// restart re-prepares the graph for the second (`OutputSpec::quantum` ->
/// `Prepare::quantum`). The session's total latency (the plugin alone on
/// output 0) is the new figure before the first block.
///
/// Mutation: the session's `rerate` passing `None` for the quantum -> the
/// chunk falls back to the graph's `MaxBlock` (1024) -> 1161, not 393 ->
/// fails. Mutation: `SessionBuilder::build_on` not passing the opened spec's
/// quantum -> 1161 on the first device -> fails.
#[test]
fn a_restart_to_another_callback_size_re_declares_a_plugins_latency() {
    fn spec(quantum: usize) -> OutputSpec {
        OutputSpec::new(
            RATE,
            ChannelLayout::STEREO,
            tutti_cpal::cpal::SampleFormat::F32,
        )
        .with_quantum(Samples(quantum))
    }
    set_server();
    let (driver, stream) = ManualStreamDriver::new();
    let (mut s, mut device) = Session::builder()
        .build_device_on(spec(512), driver)
        .expect("builds with no device");

    let client = PluginClient::new(tutti_plugin::BridgeConfig::default(), clap_probe(), RATE)
        .expect("the reference plugin loads");
    let node = s.graph().insert_plugin(Box::new(client));
    s.graph().set_output_source(0, GraphSource::Node(node, 0));
    s.update();
    let _ = stream.render_block(512).expect("the stream is open");
    s.update();
    assert_eq!(
        s.graph_ref().node_latency(node),
        Samples(137 + 512),
        "one 512-frame callback"
    );
    assert_eq!(s.latency(), s.graph_ref().node_latency(node));

    let (driver, _new) = ManualStreamDriver::new();
    s.restart_on(&mut device, spec(256), driver, None, |_| {})
        .expect("restarts");
    assert_eq!(
        s.graph_ref().node_latency(node),
        Samples(137 + 256),
        "one 256-frame callback"
    );
    assert_eq!(
        s.latency(),
        Samples(137 + 256),
        "PDC follows before the first block"
    );
}
