//! An offline fork whose plugin answers a rate change **late**, against a
//! real `plugin-server`.
//!
//! The bridge gives up waiting for a rate change after its rate timeout and
//! reports the rate refused ("did not answer"), but the plugin may still be
//! re-activating, and an idle bridge reads nothing, so its answer would sit
//! unread and `Batcher::await_output` would latch the stale refusal. A fork
//! does not reach that state: its `prepare` calls `PluginBridge::settle`, a
//! command behind the rate change, and the bridge reads the late answer ahead
//! of that command's reply. This pins it.

use std::time::Duration;
use tutti_types::MaxBlock;

use tutti_graph::{ForkMode, GraphBuilder, Prepare, Renderer};
use tutti_types::{ChannelLayout, SampleRate, Samples};

use crate::handles::PluginClient;
use tutti_fixtures::machine_lock;
use tutti_plugin_ipc::BridgeConfig;

const LOAD_RATE: f64 = 48_000.0;
const SLOW_RATE: f64 = 96_000.0;
/// How long the probe's `activate` at `SLOW_RATE` takes.
const ACTIVATE_MS: u32 = 2_500;
/// What the bridge waits for the rate change's answer: well under `ACTIVATE_MS`.
const RATE_TIMEOUT: Duration = Duration::from_millis(400);
const BLOCK: usize = 64;
const INPUT_DC: f32 = 7.0;
/// `TagPassthrough` adds the probe's tag (1.0) to channel 0.
const LIVE: f32 = INPUT_DC + 1.0;

/// **A plugin that answers a rate change after the bridge gave up on it
/// still renders an offline fork, and the fork is healthy.**
///
/// The fork is prepared at a rate whose `activate` takes 2.5 s; the bridge
/// waits 400 ms and reports the rate refused; the render starts after that,
/// with the refusal standing and the answer unread. Every block must be live
/// and the fork must report no fault.
#[test]
fn a_late_rate_answer_does_not_silence_an_offline_fork() {
    let _lock = machine_lock();
    let mut env = tutti_fixtures::ScopedEnv::new();
    env.set(
        "TUTTI_PLUGIN_SERVER",
        tutti_fixtures::plugin_server()
            .to_string_lossy()
            .into_owned(),
    );
    env.set("TUTTI_CLAP_PROBE_RENDER_MODE", "1");
    let live = PluginClient::new(
        BridgeConfig::default(),
        &crate::catalog::PluginRef::file(tutti_fixtures::clap_probe()).expect("a plugin file"),
        SampleRate::new(LOAD_RATE),
    )
    .expect("load the reference CLAP plugin through a real plugin-server");
    // Armed after the live server started: only the fork's server is slow.
    env.set(
        "TUTTI_CLAP_PROBE_SLOW_ACTIVATE",
        format!("{SLOW_RATE}:{ACTIVATE_MS}"),
    );
    let fork = live
        .fork_instance(
            ForkMode::Offline,
            tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0)).expect("a tempo"),
        )
        .expect("the probe forks");
    let bridge = fork.bridge();
    bridge.set_rate_timeout(RATE_TIMEOUT);
    let health = fork.fork_health().expect("a fork has a health probe");

    let layout = |n: usize| ChannelLayout::from_count(u16::try_from(n).expect("a few ports"));
    let mut g = GraphBuilder::new(layout(fork.inputs()), layout(fork.outputs()));
    let (key, _controls) = g.add_with_controls(fork);
    g.pipe_input(key).pipe_output(key);
    let mut renderer: Renderer = g
        .renderer(Prepare::new(
            SampleRate(SLOW_RATE),
            MaxBlock::of(Samples(BLOCK)),
        ))
        .expect("a one-plugin graph compiles");

    // The bridge gives up on the rate change, then the render starts.
    let started = std::time::Instant::now();
    while started.elapsed() < RATE_TIMEOUT * 2 {
        std::thread::sleep(Duration::from_millis(50));
    }

    assert_eq!(
        bridge.rate_refusal(),
        None,
        "prepare settled the rate change, late answer included"
    );
    let dc = [INPUT_DC; BLOCK];
    let inputs = renderer.editor().spec().topology.inputs.count() as usize;
    let mut blocks = Vec::new();
    for _ in 0..8 {
        let chans: Vec<&[f32]> = (0..inputs).map(|_| &dc[..]).collect();
        let out = renderer.render_input(&chans);
        blocks.push(out.into_iter().next().expect("the probe has outputs"));
    }
    // The first block is the pipeline's one block of latency.
    for (i, block) in blocks.iter().enumerate().skip(1) {
        assert!(
            block.iter().all(|&s| s == LIVE),
            "block {i} is not the plugin's: {:?}",
            &block[..4]
        );
    }
    assert!(
        health.fault().is_none(),
        "a late answer is not a refusal: {:?}",
        health.fault().map(|(k, c)| (k, c.to_string()))
    );
}
