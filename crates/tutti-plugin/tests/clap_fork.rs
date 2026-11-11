//! Forking a hosted plugin **by state transfer**, against the reference CLAP
//! plugin in a real `plugin-server`.
//!
//! What a fork promises (`host::node::fork`'s module docs): a fresh instance of
//! the same plugin, carrying the live instance's saved state, in a process of
//! its own, rendering deterministically offline — and the live instance is only
//! asked for its state. Each test below pins one of those against the probe's
//! applied `Gain` parameter, which is the observable that separates a parameter
//! the plugin *holds* from one a host merely remembers: it is only visible in
//! the samples.
//!
//! The probe runs in `TagPassthrough` with gain applied, so output channel 0 of
//! block `k` is `(input_{k-1} + 1.0) · gain` — one block late, the declared
//! pipeline latency. Inputs vary per block and per sample so that a block
//! delivered to the wrong place, or twice, cannot pass.
//!
//! Needs `cargo build -p tutti-plugin-server` first (see `CLAUDE.md`).

#![cfg(feature = "clap")]

#[path = "support/clap_probe.rs"]
mod clap_probe;
// The live side is paced to `PACE`; the fork is *not*: it waits for each
// block, which is the property under test.
use clap_probe::{BLOCK, PACE, ProbeEnv, Rig, SAMPLE_RATE, load_probe, load_probe_with, render};
use tutti_fixtures::machine_lock;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use tutti_graph::NodeKey;
use tutti_graph::{
    Editor, ForkError, ForkFaultKind, ForkMode, ForkTarget, IntoNode, Prepare, Renderer,
};
use tutti_graph::{OutPort, Source};
use tutti_plugin::handles::PluginClient;
use tutti_plugin::{BridgeConfig, PluginRenderFault};
use tutti_plugin_host::PluginHandle;
use tutti_plugin_types::PluginForkError;
use tutti_plugin_types::{Normalized, ParamId};
use tutti_transport::{OfflineTimeline, OfflineTimelineConfig};
use tutti_types::{SampleRate, Samples};

/// The probe's applied gain (`GAIN_PARAM_ID` in the plugin), and its range in
/// dB. Mirrored rather than imported: this crate links no probe rlib.
const GAIN: ParamId = ParamId::new(77);
const GAIN_DB_MIN: f64 = -60.0;
const GAIN_DB_MAX: f64 = 12.0;

/// `probe_tag(0, 0)`, added to channel 0 in `TagPassthrough`.
const TAG_P0C0: f32 = 1.0;

/// The normalized value that lands on `db`.
fn gain_at(db: f64) -> Normalized {
    Normalized::clamped((db - GAIN_DB_MIN) / (GAIN_DB_MAX - GAIN_DB_MIN))
}

/// The linear amplitude the probe multiplies by at `db`, computed as the probe
/// does (`10^(dB/20)` in f64, then to f32).
fn amplitude(db: f64) -> f32 {
    10f64.powf(db / 20.0) as f32
}

/// Input sample `i` of block `b`: distinct everywhere, small, exact in f32.
fn input(b: usize, i: usize) -> f32 {
    ((b * BLOCK + i) % 997) as f32 / 1024.0
}

/// What block `b` of channel 0 must read at `db`: block `b - 1`'s input,
/// tagged and scaled.
fn expected(b: usize, db: f64) -> Vec<f32> {
    (0..BLOCK)
        .map(|i| (input(b - 1, i) + TAG_P0C0) * amplitude(db))
        .collect()
}

/// Drive block `b` through `unit` and return output channel 0.
fn drive(unit: &mut Rig, b: usize) -> Vec<f32> {
    let out = unit.run(BLOCK, |_, i| input(b, i));
    out.into_iter().next().expect("the probe has outputs")
}

/// Drive `blocks` paced blocks through the live unit; return each block's
/// channel 0, silent (`None`) where the pipeline had nothing to collect.
fn drive_live(unit: &mut Rig, blocks: usize) -> Vec<Option<Vec<f32>>> {
    (0..blocks)
        .map(|b| {
            let out = drive(unit, b);
            std::thread::sleep(PACE);
            out.iter().any(|&s| s != 0.0).then_some(out)
        })
        .collect()
}

/// Drive `blocks` unpaced blocks through a fork.
fn drive_fork(unit: &mut Rig, blocks: usize) -> Vec<Vec<f32>> {
    (0..blocks).map(|b| drive(unit, b)).collect()
}

fn env() -> ProbeEnv {
    ProbeEnv::new()
        .render_mode(render::TAG_PASSTHROUGH)
        .gain_enabled(true)
}

/// **A fork renders what the live instance renders, from the live instance's
/// state** — a gain set on the live plugin before the fork is in the fork's
/// samples — and it renders every block, unpaced, because an offline fork
/// waits for its subprocess.
///
/// Every block the live pipeline collected is compared bit for bit with the
/// fork's block at the same index (both are one block late), and the fork's are
/// checked against the arithmetic too.
///
/// Mutation: drop the `load_state` call in `ServedFork::instance` → the fork
/// renders at 0 dB → fails. Mutation: drop `set_offline_wait` → unpaced blocks
/// read silence → fails. (A client is not `Clone`, so "return the live client
/// as the fork" is not a mutation the code can express;
/// `a_fork_and_the_live_instance_do_not_reach_each_other` runs the two
/// overlapped.)
#[test]
fn a_fork_renders_like_the_live_instance_from_its_state() {
    let _lock = machine_lock();
    let _env = env();
    let probe = load_probe(SAMPLE_RATE);
    probe.client.set_parameter(GAIN, gain_at(-6.0));

    let fork = probe
        .client
        .fork_instance(
            ForkMode::Offline,
            tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0)).expect("a tempo"),
        )
        .expect("the probe forks");
    assert_eq!(fork.descriptor().id, probe.client.descriptor().id);

    const BLOCKS: usize = 40;
    let mut fork = Rig::new(fork, SAMPLE_RATE, BLOCK);
    let forked = drive_fork(&mut fork, BLOCKS);
    assert!(forked[0].iter().all(|&s| s == 0.0), "one block of latency");
    for (b, block) in forked.iter().enumerate().skip(1) {
        assert_eq!(block, &expected(b, -6.0), "fork block {b}");
    }

    let mut live = Rig::new(
        probe.client.bind(
            tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0)).expect("a tempo"),
        ),
        SAMPLE_RATE,
        BLOCK,
    );
    let lived = drive_live(&mut live, BLOCKS);
    let collected: Vec<_> = lived
        .iter()
        .enumerate()
        .filter_map(|(b, o)| o.as_ref().map(|o| (b, o)))
        .collect();
    assert!(
        collected.len() >= 5,
        "the live pipeline collected {} of {BLOCKS} blocks; too few to compare",
        collected.len()
    );
    for (b, block) in collected {
        assert_eq!(block, &forked[b], "live block {b} differs from the fork's");
    }
}

/// **Rendering a fork does not touch the live instance**, with both rendering
/// on two threads over the same span of time: the live output stays exactly
/// what its own input and gain make, and its saved state is what it was
/// before the fork. And **a live parameter change after the fork does not
/// reach the fork**: the live gain moves to -15 dB, the fork stays at -6.
///
/// The overlap is enforced, not hoped for: both threads start from a barrier,
/// the fork keeps rendering until the live run is done, and the test asserts
/// the fork rendered blocks while the live run was still going.
///
/// Mutation: have `ServedFork::instance` skip the fresh launch and return a
/// client on the live bridge (`PluginBridge` shared) → the fork thread's
/// blocks land in the live instance, and the live gain change reaches the
/// "fork" → fails on both halves. Mutation: mark the live run done before it
/// starts → the fork renders nothing while it runs → the overlap check fails.
#[test]
fn a_fork_and_the_live_instance_do_not_reach_each_other() {
    let _lock = machine_lock();
    let _env = env();
    let probe = load_probe(SAMPLE_RATE);
    let handle: PluginHandle = probe.handle.clone();
    probe.client.set_parameter(GAIN, gain_at(-6.0));

    let fork = probe
        .client
        .fork_instance(
            ForkMode::Offline,
            tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0)).expect("a tempo"),
        )
        .expect("the probe forks");
    let state_at_fork = handle.any_thread().save_state().expect("the probe saves");

    // After the fork: the live instance moves to -15 dB.
    probe.client.set_parameter(GAIN, gain_at(-15.0));

    const MIN_FORK_BLOCKS: usize = 40;
    let start = Arc::new(Barrier::new(2));
    let live_done = Arc::new(AtomicBool::new(false));
    let fork_thread = {
        let (start, live_done) = (Arc::clone(&start), Arc::clone(&live_done));
        std::thread::spawn(move || {
            let mut fork = Rig::new(fork, SAMPLE_RATE, BLOCK);
            start.wait();
            let (mut blocks, mut during_live) = (Vec::new(), 0usize);
            while blocks.len() < MIN_FORK_BLOCKS || !live_done.load(Ordering::SeqCst) {
                if !live_done.load(Ordering::SeqCst) {
                    during_live += 1;
                }
                blocks.push(drive(&mut fork, blocks.len()));
            }
            (blocks, during_live)
        })
    };

    let mut live = Rig::new(
        probe.client.bind(
            tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0)).expect("a tempo"),
        ),
        SAMPLE_RATE,
        BLOCK,
    );
    start.wait();
    let lived = drive_live(&mut live, 40);
    live_done.store(true, Ordering::SeqCst);
    let (forked, during_live) = fork_thread.join().expect("the fork thread renders");
    assert!(
        during_live >= 10,
        "the fork rendered only {during_live} blocks while the live run was going; \
         the two did not overlap"
    );

    let mut collected = 0;
    for (b, block) in lived.iter().enumerate() {
        if let Some(block) = block {
            assert_eq!(block, &expected(b, -15.0), "live block {b}");
            collected += 1;
        }
    }
    assert!(
        collected >= 5,
        "the live pipeline collected {collected} blocks"
    );
    for (b, block) in forked.iter().enumerate().skip(1) {
        assert_eq!(block, &expected(b, -6.0), "fork block {b}");
    }

    // The live state moved only by its own parameter change: put the gain
    // back and it is byte for byte what it was at the fork. Through the
    // handle, the path a host has once the node is in a graph.
    handle.any_thread().set_parameter_value(GAIN, gain_at(-6.0));
    assert_eq!(
        handle.any_thread().save_state().expect("the probe saves"),
        state_at_fork,
        "rendering the fork changed the live instance's state"
    );
}

/// **A fresh instance that refuses the live state is a named error**, and
/// through the graph a `ForkError::Source` naming the node, whose cause is
/// that same `PluginForkError::LoadState` — never a fork at the plugin's
/// defaults.
///
/// The refusal is armed after the live instance loaded, so only the fork's
/// server (spawned later, inheriting the environment) refuses.
///
/// Mutation: ignore `load_state`'s result in `ServedFork::instance` →
/// both forks succeed → fails.
#[test]
fn a_state_the_fresh_instance_refuses_is_a_named_fork_error() {
    let _lock = machine_lock();
    let _env = env();
    let probe = load_probe(SAMPLE_RATE);
    let _refuse = ProbeEnv::new().refuse_state_load(true);

    let err = probe
        .client
        .fork_instance(
            ForkMode::Offline,
            tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0)).expect("a tempo"),
        )
        .expect_err("the fresh instance refuses the state");
    assert!(
        matches!(err, PluginForkError::LoadState(_)),
        "expected LoadState, got {err:?}"
    );

    let prepare = Prepare::new(SampleRate(SAMPLE_RATE), Samples(BLOCK));
    let (mut editor, _exec) = Editor::new(prepare);
    let key = NodeKey(7);
    let _controls = editor.insert(
        key,
        "plugin",
        probe.client.bind(
            tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0)).expect("a tempo"),
        ),
    );
    editor
        .spec_mut()
        .topology
        .set_outputs(vec![Source::Node(OutPort { node: key, port: 0 })]);
    match editor.fork(ForkTarget::Node(key), ForkMode::Offline, prepare) {
        Err(ForkError::Source { key: at, cause }) => {
            assert_eq!(at, key);
            assert!(
                matches!(
                    cause.downcast_ref::<PluginForkError>(),
                    Some(PluginForkError::LoadState(_))
                ),
                "the cause is the plugin's: {cause:?}"
            );
        }
        other => panic!("expected ForkError::Source, got {:?}", other.err()),
    }
}

/// **A plugin that cannot save its state cannot be forked**, and says so
/// before any fresh instance is started.
///
/// Mutation: treat a failed save as an empty state (`unwrap_or_default`) →
/// the fork loads at the plugin's defaults and succeeds → fails.
#[test]
fn a_plugin_that_cannot_save_its_state_is_a_named_fork_error() {
    let _lock = machine_lock();
    let _env = env().refuse_state_save(true);
    let probe = load_probe(SAMPLE_RATE);
    let err = probe
        .client
        .fork_instance(
            ForkMode::Live,
            tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0)).expect("a tempo"),
        )
        .expect_err("the live instance refuses to save");
    assert!(
        matches!(err, PluginForkError::SaveState(_)),
        "expected SaveState, got {err:?}"
    );
}

/// **Through the graph**: a bound `PluginClient` inserted into an editor is
/// forkable, and the forked graph renders the plugin at the live instance's
/// gain on the offline timeline — the path an export takes.
/// With its inputs unwired the probe renders its tag, scaled.
///
/// Mutation: hand no fork source from `IntoNode for PluginClient<Bound>`
/// (`fork: None`) → `ForkError::NotForkable` → fails.
#[test]
fn a_graph_holding_a_plugin_forks_and_renders_offline() {
    let _lock = machine_lock();
    let _env = env();
    let probe = load_probe(SAMPLE_RATE);
    probe.client.set_parameter(GAIN, gain_at(-6.0));

    let prepare = Prepare::new(SampleRate(SAMPLE_RATE), Samples(BLOCK));
    let (mut editor, _exec) = Editor::new(prepare);
    let key = NodeKey(1);
    let _controls = editor.insert(
        key,
        "plugin",
        probe.client.bind(
            tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0)).expect("a tempo"),
        ),
    );
    editor
        .spec_mut()
        .topology
        .set_outputs(vec![Source::Node(OutPort { node: key, port: 0 })]);

    let (fork_ed, fork_exec) = editor
        .fork(ForkTarget::Node(key), ForkMode::Offline, prepare)
        .expect("a graph holding a plugin forks");
    let out = Renderer::new(fork_ed, fork_exec).render(BLOCK * 8);
    let want = TAG_P0C0 * amplitude(-6.0);
    assert!(
        out[0][..BLOCK].iter().all(|&s| s == 0.0),
        "pipeline latency"
    );
    assert!(
        out[0][BLOCK..].iter().all(|&s| s == want),
        "every later frame is the tag at -6 dB ({want}); got {:?}",
        &out[0][BLOCK..BLOCK + 4]
    );
    drop(probe.handle);
}

/// Render `blocks` blocks of the offline fork of the plugin through the
/// graph; return the forked editor (to ask its health) and how long it took.
fn render_fork_of(client: PluginClient, blocks: usize) -> (Editor, Duration) {
    let prepare = Prepare::new(SampleRate(SAMPLE_RATE), Samples(BLOCK));
    let (mut editor, _exec) = Editor::new(prepare);
    let key = NodeKey(3);
    let _controls = editor.insert(
        key,
        "plugin",
        client.bind(
            tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0)).expect("a tempo"),
        ),
    );
    editor
        .spec_mut()
        .topology
        .set_outputs(vec![Source::Node(OutPort { node: key, port: 0 })]);
    let (fork_ed, fork_exec) = editor
        .fork(ForkTarget::Node(key), ForkMode::Offline, prepare)
        .expect("forks");
    assert_eq!(fork_ed.fork_health(), Ok(()), "healthy before rendering");
    let mut renderer = Renderer::new(fork_ed, fork_exec);
    let started = Instant::now();
    renderer.render(BLOCK * blocks);
    let took = started.elapsed();
    let (fork_ed, _exec) = renderer.into_parts();
    (fork_ed, took)
}

/// **A fork whose server dies mid-render is a named fault, promptly** — not
/// a silent render that reports success. The fork's server (only: the switch
/// is armed after the live one loaded) aborts on its 8th block; the render
/// finishes without waiting out any budget, and the forked editor reports
/// `Crashed` for the plugin's key with a `PluginRenderFault::Crashed` cause.
///
/// Mutation: hand no health probe from `ServedFork::fork` → `fork_health` is
/// `Ok` → fails. Mutation: never ask the process (`server_died`) in the
/// batcher's wait → the bridge, sent nothing, never notices the death, and the
/// block waits out its 5 s budget → the time bound fails. Mutation: drop the
/// wait's `latch_crash` → on a machine that renders the 40 blocks inside one
/// process poll, the bridge noticed the death but the executor holding it is
/// dropped before `fork_health` asks → `Ok` → fails.
#[test]
fn a_fork_whose_server_dies_mid_render_is_a_crashed_fault() {
    let _lock = machine_lock();
    let _env = env();
    let probe = load_probe(SAMPLE_RATE);
    let _crash = ProbeEnv::new().crash_on_block(8);

    let (fork_ed, took) = render_fork_of(probe.client, 40);
    let fault = fork_ed.fork_health().expect_err("the fork crashed");
    assert_eq!(fault.key, NodeKey(3));
    assert_eq!(fault.kind, ForkFaultKind::Crashed, "{fault}");
    assert!(
        matches!(
            fault.cause.downcast_ref::<PluginRenderFault>(),
            Some(PluginRenderFault::Crashed { .. })
        ),
        "{fault:?}"
    );
    assert!(took < Duration::from_secs(4), "the render took {took:?}");
}

/// **A fork whose server hangs mid-render is a named fault after one budget,
/// not one budget per block.** The fork's server parks from its 5th block
/// for good; with a 1 s budget the 30-block render takes about a second, and
/// the forked editor reports `TimedOut`.
///
/// Mutation: drop the `gave_up` early return in `Batcher::await_output` →
/// every later block waits its own second (~25 s) → the time bound fails.
/// Mutation: never latch `gave_up` → the same, and `fork_health` is `Ok`.
#[test]
fn a_fork_whose_server_hangs_is_a_timed_out_fault_after_one_budget() {
    let _lock = machine_lock();
    let _env = env();
    let config = BridgeConfig {
        timeout_ms: 1_000,
        ..BridgeConfig::default()
    };
    let probe = load_probe_with(config, SAMPLE_RATE);
    let _hang = ProbeEnv::new().block_from(5);

    let (fork_ed, took) = render_fork_of(probe.client, 30);
    let fault = fork_ed.fork_health().expect_err("the fork hung");
    assert_eq!(fault.kind, ForkFaultKind::TimedOut, "{fault}");
    assert!(
        matches!(
            fault.cause.downcast_ref::<PluginRenderFault>(),
            Some(PluginRenderFault::TimedOut { budget }) if *budget == Duration::from_secs(1)
        ),
        "{fault:?}"
    );
    assert!(took < Duration::from_secs(5), "the render took {took:?}");
}

/// **A fork source whose live instance is gone says so at once**, without
/// launching anything: `PluginForkError::LiveGone`. The source holds the live
/// bridge weakly, so it cannot keep a dropped plugin alive either.
///
/// Mutation: answer a gone live bridge the way a strong `Arc` to a dead
/// process would, `SaveState(PluginCrashed)` → not `LiveGone` →
/// fails.
#[test]
fn a_fork_of_a_dropped_plugin_is_live_gone() {
    let _lock = machine_lock();
    let _env = env();
    let probe = load_probe(SAMPLE_RATE);
    let parts = probe
        .client
        .bind(tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0)).expect("a tempo"))
        .into_parts();
    let source = parts.fork.expect("a plugin node is forkable");
    drop(parts.node);
    drop(parts.controls);
    drop(probe.handle);

    let started = Instant::now();
    let cause = match source.fork(ForkMode::Live) {
        Ok(_) => panic!("forked a plugin that is gone"),
        Err(cause) => cause,
    };
    assert!(
        matches!(
            cause.downcast_ref::<PluginForkError>(),
            Some(PluginForkError::LiveGone)
        ),
        "{cause:?}"
    );
    assert!(started.elapsed() < Duration::from_millis(100));
}

/// This process's direct children, read from `/proc` (every thread's list:
/// a child is listed under the thread that spawned it).
#[cfg(target_os = "linux")]
fn children() -> std::collections::BTreeSet<u32> {
    let mut out = std::collections::BTreeSet::new();
    for task in std::fs::read_dir("/proc/self/task").expect("procfs") {
        let path = task.expect("task").path().join("children");
        if let Ok(text) = std::fs::read_to_string(path) {
            out.extend(
                text.split_whitespace()
                    .filter_map(|p| p.parse::<u32>().ok()),
            );
        }
    }
    out
}

/// **A dropped fork's server is reaped**, on every platform: its pid
/// (`PluginHandle::server_pid`) names no live process — not even a zombie —
/// once the fork and its handle are gone, while the live server's still
/// does. The probe is `clap_probe::is_alive` (`kill(pid, 0)` on Unix,
/// `GetExitCodeProcess` on Windows), shared with `real_plugin_pressure.rs`.
///
/// Mutation: `std::mem::forget(fork)` instead of dropping it → the server
/// runs on → fails. Mutation: skip `wait()` in `ProcessGuard::drop` → a
/// zombie answers `kill(pid, 0)` → fails (Unix).
#[test]
fn a_dropped_forks_server_is_reaped() {
    let _lock = machine_lock();
    let _env = env();
    let probe = load_probe(SAMPLE_RATE);
    let live_pid = probe.handle.server_pid().expect("a subprocess plugin");

    let fork = probe
        .client
        .fork_instance(
            ForkMode::Live,
            tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0)).expect("a tempo"),
        )
        .expect("the probe forks");
    let handle = fork.handle();
    let pid = handle.server_pid().expect("the fork has its own server");
    assert_ne!(pid, live_pid, "a process of its own");
    assert!(clap_probe::is_alive(pid), "the fork's server runs");
    drop(handle);
    drop(fork);
    assert!(
        !clap_probe::is_alive(pid),
        "a dropped fork's server is reaped"
    );
    assert!(
        clap_probe::is_alive(live_pid),
        "the live server is untouched"
    );
}

/// **A fork that fails is reaped too** (`LoadState`: the fresh instance
/// started, then refused the state). Its pid never reaches the caller, so
/// this reads this process's children from `/proc` instead: Linux only
/// (`/proc/self/task/*/children`). The guard is the same code everywhere.
///
/// Mutation: `std::mem::forget(fork)` on the `LoadState` error path in
/// `ServedFork::instance` → the refused fork's server stays a child →
/// fails.
#[cfg(target_os = "linux")]
#[test]
fn a_failed_forks_server_is_reaped() {
    let _lock = machine_lock();
    let _env = env();
    let probe = load_probe(SAMPLE_RATE);
    let baseline = children();
    assert!(!baseline.is_empty(), "the live server is a child");

    let _refuse = ProbeEnv::new().refuse_state_load(true);
    let err = probe
        .client
        .fork_instance(
            ForkMode::Live,
            tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0)).expect("a tempo"),
        )
        .err();
    assert!(
        matches!(err, Some(PluginForkError::LoadState(_))),
        "{err:?}"
    );
    assert_eq!(children(), baseline, "a failed fork's server is reaped");
}

/// A live graph holding the plugin at key 9 on the master, to export from.
fn live_graph(client: PluginClient) -> (Editor, tutti_graph::Executor) {
    let (mut live, exec) = Editor::new(Prepare::new(SampleRate(SAMPLE_RATE), Samples(BLOCK)));
    let key = NodeKey(9);
    let _controls = live.insert(
        key,
        "plugin",
        client.bind(
            tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0)).expect("a tempo"),
        ),
    );
    live.spec_mut()
        .topology
        .set_outputs(vec![Source::Node(OutPort { node: key, port: 0 })]);
    (live, exec)
}

/// The master of `live` forked offline at `rate`: the pair an export
/// assembles.
fn fork_pair(live: &Editor, rate: SampleRate) -> (Editor, tutti_graph::Executor) {
    live.fork(
        ForkTarget::Master,
        ForkMode::Offline,
        tutti_export::RenderGraph::prepare(rate),
    )
    .expect("a graph holding a plugin forks")
}

/// [`fork_pair`], assembled as an export does.
fn fork_graph(live: &Editor, rate: SampleRate) -> tutti_export::RenderGraph {
    let (editor, executor) = fork_pair(live, rate);
    tutti_export::RenderGraph::assemble(editor, executor, tutti_export::Setup::new())
        .expect("forked together")
}

/// `config`'s render of `graph` into memory (a job and the planes' width) under
/// a control nobody cancels.
fn render_to_buffers(
    mut graph: tutti_export::RenderGraph,
    config: &(tutti_export::Job, tutti_export::ChannelLayout),
    clock: &dyn tutti_export::RenderClock,
) -> tutti_export::Result<tutti_export::Outcome<tutti_export::Rendered>> {
    tutti_export::render_one(
        &mut graph,
        &config.0,
        clock,
        tutti_export::Output::master(tutti_export::BufferTarget::new(config.1)),
        &mut tutti_export::Control::none(),
    )
    .map(|outcome| outcome.map(|produced| produced.buffers().expect("a buffers output")))
}

/// Export the plugin through `tutti_export` exactly as an export does: fork
/// the live graph offline (`fork_graph`), render half a second to
/// buffers.
fn export_through_a_fork(live: &Editor) -> tutti_export::Result<tutti_export::Rendered> {
    let rate = SampleRate(SAMPLE_RATE);
    let graph = fork_graph(live, rate);
    let config = (
        tutti_export::Job::new(
            rate,
            tutti_types::Seconds(0.5).to_samples(rate),
            Samples(0),
            Samples(0),
        )
        .expect("a render"),
        tutti_export::ChannelLayout::MONO,
    );
    render_to_buffers(graph, &config, &tutti_export::FrozenClock).map(|outcome| {
        outcome
            .finished()
            .expect("nothing cancels a control with no token")
    })
}

/// **An export through a fork whose plugin server dies, or hangs, fails with
/// a named error** — `Error::ForkFailed { key, kind, cause }` — instead of
/// returning half a second of mostly silence as a successful render. A healthy
/// fork exports fine (and audibly), so the check is not a blanket refusal.
///
/// Mutation: skip the `fork_health` check in tutti-export's `with_source` →
/// both failing exports return `Ok` → fails.
#[test]
fn an_export_through_a_failing_fork_fails_by_name() {
    let _lock = machine_lock();
    let _env = env();
    let config = BridgeConfig {
        timeout_ms: 1_000,
        ..BridgeConfig::default()
    };
    let probe = load_probe_with(config, SAMPLE_RATE);
    probe.client.set_parameter(GAIN, gain_at(-6.0));
    let (live, _exec) = live_graph(probe.client);

    let healthy = export_through_a_fork(&live).expect("a healthy fork exports");
    assert!(healthy.planes()[0].iter().any(|&s| s != 0.0), "audible");

    let failed = |kind: ForkFaultKind| {
        let started = Instant::now();
        match export_through_a_fork(&live) {
            Err(tutti_export::Error::ForkFailed {
                key,
                kind: got,
                cause,
            }) => {
                assert_eq!(key, NodeKey(9));
                assert_eq!(got, kind, "{cause}");
                assert!(cause.downcast_ref::<PluginRenderFault>().is_some());
            }
            Err(other) => panic!("expected ForkFailed, got {other}"),
            Ok(_) => panic!("an export through a {kind:?} fork reported success"),
        }
        assert!(started.elapsed() < Duration::from_secs(5));
    };
    {
        let _crash = ProbeEnv::new().crash_on_block(8);
        failed(ForkFaultKind::Crashed);
    }
    let _hang = ProbeEnv::new().block_from(5);
    failed(ForkFaultKind::TimedOut);
}

/// **A clip's note reaches an exported plugin on its frame**, through
/// `tutti_export` as an export runs. The export's fork is prepared at the
/// export block (1024 frames), so its chunk is 1024. The probe's `Notes`
/// gate turns the note-on into its first non-zero frame, which lands at the
/// note's frame plus the pipeline's one chunk. The note is at frame 6000, on
/// neither a 64- nor a 1024-frame boundary.
///
/// Mutation (run): the clip node's fork an empty clip → the gate never
/// opens → fails. (Here chunks and blocks coincide; where a chunk begins or
/// ends inside a block is `a_clip_note_in_a_chunk_that_begins_…`.)
#[test]
fn a_clip_note_reaches_an_exported_plugin_on_its_frame() {
    let _lock = machine_lock();
    let _env = ProbeEnv::new().render_mode(render::NOTES);
    let probe = load_probe(SAMPLE_RATE);
    // 120 BPM at 48 kHz: a beat is 24 000 frames, so beat 0.25 is frame 6000.
    const NOTE_FRAME: usize = 6_000;
    let (mut live, _exec) = live_graph(probe.client);
    feed_note(&mut live, NodeKey(9), NOTE_FRAME);

    // The export's own timeline, rolling from beat 0, and its clock.
    let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        sample_rate: SampleRate(SAMPLE_RATE),
        ..Default::default()
    }));
    let graph = fork_graph(&live, SampleRate(SAMPLE_RATE));
    // The pipeline holds one chunk, the export block. (The probe reports a
    // latency of its own in every mode but delays audio only in `Latency`.)
    let chunk = tutti_export::GRAPH_MAX_BLOCK;
    let config = (
        tutti_export::Job::new(
            SampleRate(SAMPLE_RATE),
            tutti_types::Seconds(0.25).to_samples(SampleRate(SAMPLE_RATE)),
            Samples(0),
            Samples(0),
        )
        .expect("a render"),
        tutti_export::ChannelLayout::MONO,
    );
    let rendered = render_to_buffers(graph, &config, timeline.as_ref())
        .expect("the fork exports")
        .finished()
        .expect("nothing cancels a control with no token");
    let onset = rendered.planes()[0].iter().position(|&s| s != 0.0);
    assert_eq!(
        onset,
        Some(NOTE_FRAME + chunk.get()),
        "the note sounds on its frame, one chunk late"
    );
}

/// Feed the plugin at `plugin` from a clip node holding a chord of `n`
/// notes, started on frame `on` and ended on frame `off`.
fn feed_chord(live: &mut Editor, plugin: NodeKey, n: u8, on: usize, off: usize) {
    use tutti_fixtures::midi;
    use tutti_graph::{EventEdge, EventIn, EventOut};
    use tutti_midi_runtime::{MidiClipNode, TimedMidiEvent};
    use tutti_types::Beat;
    use tutti_ump::{MidiChannel, MidiGroup};

    let events = (0..n).flat_map(|k| {
        // Notes 0..100 on channel 0, 100..200 on channel 1.
        let (g, c, key) = (MidiGroup::FIRST, MidiChannel::of(k / 100), k % 100);
        [
            TimedMidiEvent::new(Beat(on as f64 / 24_000.0), midi::note_on(g, c, key, 0xFFFF)),
            TimedMidiEvent::new(Beat(off as f64 / 24_000.0), midi::note_off(g, c, key, 0)),
        ]
    });
    let clip = NodeKey(1);
    live.insert(clip, "clip", MidiClipNode::new(events));
    live.spec_mut().connect_events(
        EventIn {
            node: plugin,
            port: 0,
        },
        EventEdge::Direct(EventOut {
            node: clip,
            port: 0,
        }),
    );
}

/// **A chord bigger than the plugin's chunk of MIDI ends, and the export
/// says what was lost.** 200 notes start on frame 5 200 and end on frame
/// 5 900 of one 1,024-frame chunk (5 120..6 144): 400 events against the
/// chunk's 256. The chunk keeps every note-off: the first 56 fit behind the
/// 200 note-ons, and each of the other 144 takes the place of the note-on of
/// its own key. 144 note-ons never reach the plugin, and that is the loss:
/// by default the export fails with it, counted where the policy sees it;
/// allowed, it is reported with the same figure. The plugin's gate (open
/// while any note is held) opens one chunk after the note-ons and closes one
/// chunk after the note-offs, on exactly those frames, and stays shut: no
/// note is left sounding.
///
/// Before, the chunk kept the first 256 events: all 200 note-ons and 56
/// note-offs, the gate stayed open to the end of the file, and the export
/// returned `Ok`.
///
/// Mutation (run): `push_displacing` as `push` (a refused note-off is not
/// kept) → 144 notes stay held, the gate never closes → fails. Mutation:
/// drop `io.count_lost(host.lost)` in the plugin node → the default export
/// is `Ok` → fails.
#[test]
fn a_chord_larger_than_a_chunk_ends_and_the_export_counts_the_loss() {
    let _lock = machine_lock();
    let _env = ProbeEnv::new().render_mode(render::NOTES);
    let render = |policy: tutti_export::EventLossPolicy| {
        let probe = load_probe(SAMPLE_RATE);
        let (mut live, _exec) = live_graph(probe.client);
        feed_chord(&mut live, NodeKey(9), 200, 5_200, 5_900);
        let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
            sample_rate: SampleRate(SAMPLE_RATE),
            ..Default::default()
        }));
        let graph = fork_graph(&live, SampleRate(SAMPLE_RATE));
        let config = (
            tutti_export::Job::new(
                SampleRate(SAMPLE_RATE),
                tutti_types::Seconds(0.25).to_samples(SampleRate(SAMPLE_RATE)),
                Samples(0),
                Samples(0),
            )
            .expect("a render")
            .with_event_loss(policy),
            tutti_export::ChannelLayout::MONO,
        );
        render_to_buffers(graph, &config, timeline.as_ref()).map(|outcome| {
            outcome
                .finished()
                .expect("nothing cancels a control with no token")
        })
    };
    let err = render(tutti_export::EventLossPolicy::Fail).expect_err("144 note-ons were lost");
    assert!(
        matches!(err, tutti_export::Error::EventsDropped { count: 144 }),
        "{err:?}"
    );
    let rendered = render(tutti_export::EventLossPolicy::Allow).expect("allowed");
    assert_eq!(rendered.dropped_events(), 144);
    let gate = &rendered.planes()[0];
    let chunk = tutti_export::GRAPH_MAX_BLOCK.get();
    assert_eq!(gate.iter().position(|&s| s != 0.0), Some(5_200 + chunk));
    assert_eq!(
        gate.iter().rposition(|&s| s != 0.0),
        Some(5_900 + chunk - 1),
        "the gate closes on the note-offs' frame and stays shut"
    );
}

/// Feed the plugin at `plugin` from a clip node holding one note-on at frame
/// `frame` of a 120 BPM timeline at [`SAMPLE_RATE`] (a beat is 24 000
/// frames).
fn feed_note(live: &mut Editor, plugin: NodeKey, frame: usize) {
    use tutti_fixtures::midi;
    use tutti_graph::{EventEdge, EventIn, EventOut};
    use tutti_midi_runtime::{MidiClipNode, TimedMidiEvent};
    use tutti_types::Beat;
    use tutti_ump::{MidiChannel, MidiGroup};

    let clip = NodeKey(1);
    live.insert(
        clip,
        "clip",
        MidiClipNode::new([TimedMidiEvent::new(
            Beat(frame as f64 / 24_000.0),
            midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xFFFF),
        )]),
    );
    live.spec_mut().connect_events(
        EventIn {
            node: plugin,
            port: 0,
        },
        EventEdge::Direct(EventOut {
            node: clip,
            port: 0,
        }),
    );
}

/// **A chunk that begins inside a block, or inside a render pass, still
/// places its notes on their frames.** A fork prepared at 480 frames ships
/// 480-frame chunks, rendered in 400-frame blocks: the chunk from frame 6 240
/// begins 240 frames into the block from 6 000 and ends inside the next. The
/// note at 6 300 is in that chunk; the probe's gate opens one chunk after it.
///
/// Rendered twice: in whole blocks, and in 64-frame passes from each block's
/// start with the timeline moved between them (what the engine did while a
/// `Legacy` node was in the graph, and what any host rendering short calls
/// does: the chunk from 6 240 then begins 48 frames into the pass from
/// 6 192).
///
/// Mutation (run): an event's chunk frame taken as its offset in the call
/// (`o` for `at + o - from` in `Chunks::take`) → the note lands late →
/// fails.
#[test]
fn a_clip_note_in_a_chunk_that_begins_mid_block_lands_on_its_frame() {
    for passes in [false, true] {
        let _lock = machine_lock();
        let _env = ProbeEnv::new().render_mode(render::NOTES);
        let probe = load_probe(SAMPLE_RATE);
        const CHUNK: usize = 480;
        const BLOCK: usize = 400;
        const NOTE_FRAME: usize = 6_300;

        let prepare = Prepare::new(SampleRate(SAMPLE_RATE), Samples(CHUNK));
        let (mut live, _exec) = Editor::new(prepare);
        let key = NodeKey(9);
        let _controls = live.insert(
            key,
            "plugin",
            probe.client.bind(
                tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0))
                    .expect("a tempo"),
            ),
        );
        feed_note(&mut live, key, NOTE_FRAME);
        live.spec_mut()
            .topology
            .set_outputs(vec![Source::Node(OutPort { node: key, port: 0 })]);
        let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
            sample_rate: SampleRate(SAMPLE_RATE),
            ..Default::default()
        }));
        let (_fork_ed, mut fork_exec) = live
            .fork(ForkTarget::Master, ForkMode::Offline, prepare)
            .expect("forks");

        let mut out = Vec::new();
        let mut block = vec![0.0f32; BLOCK];
        let pass = if passes { 64 } else { BLOCK };
        while out.len() < NOTE_FRAME + 2 * CHUNK {
            let mut done = 0;
            while done < BLOCK {
                let n = pass.min(BLOCK - done);
                timeline.render_graph(&mut fork_exec, n, &[], &mut [&mut block[done..done + n]]);
                done += n;
            }
            out.extend_from_slice(&block);
        }
        assert_eq!(
            out.iter().position(|&s| s != 0.0),
            Some(NOTE_FRAME + CHUNK),
            "64-frame passes: {passes}: the note sounds on its frame, one chunk late"
        );
    }
}

/// **A clip node's note reaches the plugin's event input on its frame**,
/// through the pipeline, in an export and in chunks that begin inside a
/// block. The clip node (key 1) feeds the plugin's MIDI event input;
/// the plugin's own port is empty. As
/// [`a_clip_note_in_a_chunk_that_begins_mid_block_lands_on_its_frame`], but
/// the note arrives on the event port: 480-frame chunks, 400-frame blocks,
/// the note at 6 300 in the chunk from 6 240.
///
/// Mutation: `Chunks::take` sending nothing → the gate never opens → fails.
/// Mutation: an event's chunk frame taken as its block offset (`o` for
/// `at + o - from`) → the chunk from 6 240 begins 240 frames into the block
/// from 6 000, so the note lands 240 frames late → fails. Mutation: no
/// event input declared (`with_events(0, 0)`) → the edge is refused at
/// compile → fails.
#[test]
fn a_clip_nodes_note_reaches_the_plugins_event_input_on_its_frame() {
    use tutti_fixtures::midi;
    use tutti_graph::{EventEdge, EventIn, EventOut};
    use tutti_midi_runtime::{MidiClipNode, TimedMidiEvent};
    use tutti_types::Beat;
    use tutti_ump::{MidiChannel, MidiGroup};

    let _lock = machine_lock();
    let _env = ProbeEnv::new().render_mode(render::NOTES);
    let probe = load_probe(SAMPLE_RATE);
    const CHUNK: usize = 480;
    const BLOCK: usize = 400;
    const NOTE_FRAME: usize = 6_300;

    let prepare = Prepare::new(SampleRate(SAMPLE_RATE), Samples(CHUNK));
    let (mut live, _exec) = Editor::new(prepare);
    let (clip, key) = (NodeKey(1), NodeKey(9));
    live.insert(
        clip,
        "clip",
        MidiClipNode::new([TimedMidiEvent::new(
            Beat(NOTE_FRAME as f64 / 24_000.0),
            midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xFFFF),
        )]),
    );
    let _controls = live.insert(
        key,
        "plugin",
        probe.client.bind(
            tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0)).expect("a tempo"),
        ),
    );
    live.spec_mut().connect_events(
        EventIn { node: key, port: 0 },
        EventEdge::Direct(EventOut {
            node: clip,
            port: 0,
        }),
    );
    live.spec_mut()
        .topology
        .set_outputs(vec![Source::Node(OutPort { node: key, port: 0 })]);
    let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        sample_rate: SampleRate(SAMPLE_RATE),
        ..Default::default()
    }));
    let (_fork_ed, mut fork_exec) = live
        .fork(ForkTarget::Master, ForkMode::Offline, prepare)
        .expect("a clip node and a plugin fork");

    let mut out = Vec::new();
    let mut block = vec![0.0f32; BLOCK];
    while out.len() < NOTE_FRAME + 2 * CHUNK {
        timeline.render_graph(&mut fork_exec, BLOCK, &[], &mut [&mut block[..]]);
        out.extend_from_slice(&block);
    }
    assert_eq!(
        out.iter().position(|&s| s != 0.0),
        Some(NOTE_FRAME + CHUNK),
        "the note sounds on its frame, one chunk late"
    );
}

/// Records the MIDI reaching its event input as `(frame, first word)`, and
/// renders a silent mono output so a `Master` fork takes it. Forked by clone:
/// the fork records into the same list.
#[derive(Clone)]
struct MidiSink(Arc<std::sync::Mutex<Vec<(u64, u32)>>>);

impl tutti_graph::Node for MidiSink {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::source_unassigned(tutti_types::ChannelLayout::MONO).with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(
        &mut self,
        cx: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        let mut seen = self.0.lock().unwrap();
        for e in io.events(0) {
            if let tutti_graph::EventKind::Midi(ump) = e.kind {
                seen.push((cx.env.frame.get() + u64::from(e.offset.get()), ump.0[0]));
            }
        }
        io.output(0).fill(0.0);
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
}

/// **A plugin's MIDI-out keeps its place against its audio**, on the node's
/// MIDI event output, in an export. The probe in `Notes` mode is a MIDI thru
/// whose gate opens on the note-on's frame, so its audio and its MIDI-out
/// mark the same frame: the note-on (a clip node's, at 6 300) comes out one
/// chunk (480) late on both, in 400-frame blocks, where the chunk the reply
/// belongs to starts playing mid-block. Rendered three times: the export
/// waits for each chunk's reply, so the frame never depends on how the
/// reply raced the audio.
///
/// Mutation: emit at the chunk's frame, ignoring where the ring plays it
/// (`host.emit(o, …)`) → the note-on comes out 240 frames early → fails.
/// Mutation: no event output declared → the edge is refused at compile →
/// fails. Mutation: skip the reply wait in `await_output` → a reply that
/// loses the race to its chunk's audio is taken with the next chunk, at its
/// frame 0 → fails (in 2 of 3 runs here: it is a race, which is why the
/// test renders three times and why the wait exists).
#[test]
fn a_plugins_midi_out_keeps_its_place_against_its_audio() {
    use tutti_fixtures::midi;
    use tutti_graph::{EventEdge, EventIn, EventOut};
    use tutti_midi_runtime::{MidiClipNode, TimedMidiEvent};
    use tutti_types::Beat;
    use tutti_ump::{MidiChannel, MidiGroup};

    let _lock = machine_lock();
    let _env = ProbeEnv::new().render_mode(render::NOTES);
    const CHUNK: usize = 480;
    const BLOCK: usize = 400;
    const NOTE_FRAME: usize = 6_300;
    for _ in 0..3 {
        let probe = load_probe(SAMPLE_RATE);
        let prepare = Prepare::new(SampleRate(SAMPLE_RATE), Samples(CHUNK));
        let (mut live, _exec) = Editor::new(prepare);
        let (clip, key, sink) = (NodeKey(1), NodeKey(9), NodeKey(3));
        let on = midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xFFFF);
        live.insert(
            clip,
            "clip",
            MidiClipNode::new([TimedMidiEvent::new(Beat(NOTE_FRAME as f64 / 24_000.0), on)]),
        );
        let _controls = live.insert(
            key,
            "plugin",
            probe.client.bind(
                tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0))
                    .expect("a tempo"),
            ),
        );
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        live.insert(
            sink,
            "sink",
            tutti_graph::ForkByClone(MidiSink(Arc::clone(&seen))),
        );
        live.spec_mut().connect_events(
            EventIn { node: key, port: 0 },
            EventEdge::Direct(EventOut {
                node: clip,
                port: 0,
            }),
        );
        live.spec_mut().connect_events(
            EventIn {
                node: sink,
                port: 0,
            },
            EventEdge::Direct(EventOut { node: key, port: 0 }),
        );
        live.spec_mut().topology.set_outputs(vec![
            Source::Node(OutPort { node: key, port: 0 }),
            Source::Node(OutPort {
                node: sink,
                port: 0,
            }),
        ]);
        let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
            sample_rate: SampleRate(SAMPLE_RATE),
            ..Default::default()
        }));
        let (_fork_ed, mut fork_exec) = live
            .fork(ForkTarget::Master, ForkMode::Offline, prepare)
            .expect("a clip, a plugin and a sink fork");

        let mut out = Vec::new();
        let (mut block, mut silent) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
        while out.len() < NOTE_FRAME + 2 * CHUNK {
            timeline.render_graph(
                &mut fork_exec,
                BLOCK,
                &[],
                &mut [&mut block[..], &mut silent[..]],
            );
            out.extend_from_slice(&block);
        }
        let heard = out.iter().position(|&s| s != 0.0);
        assert_eq!(
            heard,
            Some(NOTE_FRAME + CHUNK),
            "the gate opens a chunk late"
        );
        let seen = seen.lock().unwrap();
        let note_on = seen.iter().find(|(_, w)| (w >> 20) & 0xf == 0x9);
        assert_eq!(
            note_on.map(|&(f, _)| f),
            Some((NOTE_FRAME + CHUNK) as u64),
            "the MIDI-out's note-on on the frame its audio sounds: {seen:?}"
        );
    }
}

/// A ramp source reading its block's `Env`: frame `t` is [`ramp`]`(t)`. Forks
/// by clone (it holds nothing), so an export of a graph it feeds forks it.
#[derive(Clone)]
struct RampSource;

/// Input frame `t` of the alignment test: distinct, never zero, exact in f32.
fn ramp(t: u64) -> f32 {
    ((t % 997) + 1) as f32 / 1024.0
}

impl tutti_graph::Node for RampSource {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::source_unassigned(tutti_types::ChannelLayout::MONO)
            .with_tail(tutti_types::Tail::Unbounded)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(
        &mut self,
        cx: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        let first = cx.env.frame.get();
        for (i, s) in io.output(0).iter_mut().enumerate() {
            *s = ramp(first + i as u64);
        }
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
}

/// **An export is aligned when the plugin's latency moves in offline mode.**
/// The probe adds 24 frames to its latency (and its delay) once it is told to
/// render offline, as a plugin with a higher-quality offline mode does. The
/// fork is told so asynchronously; its `prepare` waits for the change to land
/// (`PluginBridge::settle`) before the fork's graph is compiled, so the
/// export's plan carries 137 + 24 + the render's chunk (its `MaxBlock`,
/// `GRAPH_MAX_BLOCK`: an export has no device) and trims exactly that: the render is
/// the ramp from frame 0, sample for sample, and the fork reports no fault.
///
/// Mutation: drop the `settle()` from the node's `prepare` → the plan is
/// compiled against the realtime 201 while the plugin delays 225 → the
/// render is 24 frames late, and (the backstop) the fork's health reports
/// `Failed` with `PluginRenderFault::LatencyChanged` → fails.
#[test]
fn an_export_is_aligned_when_the_plugin_latency_moves_offline() {
    let _lock = machine_lock();
    let _env = ProbeEnv::new()
        .render_mode(render::LATENCY)
        .offline_extra_latency(24);
    let probe = load_probe(SAMPLE_RATE);
    let inputs = probe.client.inputs();

    let rate = SampleRate(SAMPLE_RATE);
    let (mut live, _exec) = Editor::new(Prepare::new(rate, Samples(BLOCK)));
    let (src, key) = (NodeKey(1), NodeKey(2));
    live.insert(src, "ramp", tutti_graph::ForkByClone(RampSource));
    let _controls = live.insert(
        key,
        "plugin",
        probe.client.bind(
            tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0)).expect("a tempo"),
        ),
    );
    for port in 0..inputs {
        live.spec_mut().topology.edges.insert(
            tutti_graph::InPort {
                node: key,
                port: port as u16,
            },
            vec![tutti_graph::Edge::Direct(Source::Node(OutPort {
                node: src,
                port: 0,
            }))],
        );
    }
    live.spec_mut()
        .topology
        .set_outputs(vec![Source::Node(OutPort { node: key, port: 0 })]);

    let graph = fork_graph(&live, rate);
    let latency = graph.reported_latency();
    assert_eq!(
        latency,
        Samples(137 + 24 + tutti_export::GRAPH_MAX_BLOCK.get()),
        "the plan carries the offline latency"
    );
    let config = (
        tutti_export::Job::new(
            rate,
            tutti_types::Seconds(0.1).to_samples(rate),
            latency,
            Samples(0),
        )
        .expect("a render"),
        tutti_export::ChannelLayout::MONO,
    );
    let rendered = render_to_buffers(graph, &config, &tutti_export::FrozenClock)
        .expect("a fork whose latency settled exports without a fault")
        .finished()
        .expect("nothing cancels a control with no token");
    let plane = &rendered.planes()[0];
    assert!(!plane.is_empty());
    let wrong = plane
        .iter()
        .enumerate()
        .filter(|&(j, &s)| s != ramp(j as u64))
        .count();
    assert_eq!(
        wrong,
        0,
        "{wrong} of {} samples are not the ramp: the export is misaligned",
        plane.len()
    );
}

/// **An export renders a graph of plugins the same on a worker pool as
/// alone** (doc 013, "The parallel executor"): two out-of-process probes side by side, each
/// at its own gain, forked offline twice — once rendered on the export's
/// thread, once on a four-participant `WorkerPool`, where the two plugins'
/// blocks go to their servers from different threads at once — and the two
/// renders agree bit for bit on both channels.
///
/// Mutation: in `tutti-graph`'s `Shared::run_op`, run `Op::Output` for
/// channel 0 from the slot of channel 1 → the pooled render's channel 0
/// differs → fails.
#[test]
fn an_export_of_plugins_is_bit_identical_on_a_pool() {
    let _lock = machine_lock();
    let _env = env();
    let a = load_probe(SAMPLE_RATE);
    let b = load_probe(SAMPLE_RATE);
    a.client.set_parameter(GAIN, gain_at(-6.0));
    b.client.set_parameter(GAIN, gain_at(-12.0));
    let (mut live, _exec) = Editor::new(Prepare::new(SampleRate(SAMPLE_RATE), Samples(BLOCK)));
    let _ = live.insert(
        NodeKey(9),
        "a",
        a.client.bind(
            tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0)).expect("a tempo"),
        ),
    );
    let _ = live.insert(
        NodeKey(10),
        "b",
        b.client.bind(
            tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0)).expect("a tempo"),
        ),
    );
    live.spec_mut().topology.set_outputs(vec![
        Source::Node(OutPort {
            node: NodeKey(9),
            port: 0,
        }),
        Source::Node(OutPort {
            node: NodeKey(10),
            port: 0,
        }),
    ]);
    let export = |pooled: bool| {
        let rate = SampleRate(SAMPLE_RATE);
        let (editor, mut executor) = fork_pair(&live, rate);
        if pooled {
            executor.set_pool(Some(Arc::new(
                tutti_graph::WorkerPool::builder()
                    .num_threads(4)
                    .build()
                    .expect("a worker pool"),
            )));
        }
        let parallel = executor.is_parallel();
        let graph =
            tutti_export::RenderGraph::assemble(editor, executor, tutti_export::Setup::new())
                .expect("forked together");
        let config = (
            tutti_export::Job::new(
                rate,
                tutti_types::Seconds(0.5).to_samples(rate),
                Samples(0),
                Samples(0),
            )
            .expect("a render"),
            tutti_export::ChannelLayout::STEREO,
        );
        let rendered = render_to_buffers(graph, &config, &tutti_export::FrozenClock)
            .expect("the fork exports")
            .finished()
            .expect("nothing cancels a control with no token");
        (rendered.into_planes(), parallel)
    };
    let (serial, was_parallel) = export(false);
    assert!(!was_parallel);
    let (pooled, was_parallel) = export(true);
    assert!(was_parallel, "two plugins spread over the pool");
    assert!(
        serial.iter().all(|c| c.iter().any(|&s| s != 0.0)),
        "both channels audible"
    );
    let bits = |p: &[Vec<f32>]| -> Vec<Vec<u32>> {
        p.iter()
            .map(|c| c.iter().map(|s| s.to_bits()).collect())
            .collect()
    };
    assert_eq!(bits(&serial), bits(&pooled));
}

/// **MIDI past a chunk's inline capacity is counted, not lost silently.** The
/// out-of-process node sends a chunk's MIDI in an inline list of
/// `MIDI_STACK_CAPACITY` (256); 300 note-ons on its event input in one
/// 512-frame chunk send 256 and count the other 44 on its controls. A chunk
/// within the cap adds nothing.
///
/// Mutation (run): the drop not counted (`count_midi_dropped` removed from
/// `PluginChunks::take`, now the generic node's `count_midi_dropped` in its piece loop) → the count stays 0 → fails.
#[test]
fn midi_past_the_chunks_capacity_is_counted() {
    use tutti_fixtures::midi;
    use tutti_graph::contract::Direct;
    use tutti_graph::{Event, Offset};
    use tutti_plugin_types::MIDI_STACK_CAPACITY;
    use tutti_ump::{MidiChannel, MidiGroup};

    let _lock = machine_lock();
    let _env = ProbeEnv::new().render_mode(render::NOTES);
    let probe = load_probe(SAMPLE_RATE);
    let controls = probe.client.controls();
    let mut node = Direct::boxed(
        tutti_graph::IntoNode::into_node(probe.client.bind(
            tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0)).expect("a tempo"),
        ))
        .0,
        SampleRate(SAMPLE_RATE),
        512,
    );
    let notes = |n: usize| -> Vec<Event> {
        (0..n)
            .map(|i| {
                let on = midi::note_on(
                    MidiGroup::FIRST,
                    MidiChannel::FIRST,
                    (i % 128) as u8,
                    0xFFFF,
                );
                Event::midi(Offset::new(i, Samples(512)).expect("inside"), on)
            })
            .collect()
    };
    node.events(0, &notes(300));
    node.block();
    assert_eq!(
        controls.dropped_midi_in(),
        (300 - MIDI_STACK_CAPACITY) as u64
    );
    node.events(0, &notes(10));
    node.block();
    assert_eq!(
        controls.dropped_midi_in(),
        (300 - MIDI_STACK_CAPACITY) as u64
    );
}

/// **An offline fork of a plugin that refuses its rate renders silence at
/// once, and says why.** The fork's server (only: the switch is armed after
/// the live one loaded) refuses the 96 kHz the fork is prepared at. Each chunk
/// is collected without waiting out the budget, and the forked editor reports
/// `Failed` with a `PluginRenderFault::RateRefused` cause, not `TimedOut`.
///
/// Mutation: drop the refusal check in `Batcher::await_output` → every chunk
/// waits its budget (the first one latches `TimedOut`, 5 s) → the kind and the
/// time bound fail. Drop `latch_refusal`'s use in `ForkWatch::fault` → the
/// fork is reported healthy → fails.
#[test]
fn a_fork_of_a_refused_rate_is_silent_at_once_and_a_failed_fault() {
    let _lock = machine_lock();
    let _env = env();
    let probe = load_probe(SAMPLE_RATE);
    let _refuse = ProbeEnv::new().refuse_rate(96_000.0);

    let prepare = Prepare::new(SampleRate(96_000.0), Samples(BLOCK));
    let (mut editor, _exec) = Editor::new(Prepare::new(SampleRate(SAMPLE_RATE), Samples(BLOCK)));
    let key = NodeKey(3);
    let _controls = editor.insert(
        key,
        "plugin",
        probe.client.bind(
            tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0)).expect("a tempo"),
        ),
    );
    editor
        .spec_mut()
        .topology
        .set_outputs(vec![Source::Node(OutPort { node: key, port: 0 })]);
    let (fork_ed, fork_exec) = editor
        .fork(ForkTarget::Node(key), ForkMode::Offline, prepare)
        .expect("forks");
    let mut renderer = Renderer::new(fork_ed, fork_exec);
    let started = Instant::now();
    let out = renderer.render(BLOCK * 20);
    let took = started.elapsed();
    assert!(out[0].iter().all(|&s| s == 0.0), "silence");
    let (fork_ed, _exec) = renderer.into_parts();
    let fault = fork_ed
        .fork_health()
        .expect_err("the fork refused its rate");
    assert_eq!(fault.kind, ForkFaultKind::Failed, "{fault}");
    assert!(
        matches!(
            fault.cause.downcast_ref::<PluginRenderFault>(),
            Some(PluginRenderFault::RateRefused { rate, .. }) if rate.get() == 96_000.0
        ),
        "{fault:?}"
    );
    assert!(took < Duration::from_secs(3), "the render took {took:?}");
}
