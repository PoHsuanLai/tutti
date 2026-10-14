//! Parameter delivery to a hosted plugin, end to end: a `PluginAutomation`
//! node on the plugin node's event input, the node's per-parameter queues,
//! the bridge, a real `plugin-server` and a real format host (the reference
//! VST2 plugin, which takes its parameter count from the environment), and
//! the values read back from the plugin.
//!
//! What it pins: a chunk carries a change for every parameter (no 16-queue
//! cap), the node sends changes only, and an unchanged host value does not
//! overwrite an edit made on the plugin's side, while a moving one does.
//!
//! Needs `cargo build -p tutti-plugin-server` first (see `CLAUDE.md`).

#![cfg(feature = "clap")]

#[path = "support/clap_probe.rs"]
mod clap_probe;
#[path = "support/param_probe.rs"]
mod param_probe;
#[path = "support/probe_path.rs"]
mod probe_path;

use std::time::Duration;

use clap_probe::{exclusive, Rig};
use param_probe::{knobs, load_vst2_probe, until};
use tutti_graph::Prepare;
use tutti_graph::Transport;
use tutti_plugin::handles::{ParamAddress, PluginHandle};
use tutti_types::{Beat, Bpm, SampleRate, Samples};

const SAMPLE_RATE: f64 = 48_000.0;
const BLOCK: usize = 256;
/// Far more parameters than the 16 queues a chunk used to hold.
const PARAMS: usize = 200;
/// How long a value may take to cross the bridge and the server.
const SETTLE: Duration = Duration::from_secs(10);

/// A distinct value per parameter, exact in `f32` (eighths), never the
/// probe's default of 0.
fn value(i: usize) -> f32 {
    (1 + i % 7) as f32 / 8.0
}

/// The plugin's value for parameter `i`, read from the plugin-server.
fn read(handle: &PluginHandle, i: usize) -> Option<f32> {
    handle
        .params()
        .parameter_value(ParamAddress::Index(i32::try_from(i).expect("small")))
}

/// A rolling graph with the plugin fed by `automation`.
fn rig(
    client: tutti_plugin::handles::PluginClient,
    automation: tutti_plugin::handles::PluginAutomation,
) -> Rig {
    let prepare = Prepare::new(SampleRate(SAMPLE_RATE), Samples(BLOCK));
    let mut rig = Rig::prepared_with(client.bind(), prepare, Some(automation));
    rig.renderer()
        .set_transport(Transport::new(true, Bpm(120.0), Beat(0.0), None));
    rig
}

/// **Two hundred parameters moving in the same block all reach the plugin**,
/// each with its own value, and the node counts one point per parameter.
///
/// Mutation (run): the node's chunk sized at 16 queues
/// (`ParameterChanges::reserved(16, self.points)` in
/// `ParamDelivery::prepare`) → the parameters past the 16th are refused in
/// the first chunks, counted as dropped → fails. (They do arrive later: each
/// refused value is still unsent, so a later chunk with room takes it.)
#[test]
fn two_hundred_parameters_changing_in_one_block_all_arrive() {
    let _lock = exclusive();
    let (client, handle) = load_vst2_probe(PARAMS, SAMPLE_RATE);
    let (_knobs, params) = knobs(PARAMS, value);
    let automation = client.automation(params);
    let mut rig = rig(client, automation);

    let arrived = until(
        SETTLE,
        || {
            rig.run(BLOCK, |_, _| 0.0);
        },
        || (0..PARAMS).all(|i| read(&handle, i) == Some(value(i))),
    );
    let missing: Vec<usize> = (0..PARAMS)
        .filter(|&i| read(&handle, i) != Some(value(i)))
        .collect();
    assert!(arrived, "parameters that never arrived: {missing:?}");
    let stats = rig.controls.param_delivery();
    assert_eq!(stats.points_dropped, 0, "{stats:?}");
}

/// **An unchanged value is sent once.** Two hundred parameters held still
/// for many blocks: the automation node samples every lane at every stride
/// point of every block, and the node passes on one point per parameter in
/// all, then nothing, however many more blocks run. A value that moves is
/// one more point.
///
/// Mutation (run): no dedupe against the delivered value (`slot.sent ==
/// Some(value)` replaced by `false` in `ParamDelivery::place`) → every
/// chunk resends all two hundred → the count keeps rising → fails.
#[test]
fn an_unchanged_value_is_not_sent_again() {
    let _lock = exclusive();
    let (client, handle) = load_vst2_probe(PARAMS, SAMPLE_RATE);
    let (knobs, params) = knobs(PARAMS, value);
    let automation = client.automation(params);
    let mut rig = rig(client, automation);

    assert!(until(
        SETTLE,
        || {
            rig.run(BLOCK, |_, _| 0.0);
        },
        || (0..PARAMS).all(|i| read(&handle, i) == Some(value(i))),
    ));
    let settled = rig.controls.param_delivery().points_sent;
    assert_eq!(settled, PARAMS as u64, "one point per parameter");
    for _ in 0..50 {
        rig.run(BLOCK, |_, _| 0.0);
    }
    assert_eq!(
        rig.controls.param_delivery().points_sent,
        settled,
        "a value held still was sent again"
    );

    knobs[3].set(0.875);
    assert!(until(
        SETTLE,
        || {
            rig.run(BLOCK, |_, _| 0.0);
        },
        || read(&handle, 3) == Some(0.875),
    ));
    assert_eq!(rig.controls.param_delivery().points_sent, settled + 1);
}

/// **An edit made on the plugin's side survives a host value that holds
/// still, and a host value that moves replaces it.** The host automates a
/// parameter to 0.25; the plugin's own value is then set to 0.75 behind the
/// host's back (through the plugin handle, which reaches the plugin as an
/// edit in its editor would, without passing the node); the host keeps
/// sending 0.25 for many blocks and the plugin keeps 0.75; then the host
/// moves to 0.5 and the plugin takes it.
///
/// Mutations (run), each fails: the node forgetting what it sent
/// (`slot.sent = None` for `Some(slot.staged)` in `settle`) → the next
/// chunk resends 0.25 over the edit; the node never sending a parameter
/// again once it has sent one (`place` returning when `slot.sent.is_some()`)
/// → 0.5 never arrives.
#[test]
fn an_edit_on_the_plugin_survives_a_held_host_value() {
    let _lock = exclusive();
    let (client, handle) = load_vst2_probe(8, SAMPLE_RATE);
    let (knobs, params) = knobs(8, |_| 0.25);
    let automation = client.automation(params);
    let mut rig = rig(client, automation);
    let p = 2;

    assert!(until(
        SETTLE,
        || {
            rig.run(BLOCK, |_, _| 0.0);
        },
        || read(&handle, p) == Some(0.25),
    ));
    handle.params().set_parameter_value(
        ParamAddress::Index(p as i32),
        tutti_plugin::handles::Normalized::new(0.75),
    );
    assert!(
        until(SETTLE, || {}, || read(&handle, p) == Some(0.75)),
        "the edit reached the plugin"
    );
    for _ in 0..100 {
        rig.run(BLOCK, |_, _| 0.0);
    }
    // Long enough for any resend to have crossed the server: a block takes
    // a few milliseconds there.
    std::thread::sleep(Duration::from_millis(200));
    rig.run(BLOCK, |_, _| 0.0);
    assert_eq!(
        read(&handle, p),
        Some(0.75),
        "a held host value overwrote the plugin-side edit"
    );

    knobs[p].set(0.5);
    assert!(
        until(
            SETTLE,
            || {
                rig.run(BLOCK, |_, _| 0.0);
            },
            || read(&handle, p) == Some(0.5),
        ),
        "a moving host value did not replace the edit"
    );
}
