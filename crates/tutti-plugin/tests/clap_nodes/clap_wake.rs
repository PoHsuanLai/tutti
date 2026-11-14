//! **A CLAP plugin that slept runs again as soon as it asks.** The plugin
//! returns `CLAP_PROCESS_SLEEP` with silent output, so its node reports
//! `Status::Idle` and the executor parks it; a parked node wakes only on input,
//! so what has to wake it is the plugin's own `host.request_process()`
//! (CLAP: "the host must resume calling `process`"), made from a thread that is
//! neither the audio thread nor the main thread.
//!
//! The reference plugin runs in `Woken` render mode: silence until it has made
//! the request, then a constant. In process the first block after the request
//! must call the plugin's `process` once and carry the constant; served, the
//! plugin-server's request reaches the client as `ProcessRequested`, which the
//! node's wake answers once the control tick has read it.
//!
//! Needs `cargo build -p tutti-plugin-server` first.

#![cfg(feature = "clap")]

use crate::clap_probe;
use clap_probe::{BLOCK, PERIOD, ProbeEnv, SAMPLE_RATE, load_probe, render};
use tutti_fixtures::{machine_lock, main_thread};

use std::time::{Duration, Instant};

use tutti_clap_host::ClapLoaded;
use tutti_clap_test_plugin::{ClapProbe, ClapSwitch, StatusMode, WOKEN_LEVEL};
use tutti_graph::{GraphBuilder, Prepare, Renderer};
use tutti_plugin_types::Load;
use tutti_types::{ChannelLayout, MaxBlock, SampleRate, Samples};

/// How long a wake that never comes is waited for.
const PATIENCE: Duration = Duration::from_secs(10);

/// `node` as the only node of a graph with no global input, its outputs the
/// graph's: its inputs are silent, which is what lets the executor park it.
fn one_plugin_graph(node: impl tutti_graph::IntoNode, outputs: usize) -> Renderer {
    let layout = |n: usize| ChannelLayout::from_count(u16::try_from(n).expect("a few ports"));
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, layout(outputs));
    let (key, _controls) = g.add_with_controls(node);
    g.pipe_output(key);
    g.renderer(Prepare::new(
        SampleRate::of(SAMPLE_RATE),
        MaxBlock::of(Samples(BLOCK)),
    ))
    .expect("a one-plugin graph compiles")
}

fn is_woken(out: &[Vec<f32>]) -> bool {
    out.iter().flatten().any(|&s| s == WOKEN_LEVEL)
}

/// **In process, the block after a `request_process` from another thread calls
/// the plugin, and no block before it did.** The plugin sleeps on every block;
/// once its node is parked its `process` count stops. A request from a thread
/// of the test then makes the very next block call `process` once and carry the
/// plugin's sound.
#[test]
fn in_process_the_next_block_after_a_request_from_another_thread_runs_the_plugin() {
    let _lock = machine_lock();
    let probe = ClapProbe::open(
        &tutti_fixtures::cdylib("tutti_clap_test_plugin", "tutti-clap-test-plugin"),
        tutti_fixtures::clap_probe(),
    );
    probe.set(ClapSwitch::StatusMode, StatusMode::Sleep as u32);
    probe.set(ClapSwitch::RenderMode, render::WOKEN);
    // A plugin that sends MIDI out is never parked in process (its block has
    // events to send), so the one that sleeps declares none.
    probe.set(ClapSwitch::NoteOutPorts, 0);

    let (client, _handle) = tutti_plugin_host::load_client::<ClapLoaded>(
        main_thread(),
        &tutti_fixtures::detached_work(),
        tutti_fixtures::clap_probe(),
        &Load::new(),
        SampleRate::of(SAMPLE_RATE),
    )
    .expect("the reference plugin loads in process");
    let node = client.bind(
        tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0)).expect("a tempo"),
    );
    let mut renderer = one_plugin_graph(node, 2);

    for _ in 0..8 {
        assert!(!is_woken(&renderer.render(BLOCK)), "silent until it asks");
    }
    let parked_at = probe.blocks_seen();
    assert!(parked_at > 0, "the plugin ran before it slept");
    for _ in 0..8 {
        renderer.render(BLOCK);
    }
    assert_eq!(
        probe.blocks_seen(),
        parked_at,
        "the node is parked: the plugin's process is not called"
    );

    let asked = std::thread::scope(|s| s.spawn(|| probe.request_process_now()).join());
    assert!(asked.expect("the thread returns"), "the plugin asked");

    let out = renderer.render(BLOCK);
    assert_eq!(
        probe.blocks_seen(),
        parked_at + 1,
        "the first block after the request called process once"
    );
    assert!(is_woken(&out), "and carries the plugin's sound");
}

/// How many blocks after the request is visible the woken sound may take to
/// come back: the wake, the chunk's trip to the server and its answer's trip
/// back. About 20 ms of audio, under the server's old 33 ms tick alone.
const WOKEN_WITHIN_BLOCKS: usize = 15;

/// **Served, a `request_process` the plugin makes on a thread of its own while
/// its node is parked wakes the node within a few blocks.** No tick is driven:
/// the server's main loop sends `ProcessRequested` the moment the plugin asks (the
/// plugin's call rings the loop's bell), the bridge's watcher thread reads it the
/// moment it arrives and raises the node's wake, and the plugin's sound comes
/// back through the pipeline.
#[test]
fn served_a_request_while_parked_wakes_the_node_within_a_few_blocks() {
    let _lock = machine_lock();
    let dir = tempfile::tempdir().expect("a scratch directory");
    let request = dir.path().join("request_process");
    let _env = ProbeEnv::new()
        .render_mode(render::WOKEN)
        .status_mode(StatusMode::Sleep as u32)
        .request_process_file(&request);
    let probe = load_probe(SAMPLE_RATE);
    let outputs = probe.client.outputs();
    let mut renderer = one_plugin_graph(
        probe.client.bind(
            tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0)).expect("a tempo"),
        ),
        outputs,
    );
    let step = |renderer: &mut Renderer| {
        let out = renderer.render(BLOCK);
        std::thread::sleep(PERIOD);
        out
    };

    for _ in 0..100 {
        assert!(!is_woken(&step(&mut renderer)), "silent until it asks");
    }
    std::fs::write(&request, b"").expect("the plugin's trigger");

    // The plugin deletes the file after it has made the request.
    let deadline = Instant::now() + PATIENCE;
    while request.exists() {
        assert!(
            Instant::now() < deadline,
            "the plugin never made the request"
        );
        if is_woken(&step(&mut renderer)) {
            return;
        }
    }
    let asked = Instant::now();
    for blocks in 1..=WOKEN_WITHIN_BLOCKS {
        if is_woken(&step(&mut renderer)) {
            eprintln!(
                "woken {blocks} blocks, {:?} after the request was visible",
                asked.elapsed()
            );
            return;
        }
    }
    panic!(
        "the plugin asked to be processed and the parked node did not run within {WOKEN_WITHIN_BLOCKS} blocks"
    );
}
