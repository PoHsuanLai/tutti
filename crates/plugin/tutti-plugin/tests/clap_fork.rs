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
use clap_probe::{exclusive, load_probe, load_probe_with, render, ProbeEnv};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use tutti_core::transport::{OfflineTimeline, OfflineTimelineConfig, OfflineTransport};
use tutti_core::{AudioUnit, BufferVec, SampleRate, Samples, F32};
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
fn drive(unit: &mut dyn AudioUnit, b: usize) -> Vec<f32> {
    let ins = unit.inputs();
    let outs = unit.outputs();
    let mut inp = BufferVec::<F32>::new(ins.max(1));
    let mut out = BufferVec::<F32>::new(outs.max(1));
    for ch in 0..ins {
        for i in 0..BLOCK {
            inp.set_scalar(ch, i, input(b, i));
        }
    }
    out.clear();
    unit.process(BLOCK, &inp.buffer_ref(), &mut out.buffer_mut());
    (0..BLOCK).map(|i| out.at_f32(0, i)).collect()
}

/// Drive `blocks` paced blocks through the live unit; return each block's
/// channel 0, silent (`None`) where the pipeline had nothing to collect.
fn drive_live(unit: &mut dyn AudioUnit, blocks: usize) -> Vec<Option<Vec<f32>>> {
    (0..blocks)
        .map(|b| {
            let out = drive(unit, b);
            std::thread::sleep(PACE);
            out.iter().any(|&s| s != 0.0).then_some(out)
        })
        .collect()
}

/// Drive `blocks` unpaced blocks through a fork.
fn drive_fork(unit: &mut dyn AudioUnit, blocks: usize) -> Vec<Vec<f32>> {
    (0..blocks).map(|b| drive(unit, b)).collect()
}

fn offline() -> OfflineTransport {
    Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        sample_rate: SampleRate(SAMPLE_RATE),
        ..Default::default()
    }))
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
/// read silence → fails. Mutation: return a clone of the live client as the
/// "fork" → the fork's run is the live instance's run, and the live blocks
/// driven after it no longer line up with a fresh instance's → fails. (The two
/// run one after the other here; `a_fork_and_the_live_instance_do_not_reach_each_other`
/// runs them overlapped.)
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
    let mut fork: Box<dyn AudioUnit> = Box::new(fork);
    let forked = drive_fork(fork.as_mut(), BLOCKS);
    assert!(forked[0].iter().all(|&s| s == 0.0), "one block of latency");
    for (b, block) in forked.iter().enumerate().skip(1) {
        assert_eq!(block, &expected(b, -6.0), "fork block {b}");
    }

    let mut live: Box<dyn AudioUnit> = Box::new(probe.client.clone());
    let lived = drive_live(live.as_mut(), BLOCKS);
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
/// Mutation: return a clone of the live client from `fork_instance` → the fork
/// thread's blocks land in the live instance, and the live gain change reaches
/// the "fork" → fails on both halves. Mutation: mark the live run done before
/// it starts → the fork renders nothing while it runs → the overlap check fails.
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
            let mut fork: Box<dyn AudioUnit> = Box::new(fork);
            start.wait();
            let (mut blocks, mut during_live) = (Vec::new(), 0usize);
            while blocks.len() < MIN_FORK_BLOCKS || !live_done.load(Ordering::SeqCst) {
                if !live_done.load(Ordering::SeqCst) {
                    during_live += 1;
                }
                blocks.push(drive(fork.as_mut(), blocks.len()));
            }
            (blocks, during_live)
        })
    };

    let mut live: Box<dyn AudioUnit> = Box::new(probe.client.clone());
    start.wait();
    let lived = drive_live(live.as_mut(), 40);
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
    // back and it is byte for byte what it was at the fork.
    probe.client.set_parameter(GAIN, gain_at(-6.0));
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
        .err()
        .expect("the fresh instance refuses the state");
    assert!(
        matches!(err, PluginForkError::LoadState(_)),
        "expected LoadState, got {err:?}"
    );

    let prepare = Prepare::new(SampleRate(SAMPLE_RATE), Samples(BLOCK));
    let (mut editor, _exec) = Editor::new(prepare);
    let key = NodeKey(7);
    editor.insert(key, "plugin", probe.client.clone());
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
        .err()
        .expect("the live instance refuses to save");
    assert!(
        matches!(err, PluginForkError::SaveState(_)),
        "expected SaveState, got {err:?}"
    );
}

/// **Through the graph**: a `PluginClient` inserted into an editor is
/// forkable, and the forked graph renders the plugin at the live instance's
/// gain on the offline timeline — the path an export takes (doc 013 PR 12).
/// With its inputs unwired the probe renders its tag, scaled.
///
/// Mutation: hand no fork source from `IntoNode for PluginClient`
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
    editor.insert(key, "plugin", probe.client.clone());
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
    let _keep: &PluginClient = &probe.client;
}

/// Render `blocks` blocks of the offline fork of the plugin through the
/// graph; return the forked editor (to ask its health) and how long it took.
fn render_fork_of(probe: &clap_probe::LoadedProbe, blocks: usize) -> (Editor, Duration) {
    let prepare = Prepare::new(SampleRate(SAMPLE_RATE), Samples(BLOCK));
    let (mut editor, _exec) = Editor::new(prepare);
    let key = NodeKey(3);
    editor.insert(key, "plugin", probe.client.clone());
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
/// block waits out its 5 s budget → the time bound fails.
#[test]
fn a_fork_whose_server_dies_mid_render_is_a_crashed_fault() {
    let _lock = exclusive();
    let _env = env();
    let probe = load_probe(SAMPLE_RATE);
    let _crash = ProbeEnv::new().crash_on_block(8);

    let (fork_ed, took) = render_fork_of(&probe, 40);
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

    let (fork_ed, took) = render_fork_of(&probe, 30);
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
    let parts = probe.client.clone().into_parts();
    let source = parts.fork.expect("a plugin node is forkable");
    drop(parts.node);
    drop(probe);

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

/// Export the plugin through `tutti_export` exactly as an export does: fork
/// the live graph offline with `RenderGraph::fork`, render half a second to
/// buffers.
fn export_through_a_fork(
    probe: &clap_probe::LoadedProbe,
) -> tutti_export::Result<tutti_export::Rendered> {
    let rate = SampleRate(SAMPLE_RATE);
    let (mut live, _exec) = Editor::new(Prepare::new(rate, Samples(BLOCK)));
    let key = NodeKey(9);
    live.insert(key, "plugin", probe.client.clone());
    live.spec_mut().topology.outputs = vec![Source::Node(OutPort { node: key, port: 0 })];
    let offline = offline();
    let graph = tutti_export::RenderGraph::fork(
        &live,
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

    let healthy = export_through_a_fork(&probe).expect("a healthy fork exports");
    assert!(healthy.planes[0].iter().any(|&s| s != 0.0), "audible");

    let failed = |kind: ForkFaultKind| {
        let started = Instant::now();
        match export_through_a_fork(&probe) {
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
