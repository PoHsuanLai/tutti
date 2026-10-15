//! Parameter delivery to a hosted plugin, end to end: foreign control lanes
//! (`ControlLaneNode::for_foreign`, one per parameter) on the plugin node's
//! event input, the node's per-parameter queues,
//! the bridge, a real `plugin-server` and a real format host (the reference
//! VST2 plugin, which takes its parameter count from the environment), and
//! the values read back from the plugin.
//!
//! What it pins: a chunk carries a change for every parameter (no 16-queue
//! cap), the node sends changes only, an unchanged host value does not
//! overwrite an edit made on the plugin's side while a moving one does, and
//! an export (a fork) renders that edit as live playback does (the
//! reference CLAP plugin, whose gain parameter is audible).
//!
//! Needs `cargo build -p tutti-plugin-server` first (see `CLAUDE.md`).

#![cfg(feature = "clap")]

#[path = "support/clap_probe.rs"]
mod clap_probe;
#[path = "support/param_probe.rs"]
mod param_probe;
#[path = "support/probe_path.rs"]
mod probe_path;

use std::sync::Arc;
use std::time::Duration;

use clap_probe::{exclusive, load_probe, render, ProbeEnv, Rig};
use param_probe::{knob, knobs, load_vst2_probe, until};
use tutti_core::transport::{OfflineTimeline, OfflineTimelineConfig, OfflineTransport};
use tutti_graph::{ForkMode, ForkTarget, Prepare, Transport};
use tutti_nodes::automation::{ControlLaneNode, LaneControls};
use tutti_plugin::handles::{ParamAddress, ParamId, PluginHandle};
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

/// A rolling graph with the plugin fed by `lanes`.
fn rig(client: tutti_plugin::handles::PluginClient, lanes: Vec<ControlLaneNode>) -> Rig {
    let prepare = Prepare::new(SampleRate(SAMPLE_RATE), Samples(BLOCK));
    let mut rig = Rig::prepared_with(client.bind(), prepare, lanes);
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
    let (_knobs, lanes) = knobs(PARAMS, value);
    let mut rig = rig(client, lanes);

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
/// for many blocks, every lane asked to re-send its value before every
/// block (`resync`, as a host does after re-wiring one, and as a fork's
/// fresh lane sends): the node passes on one point per parameter in all,
/// then nothing, however many more blocks run. A value that moves is one
/// more point.
///
/// Ported in doc 014 PR 5: the source was the deleted `PluginAutomation`
/// node, which sampled every parameter at every stride point of every
/// block; a lane sends only when something moves, so the re-sends are asked
/// for, to keep the node's own dedupe under test.
///
/// Mutation (run): no dedupe against the delivered value (`slot.sent ==
/// Some(value)` replaced by `false` in `ParamDelivery::place`) → every
/// chunk resends all two hundred → the count keeps rising → fails.
#[test]
fn an_unchanged_value_is_not_sent_again() {
    let _lock = exclusive();
    let (client, handle) = load_vst2_probe(PARAMS, SAMPLE_RATE);
    let (knobs, lanes) = knobs(PARAMS, value);
    let mut rig = rig(client, lanes);
    let resend = |knobs: &[LaneControls]| knobs.iter().for_each(LaneControls::resync);

    assert!(until(
        SETTLE,
        || {
            resend(&knobs);
            rig.run(BLOCK, |_, _| 0.0);
        },
        || (0..PARAMS).all(|i| read(&handle, i) == Some(value(i))),
    ));
    let settled = rig.controls.param_delivery().points_sent;
    assert_eq!(settled, PARAMS as u64, "one point per parameter");
    for _ in 0..50 {
        resend(&knobs);
        rig.run(BLOCK, |_, _| 0.0);
    }
    assert_eq!(
        rig.controls.param_delivery().points_sent,
        settled,
        "a value held still was sent again"
    );

    assert!(knobs[3].set_knob(0.875));
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
/// sending 0.25 for many blocks (its lane re-sent before every block,
/// `resync`) and the plugin keeps 0.75; then the host moves to 0.5 and the
/// plugin takes it.
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
    let (knobs, lanes) = knobs(8, |_| 0.25);
    let mut rig = rig(client, lanes);
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
        knobs[p].resync();
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

    assert!(knobs[p].set_knob(0.5));
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

/// The reference CLAP plugin's gain parameter, and its plain range in dB.
const GAIN: ParamAddress = ParamAddress::Opaque(ParamId::new(77));
/// [`GAIN`]'s number, as a lane carries it.
const GAIN_ID: u32 = 77;
const GAIN_DB: (f64, f64) = (-60.0, 12.0);

/// The normalized gain for `db` (exact for whole dB: 72ths).
fn gain_at(db: f64) -> f32 {
    ((db - GAIN_DB.0) / (GAIN_DB.1 - GAIN_DB.0)) as f32
}

/// The probe's tag (1.0 on channel 0) at `db`.
fn amplitude(db: f64) -> f32 {
    10f64.powf(db / 20.0) as f32
}

/// **An export renders an edit made in the plugin's own editor, as live
/// playback does.** The host holds the gain at -6 dB (an automation lane
/// that never moves); the plugin's gain is then set to -15 dB behind the
/// node's back, through the plugin handle, as an edit in its editor would
/// be. Live, the held -6 dB is never re-sent, so the plugin plays -15 dB.
/// The graph is then forked offline (an export): the fork is loaded with
/// the live state (-15 dB) and its delivery is seeded with what the live
/// node had delivered (-6 dB), so the fork's fresh lane, which sends its
/// -6 dB on its first block (doc 014 §10's fork requirement), is not
/// passed on, and the export plays -15 dB too.
///
/// Mutation (run): the fork's delivery not seeded (no
/// `fork.state.params.seed(..)` in `fork.rs`) → the fork sends the lane's
/// -6 dB over the loaded -15 dB → the export plays -6 dB → fails.
#[test]
fn an_export_renders_an_edit_made_in_the_plugin_as_live_does() {
    const FORK_BLOCK: usize = 256;
    let _lock = exclusive();
    let _env = ProbeEnv::new()
        .render_mode(render::TAG_ONLY)
        .gain_enabled(true);
    let probe = load_probe(SAMPLE_RATE);
    let held = knob(GAIN_ID, gain_at(-6.0));
    let prepare = Prepare::new(SampleRate(SAMPLE_RATE), Samples(FORK_BLOCK));
    let mut live = Rig::prepared_with(probe.client.bind(), prepare, vec![held]);
    live.renderer()
        .set_transport(Transport::new(true, Bpm(120.0), Beat(0.0), None));
    let near = |out: &[Vec<f32>], db: f64| {
        out.first()
            .and_then(|c| c.last())
            .is_some_and(|&s| (s - amplitude(db)).abs() < 1e-4)
    };
    let last = std::cell::RefCell::new(Vec::new());
    assert!(
        until(
            SETTLE,
            || *last.borrow_mut() = live.run(FORK_BLOCK, |_, _| 0.0),
            || near(&last.borrow(), -6.0)
        ),
        "the held host value reached the plugin"
    );

    // The edit, behind the node's back.
    probe.handle.params().set_parameter_value(
        GAIN,
        tutti_plugin::handles::Normalized::new(f64::from(gain_at(-15.0))),
    );
    assert!(
        until(
            SETTLE,
            || *last.borrow_mut() = live.run(FORK_BLOCK, |_, _| 0.0),
            || near(&last.borrow(), -15.0)
        ),
        "the edit reached the plugin"
    );
    for _ in 0..20 {
        *last.borrow_mut() = live.run(FORK_BLOCK, |_, _| 0.0);
        std::thread::sleep(Duration::from_millis(5));
    }
    let last = last.into_inner();
    assert!(
        near(&last, -15.0),
        "live keeps the edit: {:?}",
        last[0].last()
    );

    // The export.
    let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        sample_rate: SampleRate(SAMPLE_RATE),
        ..Default::default()
    }));
    let offline = OfflineTransport::new(Arc::clone(&timeline));
    let (_fork_ed, mut fork_exec) = live
        .renderer()
        .editor()
        .fork(ForkTarget::Master, ForkMode::Offline(&offline), prepare)
        .expect("the lane and the plugin fork");
    let silence = vec![0.0f32; FORK_BLOCK];
    let inputs: Vec<&[f32]> = (0..live.inputs()).map(|_| &silence[..]).collect();
    let mut out = vec![vec![0.0f32; FORK_BLOCK]; live.outputs()];
    for _ in 0..8 {
        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[..]).collect();
        timeline.render_graph(&mut fork_exec, FORK_BLOCK, &inputs, &mut outs);
    }
    assert!(
        near(&out, -15.0),
        "the export plays {:?}, not the -15 dB live playback plays",
        out[0].last()
    );
    drop(probe.handle);
}
