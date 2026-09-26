//! Forking a hosted plugin **by state transfer**, against the reference CLAP
//! plugin in a real `plugin-server` (doc 013 Phase 3 PR 16, gap 7).
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
use clap_probe::{exclusive, load_probe, load_probe_with, render, ProbeEnv, Rig};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use tutti_core::transport::{OfflineTimeline, OfflineTimelineConfig, OfflineTransport};
use tutti_core::{SampleRate, Samples};
use tutti_graph::{
    Editor, ForkError, ForkFaultKind, ForkMode, ForkTarget, IntoNode, Prepare, Renderer,
};
use tutti_plugin::handles::{PluginClient, PluginHandle};
use tutti_plugin::{BridgeConfig, PluginForkError, PluginRenderFault};
use tutti_plugin_types::{Normalized, ParamAddress, ParamId};
use tutti_types::graph::{OutPort, Source};
use tutti_types::NodeKey;

const SAMPLE_RATE: f64 = 48_000.0;
const BLOCK: usize = 64;

/// The live side is paced to the block period, as every out-of-process suite
/// here is: its pipeline never waits, so an unpaced loop starves the subprocess
/// and reads silence. The fork is *not* paced — it waits for each block, which
/// is the property under test.
const PERIOD: Duration = Duration::from_nanos((BLOCK as f64 / SAMPLE_RATE * 1e9) as u64);
const PACE: Duration = PERIOD.saturating_mul(20);

/// The probe's applied gain (`GAIN_PARAM_ID` in the plugin), and its range in
/// dB. Mirrored rather than imported: this crate links no probe rlib.
const GAIN: ParamAddress = ParamAddress::Opaque(ParamId::new(77));
const GAIN_DB_MIN: f64 = -60.0;
const GAIN_DB_MAX: f64 = 12.0;

/// `probe_tag(0, 0)`, added to channel 0 in `TagPassthrough`.
const TAG_P0C0: f32 = 1.0;

/// The normalized value that lands on `db`.
fn gain_at(db: f64) -> Normalized {
    Normalized::new((db - GAIN_DB_MIN) / (GAIN_DB_MAX - GAIN_DB_MIN))
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

fn offline() -> OfflineTransport {
    OfflineTransport::new(Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        sample_rate: SampleRate(SAMPLE_RATE),
        ..Default::default()
    })))
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
/// Mutation: drop the `load_state` call in `PluginFork::instance` → the fork
/// renders at 0 dB → fails. Mutation: drop `set_offline_wait` → unpaced blocks
/// read silence → fails. (A client is not `Clone` any more, so "return the
/// live client as the fork" is no longer a mutation the code can express;
/// `a_fork_and_the_live_instance_do_not_reach_each_other` runs the two
/// overlapped.)
#[test]
fn a_fork_renders_like_the_live_instance_from_its_state() {
    let _lock = exclusive();
    let _env = env();
    let probe = load_probe(SAMPLE_RATE);
    probe.client.set_parameter(GAIN, gain_at(-6.0));

    let offline = offline();
    let fork = probe
        .client
        .fork_instance(ForkMode::Offline(&offline))
        .expect("the probe forks");
    assert_eq!(fork.descriptor().id, probe.client.descriptor().id);

    const BLOCKS: usize = 40;
    let mut fork = Rig::new(fork, SAMPLE_RATE, BLOCK);
    let forked = drive_fork(&mut fork, BLOCKS);
    assert!(forked[0].iter().all(|&s| s == 0.0), "one block of latency");
    for (b, block) in forked.iter().enumerate().skip(1) {
        assert_eq!(block, &expected(b, -6.0), "fork block {b}");
    }

    let mut live = Rig::new(probe.client.bind(), SAMPLE_RATE, BLOCK);
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
/// Mutation: have `PluginFork::instance` skip the fresh launch and return a
/// client on the live bridge (`PluginBridge` shared) → the fork thread's
/// blocks land in the live instance, and the live gain change reaches the
/// "fork" → fails on both halves. Mutation: mark the live run done before it
/// starts → the fork renders nothing while it runs → the overlap check fails.
#[test]
fn a_fork_and_the_live_instance_do_not_reach_each_other() {
    let _lock = exclusive();
    let _env = env();
    let probe = load_probe(SAMPLE_RATE);
    let handle: PluginHandle = probe.handle.clone();
    probe.client.set_parameter(GAIN, gain_at(-6.0));

    let offline = offline();
    let fork = probe
        .client
        .fork_instance(ForkMode::Offline(&offline))
        .expect("the probe forks");
    let state_at_fork = handle.state().save_state().expect("the probe saves");

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

    let mut live = Rig::new(probe.client.bind(), SAMPLE_RATE, BLOCK);
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
    handle.params().set_parameter_value(GAIN, gain_at(-6.0));
    assert_eq!(
        handle.state().save_state().expect("the probe saves"),
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
/// Mutation: ignore `load_state`'s result in `PluginFork::instance` →
/// both forks succeed → fails.
#[test]
fn a_state_the_fresh_instance_refuses_is_a_named_fork_error() {
    let _lock = exclusive();
    let _env = env();
    let probe = load_probe(SAMPLE_RATE);
    let _refuse = ProbeEnv::new().refuse_state_load(true);

    let offline = offline();
    let err = probe
        .client
        .fork_instance(ForkMode::Offline(&offline))
        .expect_err("the fresh instance refuses the state");
    assert!(
        matches!(err, PluginForkError::LoadState(_)),
        "expected LoadState, got {err:?}"
    );

    let prepare = Prepare::new(SampleRate(SAMPLE_RATE), Samples(BLOCK));
    let (mut editor, _exec) = Editor::new(prepare);
    let key = NodeKey(7);
    let _controls = editor.insert(key, "plugin", probe.client.bind());
    editor.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: key, port: 0 })];
    match editor.fork(ForkTarget::Node(key), ForkMode::Offline(&offline), prepare) {
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
    let _lock = exclusive();
    let _env = env().refuse_state_save(true);
    let probe = load_probe(SAMPLE_RATE);
    let err = probe
        .client
        .fork_instance(ForkMode::Live)
        .expect_err("the live instance refuses to save");
    assert!(
        matches!(err, PluginForkError::SaveState(_)),
        "expected SaveState, got {err:?}"
    );
}

/// **Through the graph**: a bound `PluginClient` inserted into an editor is
/// forkable, and the forked graph renders the plugin at the live instance's
/// gain on the offline timeline — the path an export takes (doc 013 PR 12).
/// With its inputs unwired the probe renders its tag, scaled.
///
/// Mutation: hand no fork source from `IntoNode for PluginClient<Bound>`
/// (`fork: None`) → `ForkError::NotForkable` → fails.
#[test]
fn a_graph_holding_a_plugin_forks_and_renders_offline() {
    let _lock = exclusive();
    let _env = env();
    let probe = load_probe(SAMPLE_RATE);
    probe.client.set_parameter(GAIN, gain_at(-6.0));

    let prepare = Prepare::new(SampleRate(SAMPLE_RATE), Samples(BLOCK));
    let (mut editor, _exec) = Editor::new(prepare);
    let key = NodeKey(1);
    let _controls = editor.insert(key, "plugin", probe.client.bind());
    editor.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: key, port: 0 })];

    let offline = offline();
    let (fork_ed, fork_exec) = editor
        .fork(ForkTarget::Node(key), ForkMode::Offline(&offline), prepare)
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
    let _controls = editor.insert(key, "plugin", client.bind());
    editor.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: key, port: 0 })];
    let offline = offline();
    let (fork_ed, fork_exec) = editor
        .fork(ForkTarget::Node(key), ForkMode::Offline(&offline), prepare)
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
/// Mutation: hand no health probe from `PluginFork::fork` → `fork_health` is
/// `Ok` → fails. Mutation: never ask the process (`server_died`) in the
/// batcher's wait → the bridge, sent nothing, never notices the death, and the
/// block waits out its 5 s budget → the time bound fails. Mutation: drop the
/// wait's `latch_crash` → on a machine that renders the 40 blocks inside one
/// process poll, the bridge noticed the death but the executor holding it is
/// dropped before `fork_health` asks → `Ok` → fails.
#[test]
fn a_fork_whose_server_dies_mid_render_is_a_crashed_fault() {
    let _lock = exclusive();
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
    let _lock = exclusive();
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
    let _lock = exclusive();
    let _env = env();
    let probe = load_probe(SAMPLE_RATE);
    let parts = probe.client.bind().into_parts();
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
    let _lock = exclusive();
    let _env = env();
    let probe = load_probe(SAMPLE_RATE);
    let live_pid = probe.handle.server_pid().expect("a subprocess plugin");

    let fork = probe
        .client
        .fork_instance(ForkMode::Live)
        .expect("the probe forks");
    let handle = PluginHandle::from_client(&fork);
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
/// `PluginFork::instance` → the refused fork's server stays a child →
/// fails.
#[cfg(target_os = "linux")]
#[test]
fn a_failed_forks_server_is_reaped() {
    let _lock = exclusive();
    let _env = env();
    let probe = load_probe(SAMPLE_RATE);
    let baseline = children();
    assert!(!baseline.is_empty(), "the live server is a child");

    let _refuse = ProbeEnv::new().refuse_state_load(true);
    let err = probe.client.fork_instance(ForkMode::Live).err();
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
    let _controls = live.insert(key, "plugin", client.bind());
    live.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: key, port: 0 })];
    (live, exec)
}

/// Export the plugin through `tutti_export` exactly as an export does: fork
/// the live graph offline with `RenderGraph::fork`, render half a second to
/// buffers.
fn export_through_a_fork(live: &Editor) -> tutti_export::Result<tutti_export::Rendered> {
    let rate = SampleRate(SAMPLE_RATE);
    let offline = offline();
    let graph = tutti_export::RenderGraph::fork(
        live,
        ForkTarget::Master,
        ForkMode::Offline(&offline),
        rate,
    )
    .expect("a graph holding a plugin forks");
    let config = tutti_export::ExportConfig {
        render: tutti_export::RenderConfig {
            sample_rate: rate,
            duration_seconds: 0.5,
            ..Default::default()
        },
        encode: tutti_export::EncodeConfig {
            channels: tutti_export::ChannelLayout::MONO,
            ..Default::default()
        },
        dither: tutti_export::Dither::Off,
        ..Default::default()
    };
    tutti_export::render_to_buffers(graph, &config, &tutti_export::FrozenClock)
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
    let _lock = exclusive();
    let _env = env();
    let config = BridgeConfig {
        timeout_ms: 1_000,
        ..BridgeConfig::default()
    };
    let probe = load_probe_with(config, SAMPLE_RATE);
    probe.client.set_parameter(GAIN, gain_at(-6.0));
    let (live, _exec) = live_graph(probe.client);

    let healthy = export_through_a_fork(&live).expect("a healthy fork exports");
    assert!(healthy.planes[0].iter().any(|&s| s != 0.0), "audible");

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

/// **A clip's note reaches an exported plugin on its frame**, however long
/// the pipeline's chunk. The export's fork is prepared at the export block
/// (1024 frames), so its chunk is 1024; the plan holds a `Legacy`-flagged
/// node, so it renders in 64-frame passes and the timeline moves between
/// them. The probe's `Notes` gate turns the note-on into its first non-zero
/// frame, which lands at the note's frame plus the pipeline's one chunk. The note is at frame 6000, on neither a 64- nor a 1024-frame
/// boundary.
///
/// Mutation: gather the chunk's payload at its submission (the last 64-frame
/// pass of the chunk) instead of at its start → the clip's window is read
/// 960 frames late and the note sounds 960 frames early → fails.
/// (The other mutation of the fix, dropping `rebase`, needs a chunk that
/// begins inside a pass: `a_clip_note_in_a_chunk_that_begins_mid_pass_…`.)
#[test]
fn a_clip_note_reaches_an_exported_plugin_on_its_frame() {
    let _lock = exclusive();
    let _env = ProbeEnv::new().render_mode(render::NOTES);
    let probe = load_probe(SAMPLE_RATE);
    // 120 BPM at 48 kHz: a beat is 24 000 frames, so beat 0.25 is frame 6000.
    const NOTE_FRAME: usize = 6_000;
    install_note(&probe.client, NOTE_FRAME);
    let (live, _exec) = live_graph(probe.client);

    // The export's own timeline, rolling from beat 0, and its clock.
    let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        sample_rate: SampleRate(SAMPLE_RATE),
        ..Default::default()
    }));
    let offline = OfflineTransport::new(timeline.clone());
    let graph = tutti_export::RenderGraph::fork(
        &live,
        ForkTarget::Master,
        ForkMode::Offline(&offline),
        SampleRate(SAMPLE_RATE),
    )
    .expect("a graph holding a plugin forks");
    // The pipeline holds one chunk, the export block. (The probe reports a
    // latency of its own in every mode but delays audio only in `Latency`.)
    let chunk = tutti_export::GRAPH_MAX_BLOCK;
    let config = tutti_export::ExportConfig {
        render: tutti_export::RenderConfig {
            sample_rate: SampleRate(SAMPLE_RATE),
            duration_seconds: 0.25,
            ..Default::default()
        },
        encode: tutti_export::EncodeConfig {
            channels: tutti_export::ChannelLayout::MONO,
            ..Default::default()
        },
        dither: tutti_export::Dither::Off,
        ..Default::default()
    };
    let rendered = tutti_export::render_to_buffers(graph, &config, timeline.as_ref())
        .expect("the fork exports");
    let onset = rendered.planes[0].iter().position(|&s| s != 0.0);
    assert_eq!(
        onset,
        Some(NOTE_FRAME + chunk.get()),
        "the note sounds on its frame, one chunk late"
    );
}

/// Install on `client`'s MIDI port a clip holding one note-on at frame
/// `frame` of a 120 BPM timeline at [`SAMPLE_RATE`] (the live clip a fork
/// rebinds onto its render's timeline).
fn install_note(client: &PluginClient, frame: usize) {
    use tutti_midi_runtime::{MidiClipSource, TimedClipEvent};
    use tutti_midi_types::{MidiChannel, MidiEvent, MidiGroup};
    use tutti_types::{Beat, Timeline};

    let live_timeline: Arc<dyn Timeline> = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        sample_rate: SampleRate(SAMPLE_RATE),
        ..Default::default()
    }));
    client.midi_port().install(Arc::new(MidiClipSource::new(
        client.midi_unit_id(),
        vec![TimedClipEvent {
            // 24 000 frames a beat.
            beat: Beat(frame as f64 / 24_000.0),
            event: MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xFFFF),
        }],
        live_timeline,
    )));
}

/// **A chunk that begins inside a render pass still places its notes on
/// their frames.** A fork prepared at 480 frames ships 480-frame chunks; the
/// render hands it 400-frame blocks, which its clock cuts into 64-frame passes
/// from each block's start (the plan holds a `Legacy`-flagged node). The chunk
/// from frame 6240 then begins 48 frames into the pass from 6192 (block 6000,
/// pass 192). The note at 6300 is in that chunk, and the probe's gate opens
/// one chunk after it.
///
/// Mutation: drop `rebase` → the window read from the pass's first frame is
/// sent as the chunk's → the note sounds 48 frames late → fails.
/// Mutation: gather at submission → the window starts at the chunk's last
/// pass → hundreds of frames early → fails.
#[test]
fn a_clip_note_in_a_chunk_that_begins_mid_pass_lands_on_its_frame() {
    let _lock = exclusive();
    let _env = ProbeEnv::new().render_mode(render::NOTES);
    let probe = load_probe(SAMPLE_RATE);
    const CHUNK: usize = 480;
    const BLOCK: usize = 400;
    const NOTE_FRAME: usize = 6_300;
    install_note(&probe.client, NOTE_FRAME);

    let prepare = Prepare::new(SampleRate(SAMPLE_RATE), Samples(CHUNK));
    let (mut live, _exec) = Editor::new(prepare);
    let key = NodeKey(9);
    let _controls = live.insert(key, "plugin", probe.client.bind());
    live.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: key, port: 0 })];
    let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        sample_rate: SampleRate(SAMPLE_RATE),
        ..Default::default()
    }));
    let offline = OfflineTransport::new(timeline.clone());
    let (_fork_ed, mut fork_exec) = live
        .fork(ForkTarget::Master, ForkMode::Offline(&offline), prepare)
        .expect("forks");

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
        tutti_graph::Shape::audio(
            tutti_types::ChannelLayout::EMPTY,
            tutti_types::ChannelLayout::MONO,
        )
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
    let _lock = exclusive();
    let _env = ProbeEnv::new()
        .render_mode(render::LATENCY)
        .offline_extra_latency(24);
    let probe = load_probe(SAMPLE_RATE);
    let inputs = probe.client.inputs();

    let rate = SampleRate(SAMPLE_RATE);
    let (mut live, _exec) = Editor::new(Prepare::new(rate, Samples(BLOCK)));
    let (src, key) = (NodeKey(1), NodeKey(2));
    live.insert(src, "ramp", tutti_graph::ForkByClone(RampSource));
    let _controls = live.insert(key, "plugin", probe.client.bind());
    for port in 0..inputs {
        live.spec_mut().topology.edges.insert(
            tutti_types::graph::InPort {
                node: key,
                port: port as u16,
            },
            tutti_types::graph::Edge::Direct(Source::Node(OutPort { node: src, port: 0 })),
        );
    }
    live.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: key, port: 0 })];

    let offline = offline();
    let graph = tutti_export::RenderGraph::fork(
        &live,
        ForkTarget::Master,
        ForkMode::Offline(&offline),
        rate,
    )
    .expect("the graph forks");
    let latency = graph.reported_latency();
    assert_eq!(
        latency,
        Samples(137 + 24 + tutti_export::GRAPH_MAX_BLOCK.get()),
        "the plan carries the offline latency"
    );
    let config = tutti_export::ExportConfig {
        render: tutti_export::RenderConfig {
            sample_rate: rate,
            duration_seconds: 0.1,
            latency,
            ..Default::default()
        },
        encode: tutti_export::EncodeConfig {
            channels: tutti_export::ChannelLayout::MONO,
            ..Default::default()
        },
        dither: tutti_export::Dither::Off,
        ..Default::default()
    };
    let rendered = tutti_export::render_to_buffers(graph, &config, &tutti_export::FrozenClock)
        .expect("a fork whose latency settled exports without a fault");
    let plane = &rendered.planes[0];
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
