//! Pressure test: many real out-of-process plugins, driven at audio-callback
//! timing, some of them stalled, timed per step.
//!
//! # What this exists to catch
//!
//! Per-plugin waits on the audio thread would *sum*: the graph's serial
//! executor runs nodes one after another in one callback, so N stalled plugins
//! would cost N x budget, and 3 is enough to overrun 64 frames at 48 kHz. The
//! synthetic `stalled_plugins_do_not_stall_the_audio_thread` test proves
//! nothing waits using mock servers; this proves it with **real plugin
//! subprocesses**,
//! which the mock cannot model: real scheduling, real dlopen'd DSP, real
//! shared-memory traffic, real socket round-trips.
//!
//! Only a **stalled** plugin can show a wait. The bridge is pipelined: a block
//! collects the reply to the block before, which a plugin that keeps up has
//! already published, so a design that waits for replies pays nothing while
//! every plugin answers in time. The timing tests here therefore stall probes
//! on purpose (see `load_stalled`) and assert on the steps that drive them.
//!
//! `EFFECTS` names macOS system paths, so on a bare checkout no installed
//! plugin is found. `load_n` then falls back to the **reference CLAP probe**,
//! which cargo builds as a dev-dependency, so these run anywhere the `clap`
//! feature is on:
//!
//! ```text
//! cargo build -p tutti-plugin-server
//! cargo nextest run -p tutti-plugin --features clap --test real_plugin_pressure
//! ```
//!
//! `just pressure` wraps both. Installed third-party plugins are still
//! preferred when present; each run prints which of the two it drove, and the
//! numbers mean different things — see `load_n_from_probe`.
//!
//! No `--test-threads=1` needed: these serialize on an internal lock, because
//! two of them running at once starve each other's subprocesses (see
//! [`EXCLUSIVE`]).
//!
//! Reference figures from a 12-core machine, debug build, TAL-Reverb-4:
//! 1 plugin 23 us median, 2 -> 60 us, 4 -> 135 us, 8 -> 303 us. That is
//! ~23-38 us *per plugin*, against the 667 us per plugin the synchronous
//! design could pay. Debug build, so release will be faster; the point is the
//! order of magnitude, not the absolute number. Callback wall time and
//! deadline misses are printed, not asserted: see
//! `eight_real_plugins_half_stalled_never_wait_for_replies`. What is asserted
//! about cost is a working step's median on-CPU time, against the old
//! 200 us per-plugin ceiling (`assert_working_steps_cheap`).
//!
//! How many blocks carry audio depends on the plugin subprocess running at
//! realtime priority (`plugin-server`'s `raise_to_realtime`). Without it the
//! host's callback thread is realtime — the OS audio backend makes it so — but
//! the subprocess it is waiting on is not, and the blocks that arrive in time
//! swung between 29 and 492 of 500 across identical runs. With it: 491-498 of
//! 500. The audio floor asserted here (70% of blocks, `assert_heard`) is
//! too loose to guard that call under load, so
//! `plugin_server_audio_thread_is_raised_where_the_os_allows` checks it
//! directly.
//!
//! # The timing constraint these tests must respect
//!
//! Driving blocks in a tight loop makes every block read back silent, and that
//! is CORRECT: the subprocess gets no wall-clock time to run, so nothing is ever
//! published and the host substitutes silence. A real callback spends one block
//! period per block (1.33 ms at 64/48k); a bare loop runs thousands in that
//! time. So these tests pace themselves to the real period — otherwise they
//! would measure starvation rather than throughput.
//!
//! That starvation path is worth a test of its own, and gets one below.

// The whole file needs a format host to load anything, and the builders it uses
// are feature-gated. Without this the default `cargo test` fails to compile
// rather than simply skipping. With `clap` on they run by default, against the
// reference probe where no third-party plugin is installed.
#![cfg(any(feature = "clap", feature = "vst3", feature = "au"))]

// The reference CLAP probe, so this harness has something to drive on a bare
// checkout. `clap_probe` is the same module the out-of-process CLAP suites
// use; it resolves the cdylib and the `plugin-server` binary that `build.rs`
// located, both of which cargo builds as dev-dependencies.
// Also built for VST3 alone: `repeated_load_and_drop_leaves_no_subprocesses`
// runs there too and checks its servers with `clap_probe::is_alive`, which is
// a plain process check, not CLAP's. (Without this the `vst3`-only build of
// this suite did not compile.)
#[cfg(any(feature = "clap", feature = "vst3"))]
#[path = "support/clap_probe.rs"]
mod clap_probe;

#[cfg(not(feature = "clap"))]
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Planar scratch, `channels` × [`BLOCK`] frames: what this harness fills and
/// measures.
struct Planes(Vec<Vec<f32>>);

impl Planes {
    fn new(channels: usize) -> Self {
        Self(vec![vec![0.0; BLOCK]; channels])
    }

    #[cfg(any(feature = "clap", feature = "vst3"))]
    fn clear(&mut self) {
        for c in &mut self.0 {
            c.fill(0.0);
        }
    }

    fn at(&self, channel: usize, i: usize) -> f32 {
        self.0[channel][i]
    }
}

/// A loaded plugin as the only node of a graph, driven with the
/// buffer types this suite fills and measures (`fill_sine`, `peak`).
///
/// A block reaches the plugin node through the executor, with the `Env` the
/// engine would give it.
/// The global inputs feed its inputs, its outputs feed the global outputs.
struct GraphUnit {
    renderer: tutti_graph::Renderer,
    /// Read only by [`latency`](Self::latency), for the VST3/AU null test.
    #[cfg(any(feature = "vst3", feature = "au"))]
    key: tutti_types::NodeKey,
    inputs: usize,
    outputs: usize,
    ins: Vec<Vec<f32>>,
    outs: Vec<Vec<f32>>,
}

impl GraphUnit {
    fn new<N: tutti_graph::IntoNode>(node: N, inputs: usize, outputs: usize) -> Self {
        assert!(
            inputs <= 16 && outputs <= 16,
            "`run` holds 16 ports on the stack"
        );
        let layout =
            |n: usize| tutti_types::ChannelLayout::from_count(u16::try_from(n).expect("ports"));
        let mut g = tutti_graph::GraphBuilder::new(layout(inputs), layout(outputs));
        let (key, _controls) = g.add_with_controls(node);
        g.pipe_input(key).pipe_output(key);
        let renderer = g
            .renderer(tutti_graph::Prepare::new(
                tutti_types::SampleRate(SAMPLE_RATE),
                tutti_types::Samples(BLOCK),
            ))
            .expect("a one-plugin graph compiles");
        #[cfg(not(any(feature = "vst3", feature = "au")))]
        let _ = key;
        Self {
            renderer,
            #[cfg(any(feature = "vst3", feature = "au"))]
            key,
            inputs,
            outputs,
            ins: vec![vec![0.0; BLOCK]; inputs],
            outs: vec![vec![0.0; BLOCK]; outputs],
        }
    }

    /// A loaded plugin, whichever backend it landed in.
    fn from_plugin(plugin: tutti_plugin::catalog::Plugin) -> Self {
        let (inputs, outputs) = (
            plugin.loaded().total_inputs(),
            plugin.loaded().total_outputs(),
        );
        Self::new(plugin, inputs, outputs)
    }

    fn inputs(&self) -> usize {
        self.inputs
    }

    fn outputs(&self) -> usize {
        self.outputs
    }

    /// The node's latency as the graph compiles it: its declared `Shape`.
    #[cfg(any(feature = "vst3", feature = "au"))]
    fn latency(&self) -> Option<f64> {
        let l = self.renderer.editor().spec().topology.nodes[&self.key].latency;
        Some(l.get() as f64)
    }

    /// One block: `input`'s first `inputs` channels in, the plugin's outputs
    /// into `output`'s first `outputs` channels. [`stage`](Self::stage),
    /// [`run`](Self::run) and [`read`](Self::read) in one, for the untimed
    /// callers.
    fn process(&mut self, size: usize, input: &Planes, output: &mut Planes) {
        self.stage(size, input);
        self.run(size);
        self.read(size, output);
    }

    /// Copy `input` into this unit's planar input buffers. Not the plugin
    /// path: outside a timed region.
    fn stage(&mut self, size: usize, input: &Planes) {
        for (c, ch) in self.ins.iter_mut().enumerate() {
            for (i, s) in ch[..size].iter_mut().enumerate() {
                *s = input.at(c, i);
            }
        }
    }

    /// The plugin path, and nothing else: one executor block over the staged
    /// buffers, the port tables on the stack. What a timed region measures.
    fn run(&mut self, size: usize) {
        const MAX: usize = 16;
        let mut ins: [&[f32]; MAX] = [&[]; MAX];
        for (slot, c) in ins.iter_mut().zip(&self.ins) {
            *slot = &c[..size];
        }
        let mut outs: [&mut [f32]; MAX] = std::array::from_fn(|_| Default::default());
        for (slot, c) in outs.iter_mut().zip(self.outs.iter_mut()) {
            *slot = &mut c[..size];
        }
        let transport = tutti_graph::Transport::default();
        self.renderer.executor_mut().process(
            size,
            &transport,
            &ins[..self.inputs],
            &mut outs[..self.outputs],
        );
    }

    /// Copy the last block's output out, and drain what the executor sent
    /// back. Outside a timed region.
    fn read(&mut self, size: usize, output: &mut Planes) {
        self.renderer.editor_mut().collect();
        for (c, ch) in self.outs.iter().enumerate() {
            for (i, &s) in ch[..size].iter().enumerate() {
                output.0[c][i] = s;
            }
        }
    }
}

/// Take the machine, **across processes**.
///
/// Under `cargo nextest` a `static Mutex` serializes nothing: nextest gives
/// every test its own *process*, so a process-local lock is uncontended in
/// each one. This uses `clap_probe.rs`'s lock directory instead — `create_dir`
/// is atomic and fails with `AlreadyExists` on every OS this builds for.
///
/// What needs serializing is wall clock, which is process-global: each test
/// paces itself to a real block period, so neighbours halve the time each
/// subprocess gets. The process count does not: the leak test probes only the
/// pids it launched itself, so a neighbour's servers are invisible to it.
#[cfg(feature = "clap")]
fn exclusive() -> clap_probe::cross_process_lock::Guard {
    clap_probe::exclusive()
}

/// Without `clap` there is no probe to fall back to, so every test here skips
/// unless third-party plugins are installed — which happens only when someone
/// runs this deliberately, on one machine, usually alone. The `Mutex` is kept
/// for that build rather than duplicating the lock directory into a second
/// place for a case that does not race in practice.
#[cfg(not(feature = "clap"))]
fn exclusive() -> MutexGuard<'static, ()> {
    static EXCLUSIVE: Mutex<()> = Mutex::new(());
    EXCLUSIVE.lock().unwrap_or_else(|e| e.into_inner())
}

const SAMPLE_RATE: f64 = 48_000.0;
const BLOCK: usize = 64;

/// One block's worth of wall time — the audio callback's deadline.
const PERIOD: Duration = Duration::from_nanos((BLOCK as f64 / SAMPLE_RATE * 1e9) as u64);

/// Effects, not synths: a synth ignores its input, so it cannot show that audio
/// traversed the graph. Missing ones are skipped.
///
/// `load_n` cycles through this list, so more than one entry means the multi-plugin
/// cases run a *mix* of plugins and formats rather than N copies of one — closer to
/// a real session, and it exercises both format hosts in the same callback.
#[cfg(any(feature = "clap", feature = "vst3"))]
const EFFECTS: &[&str] = &[
    #[cfg(feature = "clap")]
    "/Library/Audio/Plug-Ins/CLAP/TAL-Reverb-4.clap",
    #[cfg(feature = "vst3")]
    "/Library/Audio/Plug-Ins/VST3/TAL-Reverb-4.vst3",
    // Declares 184 samples of its own latency, unlike the TAL plugins' 0, so the
    // mixed runs cover a non-trivial PDC sum.
    #[cfg(feature = "vst3")]
    "/Library/Audio/Plug-Ins/VST3/TDR Nova.vst3",
];

#[cfg(any(feature = "clap", feature = "vst3"))]
fn available_effects() -> Vec<&'static str> {
    EFFECTS
        .iter()
        .copied()
        .filter(|p| std::path::Path::new(p).exists())
        .collect()
}

/// The fallback when no third-party plugin is installed: `count` instances of
/// the **reference CLAP probe**, through the same real `plugin-server`
/// subprocess and shared-memory bridge.
///
/// Why this exists. Every test in this file was `#[ignore]`d for needing
/// plugins *installed on the machine*, and `EFFECTS` names macOS system paths
/// — so on Linux, and on any bare checkout, the only harness that measures the
/// out-of-process bridge ran **nowhere**, including CI. That is the same
/// dark-code shape `just check-features` exists to close, on the subsystem
/// with the largest blast radius.
///
/// **Read the resulting numbers as a bridge measurement, not a plugin
/// one.** The probe is a trivial fixture, so the per-block cost here is the
/// cost of the submit/collect pipeline, the socket and the shm slab, with
/// almost no DSP under it. That is the right quantity for "does hosting
/// out-of-process cost anything" and the wrong one for "how many real plugins
/// fit" — `available_effects()` covers the latter when the plugins are there,
/// and the run prints which of the two it did.
#[cfg(feature = "clap")]
fn load_n_from_probe(
    count: usize,
) -> Option<(Vec<GraphUnit>, Vec<tutti_plugin::handles::PluginHandle>)> {
    eprintln!(
        "no third-party effect plugins installed — driving {count} instance(s) \
         of the reference CLAP probe instead"
    );
    // **The probe defaults to `RenderMode::Inert`, which writes nothing.**
    // Left at the default it reports `non-silent 0/N` on a perfectly healthy
    // bridge, and every throughput figure in this file becomes a measurement
    // of the fixture. `TAG_PASSTHROUGH` writes `in + tag`, so the output is
    // both non-silent and *provably derived from the input* — which is the
    // same reason `EFFECTS` names effects rather than synths.
    //
    // Set before the first spawn: the probe reads these on load, in the
    // subprocess, having inherited them. Held across the whole loop so every
    // instance gets it, and dropped after — by then each subprocess has its
    // own copy.
    let _env = clap_probe::ProbeEnv::new().render_mode(clap_probe::render::TAG_PASSTHROUGH);

    let mut units: Vec<GraphUnit> = Vec::with_capacity(count);
    let mut handles = Vec::with_capacity(count);
    for _ in 0..count {
        let probe = clap_probe::load_probe(SAMPLE_RATE);
        let (ins, outs) = (probe.client.inputs(), probe.client.outputs());
        units.push(GraphUnit::new(probe.client.bind(), ins, outs));
        handles.push(probe.handle);
    }
    Some((units, handles))
}

/// Without `clap` there is no probe to fall back to, so absence is still a
/// skip. A `vst3`-only build has no in-tree reference *effect*: the VST3
/// probe is `audio-probe`, which the delay-compensation tests drive directly
/// through [`load_passthrough`].
#[cfg(all(not(feature = "clap"), feature = "vst3"))]
fn load_n_from_probe(
    _count: usize,
) -> Option<(Vec<GraphUnit>, Vec<tutti_plugin::handles::PluginHandle>)> {
    eprintln!("no effect plugins installed and no `clap` feature to fall back on — skipping");
    None
}

/// Load `count` plugin instances, cycling through whatever is installed.
///
/// Returns the units and their handles; the handles must outlive the units
/// (they share the subprocess guard), so the caller keeps both.
///
/// Gated on the formats `EFFECTS` can name: with neither `clap` nor `vst3` every
/// arm of the match below is cfg'd out, leaving nothing to infer the result type
/// from. The delay-compensation tests do not go through here — they load one named
/// plugin via [`load_passthrough`], which is why `au` alone is a valid build.
#[cfg(any(feature = "clap", feature = "vst3"))]
fn load_n(count: usize) -> Option<(Vec<GraphUnit>, Vec<tutti_plugin::handles::PluginHandle>)> {
    let paths = available_effects();
    if paths.is_empty() {
        return load_n_from_probe(count);
    }
    eprintln!("driving {count} instance(s) of installed third-party plugins: {paths:?}");

    let mut units = Vec::with_capacity(count);
    let mut handles = Vec::with_capacity(count);
    for i in 0..count {
        let path = paths[i % paths.len()];
        // `Plugin::open` infers the format from the path, so this does not
        // dispatch on the extension itself. `available_effects` has already
        // filtered to what is installed, and the whole function is cfg'd on the
        // formats `EFFECTS` can name.
        let built = tutti_plugin::catalog::Plugin::open(path, SAMPLE_RATE).map(|plugin| {
            let handle = plugin.handle().clone();
            (GraphUnit::from_plugin(plugin), handle)
        });
        match built {
            Ok((unit, handle)) => {
                units.push(unit);
                handles.push(handle);
            }
            // A plugin that is *installed* but will not load is a failure, not a
            // reason to skip. Skipping here would make every test in this file
            // report `ok` while loading nothing and asserting nothing. "Absent"
            // and "broken" are different answers and only the first is a skip.
            Err(e) => panic!(
                "instance {i} ({path}) is installed but failed to load: {e}\n\
                 This is a real failure. If the plugin is genuinely unavailable, \
                 remove it from EFFECTS rather than letting the suite pass vacuously."
            ),
        }
    }
    Some((units, handles))
}

/// A sine block at full-ish scale, so "audio arrived" is unambiguous.
fn fill_sine(input: &mut Planes, channels: usize, block_index: usize) {
    for ch in 0..channels {
        for i in 0..BLOCK {
            let t = (block_index * BLOCK + i) as f32 / SAMPLE_RATE as f32;
            input.0[ch][i] = (t * 440.0 * std::f32::consts::TAU).sin() * 0.5;
        }
    }
}

#[cfg(any(feature = "clap", feature = "vst3"))]
fn peak(buf: &Planes, channels: usize) -> f32 {
    (0..channels)
        .map(|ch| {
            (0..BLOCK)
                .map(|i| buf.at(ch, i).abs())
                .fold(0.0f32, f32::max)
        })
        .fold(0.0f32, f32::max)
}

/// One plugin's step in one block, as [`StepClock`] measured it.
#[cfg(any(feature = "clap", feature = "vst3"))]
#[cfg_attr(
    not(feature = "clap"),
    allow(dead_code, reason = "only the clap-gated timing tests read a step")
)]
struct Step {
    /// On Linux, wall time less runqueue time; elsewhere, wall time. See
    /// [`StepClock`].
    took: Duration,
    /// Whether the step gave up its core of its own accord: slept, or
    /// blocked in the kernel. Always `false` off Linux, where it is not read.
    blocked: bool,
}

/// What [`drive_series`] measured: each plugin's step in each block, each
/// block's wall time, and whether each plugin's output carried audio.
#[cfg(any(feature = "clap", feature = "vst3"))]
#[cfg_attr(
    not(feature = "clap"),
    allow(dead_code, reason = "only the clap-gated timing tests read a run")
)]
struct Run {
    /// Block-major: `steps[b * plugins + p]` is plugin `p`'s step in block `b`.
    steps: Vec<Step>,
    /// Each block's wall time, from its first step's start to its last
    /// step's end: what the callback would have spent, load included. Not
    /// the steps' sum, which leaves out the time load took from them.
    block_wall: Vec<Duration>,
    /// Block-major, as `steps`: whether that output carried audio.
    audible: Vec<bool>,
    plugins: usize,
    /// From the start of the first block to the end of the last, pacing
    /// included.
    wall: Duration,
}

#[cfg(feature = "clap")]
impl Run {
    fn blocks(&self) -> usize {
        self.block_wall.len()
    }

    fn took(&self) -> impl Iterator<Item = Duration> + '_ {
        self.steps.iter().map(|s| s.took)
    }

    /// The steps of the plugins at `plugins`, every block.
    fn steps_of<'a>(&'a self, plugins: &'a [usize]) -> impl Iterator<Item = &'a Step> + 'a {
        self.steps
            .chunks(self.plugins)
            .flat_map(move |b| plugins.iter().map(move |&p| &b[p]))
    }

    /// How many blocks plugin `p`'s output carried audio in.
    fn heard(&self, p: usize) -> usize {
        self.audible
            .iter()
            .skip(p)
            .step_by(self.plugins)
            .filter(|&&a| a)
            .count()
    }
}

/// Measures one plugin step two ways, on the thread that runs it.
///
/// - **How long it took**, as wall time minus the time this thread spent
///   runnable but not running: what the step itself cost, including any time
///   it slept or blocked, but not the time the machine gave its core to
///   someone else. Read from Linux's per-thread `schedstat` (its second
///   field, `run_delay`, in ns), which the scheduler updates each time the
///   thread gets a core, so a read while running counts every wait before
///   it. What load still adds is the step's own work running slower on a
///   contended core: 2-3x under the bursty load in [`assert_no_waiting`]'s
///   notes.
/// - **Whether it blocked**: whether the thread's voluntary context switches
///   (`getrusage(RUSAGE_THREAD)`'s `ru_nvcsw`) moved across the step. A
///   thread switches voluntarily when it sleeps or blocks in the kernel, and
///   not when it is preempted.
///
/// **On Linux both must be readable, or the test fails** rather than falling
/// back: the blocked-step bound in [`assert_no_waiting`] and the cost bound
/// in [`assert_working_steps_cheap`] are Linux's, and a quiet fallback to
/// wall time would turn them off. `schedstat` needs `CONFIG_SCHED_INFO`,
/// which every distribution kernel (Ubuntu's generic one included, so CI's
/// runners) enables; `RUSAGE_THREAD` is in every Linux since 2.6.26.
///
/// Off Linux the time is wall time and no step counts as blocked; `report`
/// prints which ([`MODE`](Self::MODE)).
#[cfg(any(feature = "clap", feature = "vst3"))]
struct StepClock {
    #[cfg(target_os = "linux")]
    schedstat: std::fs::File,
}

#[cfg(any(feature = "clap", feature = "vst3"))]
#[cfg_attr(
    not(feature = "clap"),
    allow(dead_code, reason = "only the clap-gated timing tests report a mode")
)]
impl StepClock {
    /// Whether [`Step::took`] is on-CPU time and [`Step::blocked`] is read:
    /// on Linux, always (or the clock panics); elsewhere, never.
    const ON_CPU: bool = cfg!(target_os = "linux");

    /// What the clock reads, for `report`.
    const MODE: &'static str = if Self::ON_CPU {
        "on-CPU time (schedstat) + voluntary switches (getrusage)"
    } else {
        "wall time, no blocked signal"
    };

    /// For the calling thread: [`time`](Self::time) must be called on it too.
    ///
    /// # Panics
    ///
    /// On Linux, if `schedstat` or `getrusage(RUSAGE_THREAD)` cannot be read.
    fn for_this_thread() -> Self {
        #[cfg(target_os = "linux")]
        {
            const PATH: &str = "/proc/thread-self/schedstat";
            let schedstat = std::fs::File::open(PATH).unwrap_or_else(|e| {
                panic!(
                    "cannot open {PATH} ({e}): the step clock needs it to tell a \
                     step's own time from the time load took from it. It exists \
                     on any kernel built with CONFIG_SCHED_INFO."
                )
            });
            let clock = Self { schedstat };
            // Both read once here, so an unreadable one fails before any
            // timing rather than inside it.
            clock.run_delay();
            Self::voluntary_switches();
            clock
        }
        #[cfg(not(target_os = "linux"))]
        Self {}
    }

    #[cfg(target_os = "linux")]
    fn run_delay(&self) -> Duration {
        use std::os::unix::fs::FileExt;
        let mut buf = [0u8; 96];
        let n = self
            .schedstat
            .read_at(&mut buf, 0)
            .unwrap_or_else(|e| panic!("reading schedstat failed: {e}"));
        let text = std::str::from_utf8(&buf[..n]).expect("schedstat is ASCII");
        let ns: u64 = text
            .split_whitespace()
            .nth(1)
            .and_then(|f| f.parse().ok())
            .unwrap_or_else(|| panic!("schedstat has no run_delay field: {text:?}"));
        Duration::from_nanos(ns)
    }

    #[cfg(target_os = "linux")]
    fn voluntary_switches() -> i64 {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        // SAFETY: `getrusage` writes the whole struct it is handed, which is
        // live and exclusively borrowed for the call.
        let ok = unsafe { libc::getrusage(libc::RUSAGE_THREAD, usage.as_mut_ptr()) } == 0;
        assert!(
            ok,
            "getrusage(RUSAGE_THREAD) failed: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: it returned 0, so it initialised the struct.
        unsafe { usage.assume_init() }.ru_nvcsw
    }

    /// Run `f` and measure it.
    fn time(&self, f: impl FnOnce()) -> Step {
        #[cfg(target_os = "linux")]
        {
            let switches = Self::voluntary_switches();
            let start = Instant::now();
            let before = self.run_delay();
            f();
            let after = self.run_delay();
            let wall = start.elapsed();
            Step {
                took: wall.saturating_sub(after.saturating_sub(before)),
                blocked: Self::voluntary_switches() > switches,
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let start = Instant::now();
            f();
            Step {
                took: start.elapsed(),
                blocked: false,
            }
        }
    }
}

/// The median of `samples`: the `n / 2`-th smallest.
#[cfg(feature = "clap")]
fn median(samples: impl Iterator<Item = Duration>) -> Duration {
    let mut sorted: Vec<Duration> = samples.collect();
    sorted.sort_unstable();
    sorted[sorted.len() / 2]
}

/// Drive `units` in series for `blocks` blocks at real callback pacing, and
/// time each plugin's step *inside* processing (excluding the pacing sleep).
#[cfg(any(feature = "clap", feature = "vst3"))]
fn drive_series(units: &mut [GraphUnit], blocks: usize) -> Run {
    let mut steps = Vec::with_capacity(blocks * units.len());
    let mut block_wall = Vec::with_capacity(blocks);
    let mut audible = Vec::with_capacity(blocks * units.len());
    let clock = StepClock::for_this_thread();

    let max_ch = units
        .iter()
        .map(|u| u.inputs().max(u.outputs()).max(1))
        .max()
        .unwrap_or(2);
    let mut input = Planes::new(max_ch);
    let mut output = Planes::new(max_ch);

    let start = Instant::now();
    for block in 0..blocks {
        fill_sine(&mut input, max_ch, block);

        // Every unit gets the SAME input and is processed one after another —
        // the arrangement the serial executor produces for parallel plugins on
        // separate tracks, and precisely the one whose per-node waits would
        // sum inside a single callback.
        //
        // Deliberately not chained output-to-input. Chaining looks like the
        // harsher test but measures the wrong thing here: each pipelined stage
        // adds a block of latency, so a signal needs N blocks to traverse N
        // plugins, and every stage's start-up silence propagates. The result is
        // a chain that carries almost nothing while the timing numbers look
        // fine — timing empty work. Same input to each keeps every stage
        // genuinely processing audio, so the costs measured are real.
        //
        // Only the plugin path is timed, one step per plugin, so a descheduling
        // costs one sample rather than the whole block: the copies into and out
        // of each unit's planar buffers, and the editor's collect, are the
        // harness's and happen outside the region.
        for unit in units.iter_mut() {
            unit.stage(BLOCK, &input);
        }
        let block_start = Instant::now();
        for unit in units.iter_mut() {
            steps.push(clock.time(|| unit.run(BLOCK)));
        }
        let cost = block_start.elapsed();
        block_wall.push(cost);
        for unit in units.iter_mut() {
            output.clear();
            unit.read(BLOCK, &mut output);
            audible.push(peak(&output, max_ch) > 0.0);
        }

        // Pace to the real callback rate, minus what processing already took.
        if let Some(rest) = PERIOD.checked_sub(cost) {
            std::thread::sleep(rest);
        }
    }
    Run {
        steps,
        block_wall,
        audible,
        plugins: units.len(),
        wall: start.elapsed(),
    }
}

/// Drive blocks until audio is actually flowing out of the **last** unit,
/// then return.
///
/// A fixed warm-up block count is the wrong tool. Freshly spawned subprocesses
/// need to `dlopen` the plugin, activate it, and fault in the shared pages, and
/// how long that takes scales with instance count and machine load — 32 blocks
/// was ample for one plugin and nowhere near enough for eight (measured: 18 of
/// 500 blocks carried audio, because the run was still starting up).
///
/// Waiting on the condition instead of a guess makes the test independent of
/// both. Returns whether 12 of a run of 16 blocks carried audio before
/// `deadline`.
#[cfg(feature = "clap")]
fn warm_up(units: &mut [GraphUnit], deadline: Duration) -> bool {
    let start = Instant::now();
    let last = units.len() - 1;
    while start.elapsed() < deadline {
        // Short bursts: enough to make progress, short enough to re-check.
        if drive_series(units, 16).heard(last) >= 12 {
            return true;
        }
    }
    false
}

/// Print the run: the clock mode ([`StepClock::MODE`]), each block's wall
/// time as percentiles and deadline misses (reported only: see
/// `eight_real_plugins_half_stalled_never_wait_for_replies` for why), the
/// median step and slowest step per block, how many steps blocked, and how
/// many blocks the last unit's output carried audio in.
#[cfg(feature = "clap")]
fn report(label: &str, run: &Run) {
    let n = run.blocks();
    let mut blocks = run.block_wall.clone();
    blocks.sort_unstable();
    let p50 = blocks[n / 2];
    let p99 = blocks[(n * 99 / 100).min(n - 1)];
    let worst = blocks[n - 1];
    let over = blocks.iter().filter(|c| **c > PERIOD).count();
    let step_p50 = median(run.took());
    let slowest_p50 = median(
        run.steps
            .chunks(run.plugins)
            .map(|b| b.iter().map(|s| s.took).max().unwrap_or_default()),
    );
    let blocked = run.steps.iter().filter(|s| s.blocked).count();
    let heard = run.heard(run.plugins - 1);

    eprintln!(
        "{label} [clock: {}]: block wall p50 {p50:?}  p99 {p99:?}  worst {worst:?}  \
         over-deadline {over}/{n}  step p50 {step_p50:?}  \
         slowest-step p50 {slowest_p50:?}  blocked steps {blocked}/{}  \
         last unit non-silent {heard}/{n}",
        StepClock::MODE,
        run.steps.len()
    );
}

/// What a synchronous design spent per plugin per block: half the block period
/// at 64 frames / 48 kHz, waiting for a reply before giving up.
#[cfg(feature = "clap")]
const SYNC_WAIT_BUDGET: Duration = Duration::from_micros(667);

/// The most blocked steps a run of `steps` may hold on Linux:
/// `max(2, steps / 200)`.
///
/// Correct code blocked on 0 steps in every run measured, idle and under the
/// bursty load below, so this is slack for the odd page fault that waits on
/// I/O, not for a wait. A wait on one stalled plugin in every 32nd collect
/// blocks on 6 of 200 steps at one plugin, and 125 of 4000 at eight: both
/// over it.
#[cfg(feature = "clap")]
fn most_blocked(steps: usize) -> usize {
    (steps / 200).max(2)
}

/// Assert that `run` shows no step waiting for a stalled plugin's reply.
///
/// Two rules, one per signal [`StepClock`] reads.
///
/// **Blocked steps (Linux): at most [`most_blocked`]**, a handful. A wait
/// that sleeps, polls with sleeps, or blocks on the reply gives up its core
/// at least once in a step whose reply is not there, however short the wait
/// and however busy the machine. The correct path never does: 0 blocked
/// steps in every run, idle and under bursty load (four busy-sleep spinners,
/// 2 ms on and 0.3 ms off, pinned to the test's core, the test at nice 19).
/// So a bound this tight catches a wait on a small minority of steps too.
///
/// **Steps that look like a wait: fewer than half**, of each of three
/// populations, where a step looks like a wait if it blocked or took at
/// least [`SYNC_WAIT_BUDGET`]. This is the rule for time, the only signal
/// off Linux and the only one a wait that never blocks (a spin) shows. The
/// populations are those in which a waiting design makes at least half:
///
/// - **every step:** a design that waits on every stalled step, when at
///   least half the plugins are stalled;
/// - **each plugin's steps:** one plugin that waits on every block, such as a
///   per-callback budget spent by whichever plugin runs first;
/// - **each block** (does any of its steps look like one): waits spread
///   thinly across plugins, at least one in most blocks, such as one on every
///   3rd step.
///
/// "Fewer than half" is under `n - n / 2` of `n`, the count at which the
/// median sample is one of them. A minority cannot reach it, so load that
/// lands on a minority of steps cannot fail it. Its limit is the budget
/// itself, because a wait is a lower bound: a step that waits out its budget
/// for a plugin stalled far longer is at or over it on any machine, unless
/// load deschedules it for most of that budget. A correct step's median under
/// the bursty load was at most ~320 us (the slowest step per block, 8
/// plugins), over 2x under it.
///
/// **Why the blocked signal is needed on Linux:** under that load a poll to a
/// wall-clock deadline spends the deadline mostly runnable but not running,
/// which [`StepClock`] subtracts as load. The time-only version of these
/// assertions passed every wait mutation tried under it (every call, every
/// 3rd and every 8th on eight plugins, every call on one), 2 runs of 2 each.
/// Wall time would not do either: the same load stretches correct steps by
/// milliseconds.
///
/// What a plugin that answers in time costs is not this function's: a
/// waiting design never waits for one. [`assert_working_steps_cheap`] bounds
/// that.
///
/// **Not covered:** a spinning wait shorter than [`SYNC_WAIT_BUDGET`], or on
/// a minority of steps, of a plugin's steps and of blocks, or under load that
/// deschedules it; and off Linux, any wait that load hides or that lands on
/// a minority, since only wall time is read there.
#[cfg(feature = "clap")]
fn assert_no_waiting(label: &str, run: &Run) {
    let waited: Vec<bool> = run
        .steps
        .iter()
        .map(|s| s.blocked || s.took >= SYNC_WAIT_BUDGET)
        .collect();
    let blocked = run.steps.iter().filter(|s| s.blocked).count();
    let slow = run
        .steps
        .iter()
        .filter(|s| s.took >= SYNC_WAIT_BUDGET)
        .count();
    let steps = run.steps.len();
    let shape =
        format!("{blocked} of {steps} steps blocked, {slow} took {SYNC_WAIT_BUDGET:?} or more");
    let regressed = "A step that waits for a stalled plugin's reply blocks, or \
                     spends at least the synchronous design's budget, so this \
                     looks like the audio thread is waiting for replies again.";

    if StepClock::ON_CPU {
        let most = most_blocked(steps);
        assert!(
            blocked <= most,
            "{label}: {blocked} of {steps} steps blocked, more than {most} \
             ({shape}). Correct steps never block. {regressed}"
        );
    }

    // At or over `n - n / 2` of `n`, the median sample is one of them.
    let check = |what: &str, count: usize, n: usize| {
        assert!(
            count < n - n / 2,
            "{label}: {count} of {n} {what} looked like a wait ({shape}). {regressed}"
        );
    };
    check("steps", waited.iter().filter(|&&w| w).count(), steps);
    for p in 0..run.plugins {
        let mine = waited.iter().skip(p).step_by(run.plugins);
        check(
            &format!("of plugin {p}'s steps"),
            mine.filter(|&&w| w).count(),
            run.blocks(),
        );
    }
    let blocks = waited
        .chunks(run.plugins)
        .filter(|b| b.iter().any(|&w| w))
        .count();
    check("blocks", blocks, run.blocks());
}

/// The most a step that drives a working plugin may cost, as a median: the
/// ceiling these tests used to assert per plugin, of wall time.
#[cfg(feature = "clap")]
const CEILING_PER_PLUGIN: Duration = Duration::from_micros(200);

/// Assert that the plugins at `working` (ones that answer in time) cost a
/// median step under [`CEILING_PER_PLUGIN`] of on-CPU time.
///
/// This is the bound on a working transfer: submit, collect, the copies
/// through the shm slab. A bridge that got slow without ever waiting (a
/// spin, a copy gone quadratic) shows only here, since
/// [`assert_no_waiting`] judges against a far larger budget.
///
/// Measured, idle and under the bursty load in [`assert_no_waiting`]'s notes:
/// over 10 runs each, a working step's median was 43-47 us idle and
/// 95-106 us under load in the eight-plugin test, and 46-69 us idle and
/// 66-148 us under load in [`working_plugins_cost_under_the_old_ceiling`].
/// A spin of 300 us per collect puts it at 352-364 us idle. An earlier version bounded the *block*
/// median at 200 us per plugin of wall time, and failed on unchanged code at
/// a load average of 17 (239 us per plugin): on-CPU time takes out what load
/// took, which is what lets the old ceiling stand.
///
/// **Linux only** ([`StepClock::ON_CPU`]). Off Linux the step clock reads
/// wall time, and a wall-time bound loose enough not to fail on a loaded
/// machine is several times the ceiling, too loose to catch a step that
/// costs 2x what it should; the figure is still printed by `report`. No CI
/// job runs this suite off Linux.
#[cfg(feature = "clap")]
fn assert_working_steps_cheap(label: &str, run: &Run, working: &[usize]) {
    if !StepClock::ON_CPU {
        eprintln!("{label}: working-step cost not asserted: the clock is wall time here");
        return;
    }
    let mut took: Vec<Duration> = run.steps_of(working).map(|s| s.took).collect();
    took.sort_unstable();
    let p50 = took[took.len() / 2];
    eprintln!(
        "{label}: working steps p10 {:?}  p25 {:?}  p50 {p50:?}  p75 {:?}",
        took[took.len() / 10],
        took[took.len() / 4],
        took[took.len() * 3 / 4]
    );
    assert!(
        p50 < CEILING_PER_PLUGIN,
        "{label}: the median step of a plugin that answers in time took {p50:?} \
         of on-CPU time, at or over {CEILING_PER_PLUGIN:?}. Load is not counted \
         in that figure, so the bridge itself got slower."
    );
}

/// The audio floor for a bridge that delivers most blocks, in percent: see
/// [`assert_heard`].
#[cfg(feature = "clap")]
const DELIVERS: usize = 70;

/// The audio floor for a bridge that works at all, in percent: see
/// [`assert_heard`].
#[cfg(feature = "clap")]
const WORKS: usize = 50;

/// Assert that each plugin at `working` carried audio in at least `floor`
/// of the blocks: `heard >= blocks * floor`, floor given in percent.
///
/// [`DELIVERS`] (70%) asserts a bridge that delivers most blocks: one that
/// drops one block in three carries audio in at most two thirds, 333 of
/// 500, and fails. Half would not catch that, so the floor sits between it
/// and what correct code measured over 10 runs in the eight-plugin test:
/// 493-500 of 500 idle, 413-451 (83-90%) under the bursty load.
/// [`WORKS`] (half) asserts only that the steps were working transfers, for
/// [`working_plugins_cost_under_the_old_ceiling`], whose answering plugins
/// carried audio in 146-174 of 200 under that load, too close to 70%.
///
/// `warmed` is what [`warm_up`] returned. A broken bridge never warms up, so
/// it spends the whole warm-up deadline first, and the message says so.
#[cfg(feature = "clap")]
fn assert_heard(label: &str, run: &Run, working: &[usize], warmed: bool, floor: usize) {
    let n = run.blocks();
    for &p in working {
        let heard = run.heard(p);
        assert!(
            heard * 100 >= n * floor,
            "{label}: answering plugin {p} carried audio in only {heard} of {n} \
             blocks, under {floor}%. {}Either the bridge is not delivering (it \
             drops blocks, or the server is starved), or, on the reference \
             probe, it is rendering `Inert`, which writes nothing.",
            if warmed {
                ""
            } else {
                "Warm-up never reached 12 of 16 blocks either, and spent its whole \
                 deadline trying. "
            }
        );
    }
}

/// How late a stalled probe answers every block: far beyond any reply budget
/// (the bridge's is four block periods, 5.3 ms; the synchronous design's was
/// [`SYNC_WAIT_BUDGET`]), so the plugin never publishes in time and a design
/// that waits for it waits its whole budget. A missed budget costs the block,
/// not the connection (see `dispatch`'s reply timeout), so the probe stays
/// live throughout.
///
/// Far beyond the *block* too, and by more than a little: a stalled plugin
/// that publishes in time for the next block is not stalled for it. With
/// 10 ms, bursty load on the test's core stretched the paced blocks enough
/// that one stalled probe carried audio in 116 of 200, and a waiting design
/// pays nothing on those.
#[cfg(feature = "clap")]
const STALL: Duration = Duration::from_millis(500);

/// `count` instances of the reference CLAP probe, each stalled by [`STALL`]
/// on every block: alive, answering, and always late.
///
/// Stalled by a sleep (`stall_every_block`), not parked with `block_from`:
/// a parked probe spins, `plugin-server` runs its audio thread at raised
/// priority (nice -20 here), and eight spinning servers would take the host's
/// cores, which is the time these tests measure.
///
/// Probes rather than installed plugins even where those exist: a third-party
/// plugin cannot be told to stall.
#[cfg(feature = "clap")]
fn load_stalled(count: usize) -> (Vec<GraphUnit>, Vec<tutti_plugin::handles::PluginHandle>) {
    let _env = clap_probe::ProbeEnv::new()
        .render_mode(clap_probe::render::TAG_PASSTHROUGH)
        .stall_every_block(STALL);
    let mut units: Vec<GraphUnit> = Vec::with_capacity(count);
    let mut handles = Vec::with_capacity(count);
    for _ in 0..count {
        let probe = clap_probe::load_probe(SAMPLE_RATE);
        let (ins, outs) = (probe.client.inputs(), probe.client.outputs());
        units.push(GraphUnit::new(probe.client.bind(), ins, outs));
        handles.push(probe.handle);
    }
    (units, handles)
}

/// `pairs` stalled probes ([`load_stalled`]) and `pairs` answering plugins
/// ([`load_n`]), alternating: stalled at even positions, answering at odd.
#[cfg(feature = "clap")]
fn load_half_stalled(pairs: usize) -> (Vec<GraphUnit>, Vec<tutti_plugin::handles::PluginHandle>) {
    let (stalled, mut handles) = load_stalled(pairs);
    let Some((answering, answering_handles)) = load_n(pairs) else {
        unreachable!("with `clap` on, `load_n` falls back to the probe");
    };
    handles.extend(answering_handles);
    let units = stalled
        .into_iter()
        .zip(answering)
        .flat_map(|(s, a)| [s, a])
        .collect();
    (units, handles)
}

/// Assert that the plugins at `stalled` really were stalled for `run`, so
/// that [`assert_no_waiting`] measured what it claims to.
///
/// A stalled probe sleeps [`STALL`] in every `process()` and its server
/// handles one block at a time, so over the run's wall time it can publish at
/// most `wall / STALL` blocks, plus one in flight at each end. The bound holds
/// however slow the machine is, and bites wherever the run is short against
/// the stall: 200 paced blocks take ~0.3 s idle, which allows 2.
#[cfg(feature = "clap")]
fn assert_stalled(label: &str, run: &Run, stalled: impl Iterator<Item = usize>) {
    let most = (run.wall.as_nanos() / STALL.as_nanos()) as usize + 2;
    for p in stalled {
        let heard = run.heard(p);
        assert!(
            heard <= most,
            "{label}: stalled plugin {p} carried audio in {heard} of {} blocks, \
             more than a {STALL:?} stall allows in {:?} ({most}). The stall is \
             not reaching the probe, so the timings above are of plugins that \
             answer in time, which a waiting design never waits for.",
            run.blocks(),
            run.wall
        );
    }
}

/// **Eight real plugin subprocesses, four of them stalled, cost the audio
/// thread no waiting**, over 500 blocks at true callback pacing.
///
/// Stalled and answering instances alternate, starting with a stalled one and
/// ending with an answering one. The answering four are installed effects
/// where present, else the reference probe; the stalled four are always the
/// probe (see [`load_stalled`]).
///
/// # What this test asserts, and what it only reports
///
/// It asserts that each answering plugin carried audio in at least 70% of
/// the blocks ([`assert_heard`]), that their median step stayed under the old
/// per-plugin ceiling of on-CPU time ([`assert_working_steps_cheap`]), that
/// the stalled ones were stalled ([`assert_stalled`]), and that no step
/// waited for a stalled one ([`assert_no_waiting`]).
///
/// It only **reports** each block's wall time against the callback deadline.
/// That used to be asserted (the median block under [`PERIOD`]) and could
/// not catch a waiting design: with every plugin answering in time, a
/// mutation that waits on every step paid nothing and passed it, at a
/// 1.27 ms median block. Under bursty load it failed on unchanged code, every
/// run: the host's own work on a contended core sums past the period. It
/// measures the machine. For the same reason [`warm_up`] is a best effort
/// here, not an assertion: it wants 12 of 16 blocks, and under that load
/// gave up after 10 s in 3 runs of 12. The audio floor was 90% for the same
/// era and failed under that load too (the servers answered in time in
/// ~65-80% of blocks then). 70% is what it is now: low enough for the 82-90%
/// measured under that load since, high enough to catch a bridge that drops
/// one block in three.
///
/// A synchronous design makes this arrangement hopeless: four stalled plugins
/// x 667 us of budget each = 2.7 ms against a 1.33 ms deadline, on every
/// block.
///
/// Mutations. The waits go in `Batcher::collect`, after `await_output`:
/// poll `has_output(expect_seq)` every 50 us until a 667 us deadline, on
/// calls chosen by a counter shared by every batcher in the process. Each
/// failed 2 runs of 2 idle, and the four marked *load* 1 run of 1 under the
/// bursty load in [`assert_no_waiting`]'s notes.
/// - Wait on every call: 2000 of 4000 steps blocked (the stalled half) →
///   fails the blocked bound (20). *Load:* fails first at the working-step
///   bound, a 208 us median (the waits hand the shared core to the servers),
///   and at 1 stalled instance 199 of 200 blocked, of which only 5 took the
///   budget: blocking is what shows the wait there.
/// - Every 3rd: 666 blocked; every 4th: 1000; every 8th: 499-500; every
///   16th: 250; every 32nd (one stalled plugin, every 4th block): 125 →
///   each fails the blocked bound. Before the bound, a wait on a minority
///   like the every-32nd one passed. *Load,* every 32nd: 124 blocked, of
///   which 50 took the budget.
/// - Spin 300 us in every collect, no wait: the answering steps' median is
///   352-355 us of on-CPU time → fails [`assert_working_steps_cheap`].
///   *Load:* 379 us.
/// - Drop every 3rd collected block (count only the collectable ones, render
///   silence): answering plugin 1 carries audio in 328-334 of 500 → fails
///   [`assert_heard`]. *Load:* 309 of 500.
/// - `stall_every_block` dropped from [`load_stalled`]: every plugin answers
///   in time and carries audio in 474-499 of 500 blocks → fails
///   [`assert_stalled`] (recorded before this revision; that path is
///   unchanged).
/// - `TAG_PASSTHROUGH` dropped from [`load_n_from_probe`] (the probe renders
///   `Inert`, nothing): the answering plugins carry audio in 0 of 500 blocks
///   → fails [`assert_heard`] (likewise).
///
/// The starvation property, that a starved block yields silence rather than
/// the input echoed back, is asserted separately in
/// [`starving_the_subprocesses_yields_silence_not_input_echo`].
#[cfg(feature = "clap")]
#[test]
fn eight_real_plugins_half_stalled_never_wait_for_replies() {
    let _machine = exclusive();
    let label = "8 plugins, 4 stalled";
    let (mut units, _handles) = load_half_stalled(4);

    let warmed = warm_up(&mut units, Duration::from_secs(10));
    if !warmed {
        eprintln!("{label}: still warming up after 10 s; measuring anyway");
    }
    let run = drive_series(&mut units, 500);
    report(label, &run);

    let answering: Vec<usize> = (1..run.plugins).step_by(2).collect();
    assert_heard(label, &run, &answering, warmed, DELIVERS);
    assert_stalled(label, &run, (0..run.plugins).step_by(2));
    assert_working_steps_cheap(label, &run, &answering);
    assert_no_waiting(label, &run);
}

/// **Stalled real plugins cost the audio thread no waiting**, at 1, 2, 4 and
/// 8 instances, every one stalled.
///
/// The counts are what [`assert_no_waiting`]'s three populations need
/// between them: with every plugin stalled, a wait on every step owns all
/// three, one plugin's waits own that plugin's, and waits spread one or more
/// to a block own the blocks. Fewer plugins also mean fewer servers, so a
/// correct step is measured on a quieter machine.
///
/// No plugin here answers in time, so this says nothing about what a working
/// step costs: [`working_plugins_cost_under_the_old_ceiling`] does.
///
/// Mutations, as in [`eight_real_plugins_half_stalled_never_wait_for_replies`],
/// 2 runs of 2 idle, each failing at 1 instance on the blocked bound
/// (at most 2 of 200): every call 200 blocked, every 3rd 66, every 4th 50,
/// every 8th 25, every 16th 13, every 32nd 6. Before that bound, the
/// every-8th one passed at 1 and 2 instances and failed only at 4, and the
/// every-32nd one passed outright. The spin and drop mutations do not fail
/// here, and are not meant to: no plugin answers in time.
/// `stall_every_block` dropped: fails [`assert_stalled`] at 1 instance,
/// audio in 198-199 of 200 blocks (recorded before this revision).
#[cfg(feature = "clap")]
#[test]
fn stalled_plugins_cost_no_waiting_at_any_count() {
    let _machine = exclusive();

    for count in [1usize, 2, 4, 8] {
        let label = format!("{count} stalled plugin(s)");
        let (mut units, _handles) = load_stalled(count);
        // Let the previous iteration's subprocesses finish exiting before
        // measuring this one; otherwise the two overlap and starve each other.
        std::thread::sleep(Duration::from_millis(300));
        // Nothing to wait for: a stalled plugin never warms up. This only
        // gets past the first blocks' one-off costs.
        drive_series(&mut units, 16);
        let run = drive_series(&mut units, 200);
        report(&label, &run);

        assert_stalled(&label, &run, 0..count);
        assert_no_waiting(&label, &run);
    }
}

/// **A working step costs under the old per-plugin ceiling**, at 1, 2 and 4
/// answering instances, each after a stalled one, as in
/// [`eight_real_plugins_half_stalled_never_wait_for_replies`].
///
/// The bound the stalled-only test cannot give: what driving a plugin that
/// answers costs the audio thread, as on-CPU time
/// ([`assert_working_steps_cheap`]), with each answering plugin carrying
/// audio in at least half the blocks ([`assert_heard`]) so the steps
/// measured are working transfers and not a bridge that returns early with
/// silence.
///
/// **Why each answering plugin follows a stalled one.** A plugin that
/// answers wakes its server, and when that server shares the host's core
/// (the bursty load pins both to one) it runs during the host's *next* step
/// and that step pays for it in cache and switches, though not in runqueue
/// time. With every plugin answering, every step is such a next step: under
/// the load, a working step's median was 204-213 us at 1, 2 and 4 instances
/// (100, 75 and 65 us idle), against 66-148 us over 10 runs when each
/// follows a stalled probe, whose server sleeps through the block. That is the machine, not
/// the bridge, so the arrangement keeps it out of the figure.
///
/// Mutation: spin 300 us in every collect gives a working median of
/// 362-364 us at 1 pair → fails [`assert_working_steps_cheap`], 2 runs of 2
/// idle. Dropping every 3rd collected block (133-134 of 200) passes this
/// floor and fails the eight-plugin test's; the wait mutations pass here
/// too, since they wait only for a stalled plugin, whose steps this test
/// does not bound.
#[cfg(feature = "clap")]
#[test]
fn working_plugins_cost_under_the_old_ceiling() {
    let _machine = exclusive();

    for pairs in [1usize, 2, 4] {
        let label = format!("{pairs} answering after {pairs} stalled");
        let (mut units, _handles) = load_half_stalled(pairs);
        // As in the stalled test: let the last iteration's servers exit.
        std::thread::sleep(Duration::from_millis(300));
        let warmed = warm_up(&mut units, Duration::from_secs(10));
        let run = drive_series(&mut units, 200);
        report(&label, &run);

        let answering: Vec<usize> = (1..run.plugins).step_by(2).collect();
        assert_heard(&label, &run, &answering, warmed, WORKS);
        assert_stalled(&label, &run, (0..run.plugins).step_by(2));
        assert_working_steps_cheap(&label, &run, &answering);
    }
}

/// **`plugin-server` raises its audio thread, where the OS lets it.**
///
/// How many blocks carry audio under load depends on `raise_to_realtime`
/// (see the module docs), but no audio floor loose enough to hold under load
/// can tell a raised server from an unraised one. So this reads the server's
/// audio thread directly: its scheduling policy, realtime priority and nice
/// level from `/proc/<pid>/task/<tid>/stat`. The audio phase runs on the
/// server's main thread (`PluginServer::run`, called from `main`), whose tid
/// is the pid.
///
/// What "raised" means is derived, not restated: a scratch thread of this
/// process makes the same call `raise_to_realtime` makes
/// (`set_current_thread_priority(ThreadPriority::Max)`), under the same
/// limits the server inherits from it, and the server must match what it
/// got. Where the OS refuses (an unprivileged CI runner: `RLIMIT_NICE` 0 and
/// no `CAP_SYS_NICE`), both stay where they started, and this checks only
/// that the server did not end up anywhere else; it says so.
///
/// Mutation: `raise_to_realtime()` commented out of `PluginServer::run`.
/// As root, the server reads `(0, 0, 0)` against the scratch thread's
/// `(0, 0, -20)` and this fails, 2 runs of 2. With `CAP_SYS_NICE` dropped
/// (`setpriv --inh-caps=-sys_nice --bounding-set=-sys_nice`, `RLIMIT_NICE`
/// 0, CI's case) both read `(0, 0, 0)` and it passes, saying the OS
/// refused.
#[cfg(all(feature = "clap", target_os = "linux"))]
#[test]
fn plugin_server_audio_thread_is_raised_where_the_os_allows() {
    /// `(policy, rt_priority, nice)` from a `/proc/.../stat` line.
    fn sched(path: &str) -> (i64, i64, i64) {
        let stat = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
        // `comm` is in parentheses and may hold spaces: count from after it.
        let rest = &stat[stat.rfind(')').expect("stat has a comm") + 2..];
        let fields: Vec<i64> = rest
            .split_whitespace()
            .skip(1) // state
            .map(|f| f.parse().unwrap_or(0))
            .collect();
        // proc(5) numbers from 1, and `rest` starts at field 3 (state):
        // nice is field 19, rt_priority 40, policy 41.
        let field = |n: usize| fields[n - 4];
        (field(41), field(40), field(19))
    }

    let _machine = exclusive();
    let (raised, expected) = std::thread::spawn(|| {
        let raised =
            thread_priority::set_current_thread_priority(thread_priority::ThreadPriority::Max)
                .is_ok();
        (raised, sched("/proc/thread-self/stat"))
    })
    .join()
    .expect("the scratch thread does not panic");

    let Some((mut units, handles)) = load_n(1) else {
        unreachable!("with `clap` on, `load_n` falls back to the probe");
    };
    // Audio flowing means the server is in its audio phase, past the raise.
    assert!(
        warm_up(&mut units, Duration::from_secs(10)),
        "the server never delivered audio, so it may not have reached the \
         audio phase this checks"
    );
    let pid = handles[0]
        .server_pid()
        .expect("an out-of-process plugin has a server pid");
    let server = sched(&format!("/proc/{pid}/task/{pid}/stat"));
    eprintln!(
        "(policy, rt_priority, nice): server {server:?}, a thread that made \
         the same call {expected:?} ({})",
        if raised {
            "the OS allowed the raise"
        } else {
            "the OS refused the raise, so this checks only that the server \
             stayed put too"
        }
    );
    assert_eq!(
        server, expected,
        "plugin-server's audio thread is not scheduled as a thread that \
         called `raise_to_realtime`'s own call is (policy, rt_priority, nice). \
         Is `raise_to_realtime` still called on entry to the audio phase?"
    );
}

/// Starvation must degrade to silence, never to wrong audio.
///
/// Driving with no pacing gives the subprocesses no wall-clock time, so nothing
/// is published and every block reads back silent. That is the designed failure
/// mode — the failure it replaces echoed the host's own input back at unity
/// gain.
///
/// This is the property that most directly guards the original bypass, checked
/// against real subprocesses rather than a mock.
#[cfg(any(feature = "clap", feature = "vst3"))]
#[test]
fn starving_the_subprocesses_yields_silence_not_input_echo() {
    let _machine = exclusive();
    let Some((mut units, _handles)) = load_n(4) else {
        return;
    };

    let max_ch = units
        .iter()
        .map(|u| u.inputs().max(u.outputs()).max(1))
        .max()
        .unwrap_or(2);
    let mut input = Planes::new(max_ch);
    let mut output = Planes::new(max_ch);

    // No sleep anywhere: as fast as the loop will go.
    let mut echoed = 0usize;
    const BLOCKS: usize = 200;
    for block in 0..BLOCKS {
        fill_sine(&mut input, max_ch, block);
        let in_peak = peak(&input, max_ch);

        for unit in units.iter_mut() {
            output.clear();
            unit.process(BLOCK, &input, &mut output);
        }

        // The bypass signature: output identical to the input we just fed in.
        let matches_input = (0..max_ch)
            .all(|ch| (0..BLOCK).all(|i| (output.at(ch, i) - input.at(ch, i)).abs() < 1e-6));
        if matches_input && in_peak > 0.0 {
            echoed += 1;
        }
    }

    assert_eq!(
        echoed, 0,
        "{echoed}/{BLOCKS} starved blocks returned the host's own input at unity \
         gain — that is the out-of-process bypass, reappearing under load"
    );
}

/// Load and drop many plugins in sequence: every subprocess must be reaped.
///
/// Not a timing test — a resource one. A DAW loads and unloads plugins all
/// session; leaking one host process per load would be fatal over hours.
///
/// # What is observed, and why not a process count
///
/// This counted `plugin-server` processes system-wide with `pgrep` and
/// compared before with after. That had two faults:
/// - **It could not fail on Windows.** There is no `pgrep` there, the spawn
///   error fell back to 0, and the leak check compared 0 with 0.
/// - **It read global state.** A neighbouring test binary's healthy server,
///   started between the two counts, was counted as this test's leak.
///   `launch.rs`'s `a_failed_launch_does_not_strand_the_subprocess` documents
///   the same flake.
///
/// Now the test records the pid of every server *it* launched
/// ([`PluginHandle::server_pid`](tutti_plugin::handles::PluginHandle::server_pid))
/// and asks the OS about exactly those. No scan is involved, so no other
/// process's server can be miscounted.
///
/// There is no settling delay either. `ProcessGuard::drop` kills and waits
/// synchronously when the last `Arc` goes, and the unit and the handle are the
/// only holders. So by the time `drop(handles)` returns, each server is already
/// reaped.
///
/// Mutation: `std::mem::forget` one round's `handles` instead of dropping
/// them. That server's guard never runs, and this fails naming its pid on
/// every OS.
#[cfg(any(feature = "clap", feature = "vst3"))]
#[test]
fn repeated_load_and_drop_leaves_no_subprocesses() {
    let _machine = exclusive();
    // No `available_effects()` guard: subprocess teardown is the same code
    // whatever is loaded into it, so the reference probe exercises this
    // exactly as a third-party plugin does — and `load_n` falls back to it.
    // A guard would skip this test on every machine without plugins
    // installed, which is every bare checkout.
    let mut launched = Vec::new();
    for round in 0..8 {
        let Some((mut units, handles)) = load_n(2) else {
            return;
        };
        for handle in &handles {
            launched.push(
                handle
                    .server_pid()
                    .expect("every plugin here is out-of-process, so it has a server pid"),
            );
        }
        drive_series(&mut units, 4);
        drop(units);
        drop(handles);
        eprintln!("round {round}: dropped");
    }

    assert_eq!(launched.len(), 16, "8 rounds of 2 servers each");
    let survivors: Vec<u32> = launched
        .iter()
        .copied()
        .filter(|&pid| clap_probe::is_alive(pid))
        .collect();
    assert!(
        survivors.is_empty(),
        "{} of {} plugin-servers outlived every handle and unit across 8 \
         load/drop rounds — subprocesses are leaking (pids {survivors:?})",
        survivors.len(),
        launched.len(),
    );
}

// ---------------------------------------------------------------------------
// Delay compensation
// ---------------------------------------------------------------------------

/// A plugin that is a **passthrough at its defaults**, which is what the null
/// test needs and what most effects are not.
///
/// TDR Nova is a linear-phase EQ whose bands are all flat until moved, so a
/// freshly loaded instance reproduces its input exactly — and, usefully, does so
/// behind 184 samples of its own declared latency. That is the interesting case:
/// the total it reports is *its* latency plus the pipeline's, so the test
/// exercises a real sum rather than the pipeline constant alone.
///
/// Deliberately no `set_parameter` call. An earlier version of this test used
/// TAL-Reverb-4 with `Wet=0`/`Dry=1`. Those values *do* arrive — verified by
/// reading them back over IPC — but that plugin is an algorithmic reverb whose
/// dry path is not a bit-exact passthrough, so the test measured a reverb tail and
/// could say nothing about latency. A fixture that needs no configuration cannot
/// be misconfigured, and cannot be misdiagnosed either.
#[cfg(feature = "vst3")]
const PASSTHROUGH_VST3: &str = "/Library/Audio/Plug-Ins/VST3/TDR Nova.vst3";

/// The same plugin as an Audio Unit.
///
/// Worth testing separately rather than trusting the VST3 result: it is a
/// different format host, a different loader, and a different parameter/latency
/// path in `tutti-plugin-server`, and AU has had format-specific bugs (NaN
/// output) of its own.
#[cfg(feature = "au")]
const PASSTHROUGH_AU: &str = "/Library/Audio/Plug-Ins/Components/TDR Nova.component";

/// The null test for delay compensation: the output must be the input delayed by
/// *exactly* the latency the plugin declares.
///
/// This is what the whole PDC declaration is for. A parallel dry/wet split routes
/// the same signal down two paths, one through the plugin; the host delays the
/// clean path by the declared latency so the two line up again. If the number is
/// wrong the paths are misaligned by the error and summing them comb-filters —
/// audible, and silent in every other test, because each path on its own is
/// perfectly fine.
///
/// The check is what an engineer does by ear, done numerically: align the legs by
/// the declared latency, invert one, sum, and require the residual to vanish.
/// Nulling is far sharper than comparing peaks. Measured on this fixture: exact
/// alignment gives a residual energy of 0.000000000 over 7176 samples, while a
/// **one-sample** slip gives -24.8 dB. No amplitude assertion would notice the
/// latter.
///
/// Distinct from the pipelining tests: those establish that output lags input by
/// one block. This establishes that the number reported to `latency::plan` is that
/// same lag — a pipeline that worked while declaring 0 would pass every other test
/// in this file.
#[test]
#[ignore]
#[cfg(feature = "vst3")]
fn vst3_output_nulls_against_the_input_delayed_by_the_declared_latency() {
    let _guard = exclusive();
    let Some((unit, _handle)) = load_passthrough(PASSTHROUGH_VST3) else {
        return;
    };
    assert_nulls_at_declared_latency(unit, PASSTHROUGH_VST3);
}

/// The same check through the AU host. Not redundant with the VST3 case: a
/// different loader, a different format host, and a different latency path on the
/// server side — any of which could report a number the compensation does not
/// match.
#[test]
#[ignore]
#[cfg(feature = "au")]
fn au_output_nulls_against_the_input_delayed_by_the_declared_latency() {
    let _guard = exclusive();
    let Some((unit, _handle)) = load_passthrough(PASSTHROUGH_AU) else {
        return;
    };
    assert_nulls_at_declared_latency(unit, PASSTHROUGH_AU);
}

/// Load an installed passthrough plugin.
///
/// Returns `None` only when the plugin is absent — a genuine skip. A plugin that
/// is installed but will not load panics, for the reason `load_n` does.
///
/// `Plugin::open` picks the host from the path itself, so this does not
/// dispatch on the extension.
#[cfg(any(feature = "vst3", feature = "au"))]
fn load_passthrough(path: &str) -> Option<(GraphUnit, tutti_plugin::handles::PluginHandle)> {
    if !std::path::Path::new(path).exists() {
        eprintln!("{path} not installed — skipping");
        return None;
    }
    let built = tutti_plugin::catalog::Plugin::open(path, SAMPLE_RATE).map(|plugin| {
        let handle = plugin.handle().clone();
        (GraphUnit::from_plugin(plugin), handle)
    });
    Some(built.unwrap_or_else(|e| panic!("{path} is installed but failed to load: {e}")))
}

/// Drive `unit` and require its output to null against the input delayed by the
/// latency it declares.
#[cfg(any(feature = "vst3", feature = "au"))]
fn assert_nulls_at_declared_latency(mut unit: GraphUnit, path: &str) {
    let unit = &mut unit;
    // Wide enough for *both* directions: one width sizes both `Planes`, and
    // `stage`/`read` index each by channel up to its own side, so sizing to the
    // narrower one panics.
    let channels = unit.inputs().max(unit.outputs()).max(1);

    let declared = unit
        .latency()
        .expect("an out-of-process plugin must declare its pipeline latency to PDC")
        as usize;
    assert!(
        declared > 0,
        "a pipelined plugin declaring zero latency is the failure this test exists \
         for: every other test still passes, and a dry/wet split comb-filters"
    );

    // Drive well past the declared latency so the comparison happens in steady
    // state rather than across the start-up transient. This fixture declares 248
    // samples (its own 184 plus the pipeline's 64), so 40 blocks would leave
    // almost nothing to compare — enough to make an empty comparison look like a
    // perfect null.
    let blocks = 120;
    let mut sent: Vec<Vec<f32>> = Vec::with_capacity(blocks);
    let mut got: Vec<Vec<f32>> = Vec::with_capacity(blocks);

    let mut input = Planes::new(channels);
    let mut output = Planes::new(channels);

    for b in 0..blocks {
        fill_sine(&mut input, channels, b);
        sent.push((0..BLOCK).map(|i| input.at(0, i)).collect());

        let start = Instant::now();
        unit.process(BLOCK, &input, &mut output);
        got.push((0..BLOCK).map(|i| output.at(0, i)).collect());

        // Real callback pacing: without it the subprocess never runs and every
        // block reads back silent (see the module doc).
        if let Some(rest) = PERIOD.checked_sub(start.elapsed()) {
            std::thread::sleep(rest);
        }
    }

    // Flatten to sample streams so the delay is expressible in samples, not
    // blocks — the declared latency need not be a whole block.
    let dry: Vec<f32> = sent.concat();
    let wet: Vec<f32> = got.concat();

    // Compare only the steady-state tail, and only where the plugin actually
    // produced audio: a block whose reply missed its window is silence by design
    // (see `starving_the_subprocesses_yields_silence_not_input_echo`) and is a
    // scheduling artefact, not a latency error.
    let skip = declared + BLOCK * 4;
    let mut compared = 0usize;
    let mut worst_residual = 0.0f32;
    let mut signal_energy = 0.0f64;
    let mut residual_energy = 0.0f64;

    for i in skip..wet.len() {
        let expected = dry[i - declared];
        let actual = wet[i];
        // Skip the silent stretches left by dropped blocks.
        if actual == 0.0 && expected.abs() > 0.01 {
            continue;
        }
        let residual = actual - expected;
        worst_residual = worst_residual.max(residual.abs());
        signal_energy += (expected as f64) * (expected as f64);
        residual_energy += (residual as f64) * (residual as f64);
        compared += 1;
    }

    // Scale the floor with the offset, not a constant: at a 248-sample latency a
    // short run leaves few overlapping samples, and comparing almost nothing
    // yields a residual of zero — a perfect-looking null that means the test did
    // not run. This bit me while characterising the fixture.
    assert!(
        compared > BLOCK * 8,
        "too few live samples to judge ({compared} at offset {declared}); either the \
         plugin was starved or the run is too short for this latency — a near-empty \
         comparison nulls perfectly and proves nothing"
    );

    // Null depth in dB. A correct alignment leaves only the plugin's own
    // arithmetic error; a one-sample slip on this 440 Hz sine leaves ~-24 dB.
    let null_db = 10.0 * (residual_energy / signal_energy.max(f64::MIN_POSITIVE)).log10();
    println!(
        "null: {null_db:.1} dB over {compared} samples (worst residual {worst_residual:.6}, \
         declared latency {declared})"
    );

    if null_db >= -60.0 {
        // Say which of the two failures this is before blaming either. Search
        // every plausible offset for the deepest null: if some *other* offset
        // nulls, the signal is passing through cleanly and the declared latency is
        // simply wrong — the PDC bug this test exists to catch. If nothing nulls
        // anywhere the fixture has stopped being a passthrough (a preset, a plugin
        // update), which says nothing about PDC and should not be reported as if
        // it did.
        let (best_offset, best_db) = best_null_offset(&dry, &wet, declared * 2);
        assert!(
            best_db >= -60.0,
            "DECLARED LATENCY IS WRONG: the output nulls at {best_offset} samples \
             ({best_db:.1} dB) but the plugin declares {declared}. Any parallel \
             dry/wet split will comb-filter by the {} sample difference.",
            (best_offset as isize - declared as isize).abs()
        );
        panic!(
            "nothing nulls at any offset (best {best_db:.1} dB at {best_offset}). \
             This is not a PDC verdict, but it is not necessarily the fixture's \
             fault either — two causes produce it identically:\n\
             \x20 1. {path} stopped reproducing its input (a loaded preset, a \
             plugin update), so there is no matching signal to null against; or\n\
             \x20 2. the host stopped delivering the plugin's output — a \
             regression in the collect path (the sequence check, the ring slot \
             bound) yields silence every block, and silence nulls nowhere.\n\
             Check whether the wet signal is non-silent to tell them apart: if it \
             is silent, the fixture is fine and the bug is host-side."
        );
    }
}

/// Find the delay offset at which `wet` best nulls against `dry`, and how deep
/// that null is in dB.
///
/// Used only to *diagnose* a failure: it separates "the signal passes through but
/// the declared latency is wrong" from "the signal is not passing through", which
/// otherwise look identical from a single failed comparison.
#[cfg(any(feature = "vst3", feature = "au"))]
fn best_null_offset(dry: &[f32], wet: &[f32], search_max: usize) -> (usize, f64) {
    let mut best = (0usize, f64::INFINITY);
    for offset in 0..=search_max.min(wet.len().saturating_sub(1)) {
        let mut sig = 0.0f64;
        let mut res = 0.0f64;
        for i in (offset + BLOCK * 4)..wet.len() {
            let expected = dry[i - offset];
            let actual = wet[i];
            if actual == 0.0 && expected.abs() > 0.01 {
                continue;
            }
            sig += (expected as f64) * (expected as f64);
            res += ((actual - expected) as f64) * ((actual - expected) as f64);
        }
        if sig > 0.0 {
            let db = 10.0 * (res / sig).log10();
            if db < best.1 {
                best = (offset, db);
            }
        }
    }
    best
}
