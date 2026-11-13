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

use crate::support;

use std::time::Instant;
use tutti_types::MaxBlock;

use support::{clap_probe, plugin_server};
use tutti_cpal::{ManualStreamDriver, OutputSpec};
use tutti_plugin::catalog::PluginRef;
use tutti_plugin::handles::PluginClient;
use tutti_session::{
    Bounce, BounceLatency, BounceTail, BufferMaster, Crossfade, ExportClock, OutPort, PHASES,
    Phase, PhaseCx, PluginLoadRequest, Range, Root, Session, SessionEvent,
};
use tutti_session::{PluginChange, PluginEvent};
use tutti_types::{ChannelLayout, SampleRate, Samples};

const RATE: SampleRate = SampleRate::SR_48K;
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
        .build_offline(RATE, ChannelLayout::STEREO, MaxBlock::of(Samples(BLOCK)))
        .expect("an offline session builds")
}

fn add_probe_on_outputs(s: &mut Session) -> tutti_graph::NodeKey {
    let node = s
        .add_plugin_blocking(
            &main_thread(),
            PluginLoadRequest::new(PluginRef::file(clap_probe()).expect("a plugin file")),
        )
        .expect("the reference CLAP plugin loads");
    s.graph()
        .set_sources(Root::output(0), [OutPort::new(node, 0)])
        .unwrap();
    let _ = s.update();
    node.erase()
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
#[test]
fn a_plugins_latency_is_the_sessions_and_follows_the_plugin() {
    let mut s = offline();
    let node = add_probe_on_outputs(&mut s);

    assert_eq!(
        s.graph_ref().node_event_inputs(node).unwrap(),
        1,
        "a plugin takes MIDI (and automation) on its node's event input"
    );
    assert_eq!(
        s.graph_ref().node_latency(node).unwrap(),
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
            !out.iter().any(|e| matches!(
                e,
                SessionEvent::Plugin(PluginEvent {
                    change: PluginChange::LatencyChanged { .. },
                    ..
                })
            )),
            "{out:?}"
        );
    }
}

/// **The graph holds the tail ceiling the plugin claims** (`clap.tail`), apart
/// from its latency (doc 023 step 2): the node's shape declares it through
/// `Processing::ceiling`, an unmeasured plugin's claim, and the session's
/// phase leaves it alone while the plugin's figure stands.
#[test]
fn a_plugins_tail_ceiling_is_the_plugins_claim() {
    let mut s = offline();
    let node = add_probe_on_outputs(&mut s);
    assert_eq!(
        s.graph_ref().node_tail(node).unwrap(),
        tutti_types::Tail::Finite(Samples(
            tutti_clap_test_plugin::REPORTED_TAIL_SAMPLES as usize
        )),
    );
    let now = Instant::now();
    let mut out = Vec::new();
    for &phase in PHASES {
        s.run_phase(phase, PhaseCx::at(now), &mut out);
        if phase == Phase::Plugins {
            assert!(
                !s.graph_ref().is_edited(),
                "an unchanged plugin tail marked the graph edited"
            );
        }
    }
}

/// A short offline bounce into memory: the render mode a plugin is told
/// Offline for as long as it is in flight.
fn request() -> Bounce<tutti_graph::NodeKey, BufferMaster> {
    Bounce::project()
        .master_buffers()
        .channels(ChannelLayout::STEREO)
        .range(Range::Seconds(
            tutti_types::Seconds(0.0),
            tutti_types::Seconds(0.05),
        ))
        .sample_rate(RATE)
        .clock(ExportClock::frozen())
        .latency(BounceLatency::Keep)
        .tail(BounceTail::None)
}

/// **A latency the plugin reports reaches the graph through the session's
/// phase.** The reference plugin adds `TUTTI_CLAP_PROBE_OFFLINE_EXTRA_LATENCY`
/// frames to its latency while it renders offline and says so (`latency.changed`):
/// a bounce in flight is that report. It is `diagnostics().reported_latency()` at
/// once, and the graph's (`latency()`, the report plus the pipeline's chunk) at
/// the next update, with a `LatencyChanged`.
#[test]
fn a_latency_the_plugin_reports_reaches_the_graph() {
    // SAFETY: as `set_server`: before the first server, from the test's thread.
    unsafe { std::env::set_var("TUTTI_CLAP_PROBE_OFFLINE_EXTRA_LATENCY", "300") };
    let mut s = offline();
    let node = add_probe_on_outputs(&mut s);
    let own = tutti_clap_test_plugin::REPORTED_LATENCY_SAMPLES as usize;
    assert_eq!(diagnostics_of(&s, node).reported_latency(), Samples(own));
    assert_eq!(diagnostics_of(&s, node).latency(), Samples(own + BLOCK));

    let _job = s.bounce_start(request()).expect("the bounce starts");
    let deadline = Instant::now() + std::time::Duration::from_secs(10);
    while diagnostics_of(&s, node).reported_latency() != Samples(own + 300) {
        assert!(Instant::now() < deadline, "the report never arrived");
        round_trip(&s, node);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(
        diagnostics_of(&s, node).latency(),
        Samples(own + BLOCK),
        "the graph has not taken it yet"
    );

    let events = s.update();
    assert_eq!(
        s.graph_ref().node_latency(node).unwrap(),
        Samples(own + 300 + BLOCK),
        "a changed plugin latency reaches the graph"
    );
    assert_eq!(
        diagnostics_of(&s, node).latency(),
        Samples(own + 300 + BLOCK)
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            SessionEvent::Plugin(PluginEvent { node: n, change: PluginChange::LatencyChanged { latency }, .. })
                if *n == node && *latency == Samples(own + 300 + BLOCK)
        )),
        "{events:?}"
    );
}

/// **A plugin crossfaded in is the one the latency phase reads.** The
/// incoming client is told another latency, so the figure says which client
/// the session's record follows: after `crossfade_plugin` the graph plans
/// against the incoming plugin's figure plus the chunk, and a later change of
/// the incoming plugin's latency is the one the phase hands the graph (its
/// record moved to the incoming plugin). The node keeps its key.
#[test]
fn a_crossfaded_plugins_latency_is_the_incoming_ones() {
    const INCOMING: usize = 211;
    let mut s = offline();
    let node = add_probe_on_outputs(&mut s);
    let outgoing = s.graph_ref().node_latency(node).unwrap();

    let incoming = PluginClient::new(
        tutti_plugin::BridgeConfig::default(),
        &PluginRef::file(clap_probe()).expect("a plugin file"),
        RATE,
    )
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
    let _ = s.update();
    let _ = s.update();

    assert!(s.graph_ref().contains(node), "the node keeps its key");
    assert_ne!(
        s.graph_ref().node_latency(node).unwrap(),
        outgoing,
        "precondition: the figures differ"
    );
    assert_eq!(
        s.graph_ref().node_latency(node).unwrap(),
        Samples(INCOMING + BLOCK),
        "the graph plans against the incoming plugin's declared latency"
    );
    controls.set_latency(Samples(INCOMING + 7));
    let _ = s.update();
    assert_eq!(
        s.graph_ref().node_latency(node).unwrap(),
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
#[test]
fn a_restart_to_another_callback_size_re_declares_a_plugins_latency() {
    fn spec(quantum: usize) -> OutputSpec {
        OutputSpec::new(
            RATE,
            ChannelLayout::STEREO,
            tutti_cpal::cpal::SampleFormat::F32,
        )
        .with_quantum(MaxBlock::of(Samples(quantum)))
    }
    set_server();
    let (driver, stream) = ManualStreamDriver::new();
    let (mut s, mut device) = Session::builder()
        .build_device_on(spec(512), driver)
        .expect("builds with no device")
        .into_parts();

    let client = PluginClient::new(
        tutti_plugin::BridgeConfig::default(),
        &PluginRef::file(clap_probe()).expect("a plugin file"),
        RATE,
    )
    .expect("the reference plugin loads");
    let node = s.graph().insert_plugin(Box::new(client)).erase();
    s.graph()
        .set_sources(Root::output(0), [OutPort::new(node, 0)])
        .unwrap();
    let _ = s.update();
    let _ = stream.render_block(512).expect("the stream is open");
    let _ = s.update();
    assert_eq!(
        s.graph_ref().node_latency(node).unwrap(),
        Samples(137 + 512),
        "one 512-frame callback"
    );
    assert_eq!(s.latency(), s.graph_ref().node_latency(node).unwrap());

    let (driver, _new) = ManualStreamDriver::new();
    s.restart_on(&mut device, spec(256), driver, None)
        .expect("restarts");
    assert_eq!(
        s.graph_ref().node_latency(node).unwrap(),
        Samples(137 + 256),
        "one 256-frame callback"
    );
    assert_eq!(
        s.latency(),
        Samples(137 + 256),
        "PDC follows before the first block"
    );
}

fn diagnostics_of(s: &Session, node: tutti_graph::NodeKey) -> tutti_session::Diagnostics<'_> {
    s.plugin(node).expect("hosted").diagnostics()
}

/// One control round trip to the probe's server: the server sends the
/// `latency.changed` of a render mode after that call's reply, and the bridge
/// thread reads it off the socket only while it waits for the next reply.
fn round_trip(s: &Session, node: tutti_graph::NodeKey) {
    let _ = s.plugin(node).expect("hosted").state().save();
}
