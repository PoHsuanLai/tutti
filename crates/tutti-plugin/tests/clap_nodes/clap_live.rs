//! **A live plugin keeps in step with the device.** The reference CLAP
//! plugin in a real `plugin-server`, rendered by tutti-engine's `Engine` exactly
//! as a device drives it — one call per callback, the callbacks paced to real
//! time — at 480-, 1024- and 441-frame device quanta.
//!
//! A live plugin's pipeline chunk is the device's callback
//! (`Prepare::quantum`), not a fixed 64 frames. With 64, a chunk completed
//! mid-callback would be collected microseconds after it was submitted, and
//! read as silence; with the callback as the chunk, the server has a whole
//! device period to answer. Asserted here: the blocks after warm-up are
//! silent in at most one run, no longer than a scheduler stall plus the
//! pipeline's refill (`max_stall_blocks`), every block is either the input delayed
//! by exactly the declared latency (137 + quantum) or silence, and the graph
//! declares that latency.
//!
//! Needs `cargo build -p tutti-plugin-server` first (see `CLAUDE.md`).

#![cfg(feature = "clap")]

use crate::clap_probe;
use clap_probe::{ProbeEnv, load_probe, render};
use tutti_fixtures::machine_lock;
use tutti_types::MaxBlock;

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tutti_engine::Engine;
use tutti_graph::NodeKey;
use tutti_graph::{Cx, Editor, Io, Node, Prepare, Shape, Status, Unforkable};
use tutti_graph::{Edge, InPort, OutPort, Source};
use tutti_plugin_ipc::RING_SLOTS;
use tutti_transport::Transport;
use tutti_types::{ChannelLayout, InterleavedMut};
use tutti_types::{SampleRate, Samples, Tail};

/// What the probe declares and delays by (`REPORTED_LATENCY_SAMPLES`).
const PROBE_LATENCY: usize = 137;

/// Input frame `t`: distinct, never zero, exact in `f32`.
fn ramp(t: u64) -> f32 {
    ((t % 997) + 1) as f32 / 1024.0
}

/// The ramp as a graph source, read off each block's `Env`.
struct RampSource;

impl Node for RampSource {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).with_tail(Tail::Unbounded)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let first = cx.env.frame.get();
        for (i, s) in io.output(0).iter_mut().enumerate() {
            *s = ramp(first + i as u64);
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// What a live run saw.
struct Run {
    declared: Samples,
    /// Blocks after warm-up.
    blocks: usize,
    /// Of those, entirely silent (block indices).
    silent: Vec<usize>,
    /// Of those, neither silent nor the input delayed by `declared`.
    wrong: Vec<usize>,
}

/// Keeps idle CPUs awake for the length of a paced run (Linux only).
///
/// Measured on an idle 4-vCPU VM: a thread woken from an idle CPU is
/// sometimes 10 to 28 ms late (a bare `sleep(1 ms)` loop with no tutti code
/// oversleeps by more than 10 ms about once in 4000 calls), at any thread
/// priority, and the late thread is the host's bridge thread or the server's
/// about once in 1000 blocks. The plugin gets one period to answer, so such a
/// wake-up costs the block and its successors up to the next fresh one: the
/// source of the stalls `judge_stall` tolerates. With spinners on the other
/// CPUs at the lowest priority (they yield to any waking thread) the late
/// wake-ups fell from 8 in 5249 blocks to 1 in 8399. A device's callback is
/// woken by an interrupt, not from an idle CPU, so this keeps the run closer
/// to one.
///
/// Safe where the CPU count is a quota: fewer than 4 CPUs spawn none, and
/// `cpus / 2` bounds the rest. Every caller holds [`exclusive`], so one
/// process spins at a time. A spinner whose `setpriority` fails exits rather
/// than spin at normal priority; other platforms spawn none.
struct KeepAwake {
    stop: Arc<AtomicBool>,
    spinners: Vec<JoinHandle<()>>,
}

/// Spinners for `cpus` CPUs.
fn spinner_count(cpus: usize) -> usize {
    if cfg!(target_os = "linux") && cpus >= 4 {
        cpus / 2
    } else {
        0
    }
}

impl KeepAwake {
    fn new() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
        let spinners = (0..spinner_count(cpus))
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    // Nice 19 on the calling thread; a spinner that cannot
                    // get it would compete with the run.
                    #[cfg(target_os = "linux")]
                    // SAFETY: `setpriority` takes no pointers.
                    if unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 19) } != 0 {
                        return;
                    }
                    while !stop.load(Ordering::Relaxed) {
                        std::hint::spin_loop();
                    }
                })
            })
            .collect();
        Self { stop, spinners }
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.spinners.drain(..) {
            let _ = t.join();
        }
    }
}

/// Callbacks a [`run_live`] burst makes back to back.
const BURST: usize = 3;

/// Render `blocks` callbacks of `quantum` frames at `rate`, paced to real
/// time, through an engine whose graph is the ramp into the probe. With
/// `burst_at`, the callbacks from that block (counted after warm-up) on run
/// [`BURST`] in a row with no pause, as a device does when it catches up.
fn run_live(rate: f64, quantum: usize, blocks: usize, burst_at: Option<usize>) -> Run {
    let probe = load_probe(rate);
    let _awake = KeepAwake::new();
    let inputs = probe.client.inputs();
    let prepare = Prepare::new(SampleRate(rate), MaxBlock::of(Samples(quantum)))
        .with_quantum(MaxBlock::of(Samples(quantum)));
    let (mut ed, exec) = Editor::new(prepare);
    let (src, key) = (NodeKey(1), NodeKey(2));
    ed.insert(src, "ramp", Unforkable(RampSource));
    let _controls = ed.insert(
        key,
        "plugin",
        probe.client.bind(
            tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0)).expect("a tempo"),
        ),
    );
    for port in 0..inputs {
        ed.spec_mut().topology.edges.insert(
            InPort {
                node: key,
                port: port as u16,
            },
            vec![Edge::Direct(Source::Node(OutPort { node: src, port: 0 }))],
        );
    }
    ed.spec_mut()
        .topology
        .set_outputs(vec![Source::Node(OutPort { node: key, port: 0 })]);
    ed.commit().expect("the graph compiles");
    let declared = ed.spec().topology.nodes[&key].latency;

    let transport = Transport::new(SampleRate::new(rate));
    let engine = Engine::new(&transport, &mut ed, exec).expect("the engine builds");
    let period = Duration::from_secs_f64(quantum as f64 / rate);
    let warm_up = 8 + declared.get().div_ceil(quantum);
    let mut buf = vec![0.0f32; quantum];
    let (mut silent, mut wrong) = (Vec::new(), Vec::new());
    let mut next = Instant::now();
    for b in 0..warm_up + blocks {
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::MONO));
        ed.collect();
        if b >= warm_up {
            let first = (b * quantum) as u64;
            if buf.iter().all(|&s| s == 0.0) {
                silent.push(b - warm_up);
            } else if buf
                .iter()
                .enumerate()
                .any(|(i, &s)| s != ramp(first + i as u64 - declared.get() as u64))
            {
                wrong.push(b);
            }
        }
        if burst_at.is_some_and(|at| (warm_up + at..warm_up + at + BURST - 1).contains(&b)) {
            continue;
        }
        // A device's callbacks stay a period apart: one that oversleeps (a
        // loaded machine) is not followed by a burst of catch-up calls, which
        // would hand the server no period to answer and so read as silence.
        let now = Instant::now();
        next += period;
        if next <= now {
            next = now + period;
        }
        std::thread::sleep(next - now);
    }
    drop(probe.handle);
    Run {
        declared,
        blocks,
        silent,
        wrong,
    }
}

/// Longest scheduler stall a paced run tolerates, once. Empirical: the
/// wake-up delays measured on the 4-vCPU VM were 10 to 28 ms (max 28 ms), and
/// 36 ms leaves headroom over that for the test thread and the server each
/// being late. Not derived from a scheduler's constants, which differ by
/// kernel and CPU count.
const STALL: Duration = Duration::from_millis(36);

/// Silent blocks one stall of [`STALL`] may cost at `period`: the blocks the
/// stall spans, plus `RING_SLOTS` while the pipeline refills.
fn max_stall_blocks(period: Duration) -> usize {
    STALL.as_nanos().div_ceil(period.as_nanos()) as usize + RING_SLOTS
}

/// `Ok` iff the sorted block indices `silent` form at most one contiguous run
/// and it is no longer than `max_run`. A second run, or a longer one, is a
/// fault in the pipeline and not a stall.
///
/// `neutral` is a range whose blocks are not judged (the burst window): they
/// count as silent when runs are formed, so a stall straddling its edge is one
/// run, and not towards a run's length.
fn judge_stall(silent: &[usize], max_run: usize, neutral: Range<usize>) -> Result<(), String> {
    let mut all: Vec<usize> = silent.iter().copied().chain(neutral.clone()).collect();
    all.sort_unstable();
    all.dedup();
    let mut runs: Vec<Vec<usize>> = Vec::new();
    for &b in &all {
        match runs.last_mut() {
            Some(run) if run.last().is_some_and(|&l| l + 1 == b) => run.push(b),
            _ => runs.push(vec![b]),
        }
    }
    for run in &mut runs {
        run.retain(|b| !neutral.contains(b));
    }
    runs.retain(|r| !r.is_empty());
    if runs.len() > 1 {
        return Err(format!("{} separate silent runs: {silent:?}", runs.len()));
    }
    match runs.first() {
        Some(r) if r.len() > max_run => Err(format!(
            "one silent run of {} blocks (> {max_run}): {r:?}",
            r.len()
        )),
        _ => Ok(()),
    }
}

#[test]
fn tolerates_one_bounded_stall() {
    assert!(judge_stall(&[], 4, 0..0).is_ok());
    assert!(judge_stall(&[22, 23, 24], 4, 0..0).is_ok());
    assert!(
        judge_stall(&[9, 10, 11, 12], 4, 0..0).is_ok(),
        "at the bound"
    );
    assert!(
        judge_stall(&[9, 10, 11, 12, 13], 4, 0..0).is_err(),
        "bound + 1"
    );
    assert!(
        judge_stall(&[22, 23, 24, 87, 88], 4, 0..0).is_err(),
        "two stalls"
    );
    assert!(
        judge_stall(&[5, 7], 4, 0..0).is_err(),
        "a gap of one block separates"
    );
}

#[test]
fn a_stall_straddling_the_neutral_window_is_one_run() {
    let window = 60..65;
    assert!(judge_stall(&[58, 59, 65, 66], 4, window.clone()).is_ok());
    assert!(judge_stall(&[59, 65], 4, window.clone()).is_ok());
    assert!(judge_stall(&[], 4, window.clone()).is_ok(), "empty window");
    assert!(
        judge_stall(&[56, 57, 58, 59, 65], 4, window.clone()).is_err(),
        "five blocks outside the window exceed the bound"
    );
    assert!(
        judge_stall(&[10, 11, 58, 59], 4, window.clone()).is_err(),
        "a stall away from the window is a second run"
    );
    assert!(
        judge_stall(&[57], 4, window).is_ok(),
        "one stall near the window"
    );
}

#[test]
fn a_stall_bound_follows_the_block_period() {
    // 36 ms over 10 ms blocks is 4 blocks, over 21.33 ms blocks 2.
    assert_eq!(max_stall_blocks(Duration::from_millis(10)), 4 + RING_SLOTS);
    assert_eq!(
        max_stall_blocks(Duration::from_secs_f64(1024.0 / 48_000.0)),
        2 + RING_SLOTS
    );
}

fn assert_live(rate: f64, quantum: usize) {
    let run = run_live(rate, quantum, 200, None);
    eprintln!(
        "{quantum}-frame callbacks: declared {:?}, {} of {} blocks silent, {} wrong",
        run.declared,
        run.silent.len(),
        run.blocks,
        run.wrong.len()
    );
    let period = Duration::from_secs_f64(quantum as f64 / rate);
    if let Err(why) = judge_stall(&run.silent, max_stall_blocks(period), 0..0) {
        panic!(
            "{quantum}-frame callbacks: {why} of {} blocks: more than one \
             scheduler stall, or the server was not given the device period to answer",
            run.blocks
        );
    }
    assert!(
        run.wrong.is_empty(),
        "{quantum}-frame callbacks: blocks {:?} are neither silent nor the input \
         delayed by {:?}",
        run.wrong,
        run.declared
    );
    assert_eq!(
        run.declared,
        Samples(PROBE_LATENCY + quantum),
        "a {quantum}-frame device: the plugin's latency plus one callback"
    );
}

/// 480-frame callbacks at 48 kHz: not a multiple of 64, so a 64-frame
/// pipeline would collect most chunks mid-callback.
///
/// Ignoring `Prepare::quantum` is invisible here (the graph's `MaxBlock` is the same
/// 480); the batcher's unit test pins it.
#[test]
fn a_480_frame_device_hears_its_plugin() {
    let _lock = machine_lock();
    let _env = ProbeEnv::new().render_mode(render::LATENCY);
    assert_live(48_000.0, 480);
}

/// 1024-frame callbacks at 48 kHz: sixteen 64-frame passes per callback, of
/// which the reversed pipeline could fill only the first.
#[test]
fn a_1024_frame_device_hears_its_plugin() {
    let _lock = machine_lock();
    let _env = ProbeEnv::new().render_mode(render::LATENCY);
    assert_live(48_000.0, 1024);
}

/// 441-frame callbacks at 44.1 kHz (10 ms): odd, so no power-of-two chunk
/// could line up with it.
#[test]
fn a_441_frame_device_hears_its_plugin() {
    let _lock = machine_lock();
    let _env = ProbeEnv::new().render_mode(render::LATENCY);
    assert_live(44_100.0, 441);
}

/// **A burst of catch-up callbacks costs blocks, never correctness.** The
/// paced runs above no longer exercise back-to-back calls, which hand the
/// server no period to answer in. The slab ring is `RING_SLOTS` (2) deep and a
/// block more than `MAX_BEHIND` (`RING_SLOTS - 1`, 1) behind the newest is
/// dropped by design, so a burst of [`BURST`] callbacks may lose blocks as
/// silence, and the loss is bounded by the burst and its ring: the
/// [`BURST`] callbacks plus `RING_SLOTS` more while the pipeline refills.
/// Asserted: no block is ever neither silent nor the delayed input (a dropped
/// block must not surface stale or torn data), and outside that window the
/// stream is as sound as a paced run (at most one bounded stall), counted from
/// `SETTLE` blocks after the run's warm-up: a loaded machine can still be
/// starting the server's pipeline then, which says nothing about the burst.
/// How many blocks the burst itself drops depends on how fast the server
/// answers, so that number is not asserted.
///
/// The mechanism of the drop is pinned by `dispatch`'s unit tests; this pins the
/// outcome a device sees.
#[test]
fn a_burst_of_callbacks_loses_blocks_but_never_corrupts_them() {
    const QUANTUM: usize = 480;
    const BLOCKS: usize = 200;
    const BURST_AT: usize = 60;
    /// Blocks after the run's warm-up that may still be start-up silence.
    const SETTLE: usize = 10;
    let _lock = machine_lock();
    let _env = ProbeEnv::new().render_mode(render::LATENCY);

    let run = run_live(48_000.0, QUANTUM, BLOCKS, Some(BURST_AT));

    assert!(
        run.wrong.is_empty(),
        "blocks {:?} are neither silent nor the input delayed by {:?}: a block \
         the ring dropped surfaced as data",
        run.wrong,
        run.declared
    );
    eprintln!("burst at {BURST_AT}: silent blocks {:?}", run.silent);
    let window = BURST_AT..BURST_AT + BURST + RING_SLOTS;
    let silent: Vec<usize> = run
        .silent
        .iter()
        .copied()
        .filter(|&b| b >= SETTLE)
        .collect();
    let period = Duration::from_secs_f64(QUANTUM as f64 / 48_000.0);
    if let Err(why) = judge_stall(&silent, max_stall_blocks(period), window.clone()) {
        panic!("outside the burst window {window:?} from {SETTLE}: {why}");
    }
}
