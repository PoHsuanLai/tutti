//! Pressure test: many real out-of-process plugins, driven at audio-callback
//! timing, measured against the callback deadline.
//!
//! # What this exists to catch
//!
//! The bug this whole change addresses was that per-plugin waits *summed*.
//! fundsp runs nodes serially in one callback, so N stalled plugins cost N x
//! budget, and 3 was enough to overrun 64 frames at 48 kHz. The synthetic
//! `stalled_plugins_do_not_stall_the_audio_thread` test proves the waiting is
//! gone using mock servers; this proves it with **real plugin subprocesses**,
//! which the mock cannot model: real scheduling, real dlopen'd DSP, real
//! shared-memory traffic, real socket round-trips.
//!
//! **These used to be `#[ignore]`d for needing plugins installed, and were
//! therefore run nowhere** — not locally on Linux, not in CI. `EFFECTS` names
//! macOS system paths, so on a bare checkout the only harness measuring the
//! out-of-process bridge executed zero assertions. `load_n` now falls back to
//! the **reference CLAP probe**, which cargo builds as a dev-dependency, so
//! they run anywhere the `clap` feature is on:
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
//! order of magnitude, not the absolute number.
//!
//! These figures depend on the plugin subprocess running at realtime priority
//! (`plugin-server`'s `raise_to_realtime`). Without it the host's callback
//! thread is realtime — the OS audio backend makes it so — but the subprocess
//! it is waiting on is not, and the blocks that arrive in time swung between
//! 29 and 492 of 500 across identical runs. With it: 491-498 of 500.
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
// rather than simply skipping — these tests are opt-in twice over: a feature to
// build them, and `--ignored` to run them.
#![cfg(any(feature = "clap", feature = "vst3", feature = "au"))]

// The reference CLAP probe, so this harness has something to drive on a bare
// checkout. `clap_probe` is the same module the out-of-process CLAP suites
// use; it resolves the cdylib and the `plugin-server` binary that `build.rs`
// located, both of which cargo builds as dev-dependencies.
#[cfg(feature = "clap")]
#[path = "support/clap_probe.rs"]
mod clap_probe;

#[cfg(not(feature = "clap"))]
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tutti_core::{AudioUnit, BufferVec, F32};

/// Take the machine, **across processes**.
///
/// This was a `static Mutex`, and under `cargo nextest` a `static Mutex`
/// serializes nothing: nextest gives every test its own *process*, so a
/// process-local lock is uncontended in each one and the tests run fully
/// parallel anyway. `clap_probe.rs` had already learned this and answered it
/// with a lock directory — `create_dir` is atomic and fails with
/// `AlreadyExists` on every OS this builds for — while this file kept the
/// `Mutex` and a doc describing `cargo test`'s threading model, which is not
/// the model this repo runs under.
///
/// It went unnoticed because every test here was `#[ignore]`d. The first CI
/// run after they were enabled failed exactly there:
/// `repeated_load_and_drop_leaves_no_subprocesses` counts `plugin-server`
/// processes **system-wide**, and its "before" count came back 5 rather than
/// 0 — the three neighbouring tests' subprocesses, live in their own
/// processes. It reported a leak that was a race.
///
/// What still needs serializing is wall clock, which is process-global: each
/// test paces itself to a real block period, so neighbours halve the time each
/// subprocess gets. The process count no longer does. The leak test now
/// probes only the pids it launched itself, so a neighbour's servers are
/// invisible to it.
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
) -> Option<(
    Vec<Box<dyn AudioUnit>>,
    Vec<tutti_plugin::handles::PluginHandle>,
)> {
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

    let mut units: Vec<Box<dyn AudioUnit>> = Vec::with_capacity(count);
    let mut handles = Vec::with_capacity(count);
    for _ in 0..count {
        let probe = clap_probe::load_probe(SAMPLE_RATE);
        units.push(Box::new(probe.client));
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
) -> Option<(
    Vec<Box<dyn AudioUnit>>,
    Vec<tutti_plugin::handles::PluginHandle>,
)> {
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
fn load_n(
    count: usize,
) -> Option<(
    Vec<Box<dyn AudioUnit>>,
    Vec<tutti_plugin::handles::PluginHandle>,
)> {
    let paths = available_effects();
    if paths.is_empty() {
        return load_n_from_probe(count);
    }
    eprintln!("driving {count} instance(s) of installed third-party plugins: {paths:?}");

    let mut units = Vec::with_capacity(count);
    let mut handles = Vec::with_capacity(count);
    for i in 0..count {
        let path = paths[i % paths.len()];
        // `Plugin::open` infers the format from the path, so this no longer
        // dispatches on the extension itself. `available_effects` has already
        // filtered to what is installed, and the whole function is cfg'd on the
        // formats `EFFECTS` can name.
        let built = tutti_plugin::catalog::Plugin::open(path, SAMPLE_RATE)
            .map(|plugin| plugin.into_parts());
        match built {
            Ok((unit, handle)) => {
                units.push(unit);
                handles.push(handle);
            }
            // A plugin that is *installed* but will not load is a failure, not a
            // reason to skip. Skipping here made every test in this file report
            // `ok` while loading nothing and asserting nothing — which is how a
            // server-side regression that broke plugin loading outright went
            // unnoticed through a full run. "Absent" and "broken" are different
            // answers and only the first is a skip.
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
fn fill_sine(input: &mut BufferVec<F32>, channels: usize, block_index: usize) {
    for ch in 0..channels {
        for i in 0..BLOCK {
            let t = (block_index * BLOCK + i) as f32 / SAMPLE_RATE as f32;
            input.set_scalar(ch, i, (t * 440.0 * std::f32::consts::TAU).sin() * 0.5);
        }
    }
}

#[cfg(any(feature = "clap", feature = "vst3"))]
fn peak(buf: &BufferVec<F32>, channels: usize) -> f32 {
    (0..channels)
        .map(|ch| {
            (0..BLOCK)
                .map(|i| buf.at_scalar(ch, i).abs())
                .fold(0.0f32, f32::max)
        })
        .fold(0.0f32, f32::max)
}

/// Drive `units` in series for `blocks` blocks at real callback pacing, and
/// return the per-block wall time spent *inside* processing (excluding the
/// pacing sleep).
#[cfg(any(feature = "clap", feature = "vst3"))]
fn drive_series(units: &mut [Box<dyn AudioUnit>], blocks: usize) -> (Vec<Duration>, usize) {
    let mut costs = Vec::with_capacity(blocks);
    let mut non_silent = 0usize;

    let max_ch = units
        .iter()
        .map(|u| u.inputs().max(u.outputs()).max(1))
        .max()
        .unwrap_or(2);
    let mut input = BufferVec::<F32>::new(max_ch);
    let mut output = BufferVec::<F32>::new(max_ch);

    for block in 0..blocks {
        fill_sine(&mut input, max_ch, block);

        // Every unit gets the SAME input and is processed one after another —
        // the arrangement fundsp produces for parallel plugins on separate
        // tracks, and precisely the one whose per-node waits used to sum inside
        // a single callback.
        //
        // Deliberately not chained output-to-input. Chaining looks like the
        // harsher test but measures the wrong thing here: each pipelined stage
        // adds a block of latency, so a signal needs N blocks to traverse N
        // plugins, and every stage's start-up silence propagates. The result is
        // a chain that carries almost nothing while the timing numbers look
        // fine — timing empty work. Same input to each keeps every stage
        // genuinely processing audio, so the costs measured are real.
        let start = Instant::now();
        for unit in units.iter_mut() {
            output.clear();
            unit.process(BLOCK, &input.buffer_ref(), &mut output.buffer_mut());
        }
        let cost = start.elapsed();
        costs.push(cost);

        if peak(&output, max_ch) > 0.0 {
            non_silent += 1;
        }

        // Pace to the real callback rate, minus what processing already took.
        if let Some(rest) = PERIOD.checked_sub(cost) {
            std::thread::sleep(rest);
        }
    }
    (costs, non_silent)
}

/// Drive blocks until audio is actually flowing, then return.
///
/// A fixed warm-up block count is the wrong tool. Freshly spawned subprocesses
/// need to `dlopen` the plugin, activate it, and fault in the shared pages, and
/// how long that takes scales with instance count and machine load — 32 blocks
/// was ample for one plugin and nowhere near enough for eight (measured: 18 of
/// 500 blocks carried audio, because the run was still starting up).
///
/// Waiting on the condition instead of a guess makes the test independent of
/// both. Returns `false` if audio never appears, so callers can fail with that
/// as the diagnosis rather than reporting meaningless timings.
#[cfg(any(feature = "clap", feature = "vst3"))]
fn warm_up(units: &mut [Box<dyn AudioUnit>], deadline: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        // Short bursts: enough to make progress, short enough to re-check.
        let (_, non_silent) = drive_series(units, 16);
        if non_silent >= 12 {
            return true;
        }
    }
    false
}

#[cfg(any(feature = "clap", feature = "vst3"))]
fn report(label: &str, costs: &[Duration], non_silent: usize) -> Duration {
    let n = costs.len();
    let mut sorted: Vec<Duration> = costs.to_vec();
    sorted.sort();
    let total: Duration = costs.iter().sum();
    let mean = total / n as u32;
    let p50 = sorted[n / 2];
    let p99 = sorted[(n * 99 / 100).min(n - 1)];
    let worst = *sorted.last().unwrap();
    let over = costs.iter().filter(|c| **c > PERIOD).count();

    eprintln!(
        "{label}: mean {mean:?}  p50 {p50:?}  p99 {p99:?}  worst {worst:?}  \
         over-deadline {over}/{n}  non-silent {non_silent}/{n}"
    );
    worst
}

/// **The headline pressure test.** Eight real plugin subprocesses driven for
/// 500 blocks at true callback pacing.
///
/// A synchronous design makes this arrangement hopeless: eight plugins x 667 us
/// of budget each = 5333 us against a 1333 us deadline, a 4x overrun on every
/// block where the subprocesses are slow. Pipelined, the per-block cost is a
/// memcpy plus a queue push, so eight sit far under the deadline.
///
/// # What this test does and does not assert
///
/// It asserts on **cost**, and only reports **throughput**. That split is
/// forced by the machine, not chosen for convenience.
///
/// Eight subprocesses each needing to be scheduled inside a 1.33 ms window is
/// more than a 12-core box reliably delivers under a test harness. Three
/// consecutive runs of this exact code produced 29, 78, and 492 non-silent
/// blocks out of 500 — the same code, the same machine, an order of magnitude
/// apart. Asserting a throughput floor would therefore be asserting on the
/// scheduler's mood.
///
/// The cost figure survives that, and survives it *safely*: a starved block
/// copies no audio, so starvation can only push the per-block cost DOWN. A run
/// that starves cannot fake a passing time — it can only make a real pass look
/// less impressive. So the assertion below is sound in both directions, while a
/// throughput assertion would be sound in neither.
///
/// The genuinely load-bearing correctness property under starvation — that a
/// starved block yields silence rather than the input echoed back — is asserted
/// separately and deterministically in
/// [`starving_the_subprocesses_yields_silence_not_input_echo`].
#[cfg(any(feature = "clap", feature = "vst3"))]
#[test]
fn eight_real_plugins_stay_under_the_callback_deadline() {
    let _machine = exclusive();
    let Some((mut units, _handles)) = load_n(8) else {
        return;
    };
    warm_up(&mut units, Duration::from_secs(5));

    let (costs, non_silent) = drive_series(&mut units, 500);
    report("8 plugins", &costs, non_silent);

    // Now assertable, and only because the subprocess runs at realtime
    // priority *where the OS grants it*. Before `raise_to_realtime` this check
    // swung between 29 and 492 out of 500 on identical code and had to be a
    // printed note; with it, three consecutive runs gave 498/495/495. So it
    // doubles as the regression guard for that call going missing.
    //
    // It holds without realtime priority too, on a machine with cores to
    // spare: a 32-thread box with `RLIMIT_RTPRIO` capped at 0 gives 498/500
    // across 8 probe instances. Realtime priority buys the *guarantee*, not
    // the throughput, so this is not gated on it — a floor that silently
    // stopped being checked wherever the OS says no would stop being checked
    // in most containers, CI included.
    assert!(
        non_silent > costs.len() * 9 / 10,
        "only {non_silent}/{} blocks carried audio across 8 plugins. Either the \
         subprocesses are not getting realtime priority (see \
         `plugin-server`'s `raise_to_realtime`), this machine cannot schedule \
         8 of them within a {PERIOD:?} block, or — if running on the reference \
         probe — it is rendering `Inert`, which writes nothing.",
        costs.len()
    );

    // Judge the TYPICAL block, not the worst.
    //
    // This is a wall-clock measurement on a non-realtime OS with no thread
    // priority, sharing a machine with a compiler: individual blocks get
    // descheduled and land in the milliseconds. Asserting on the max would make
    // this a flake generator that tells us nothing about the design.
    //
    // The median is what distinguishes the two designs, and it does so by an
    // order of magnitude. Eight plugins under the old summing budget could not
    // come in under 8 x 667 us = 5333 us by construction; pipelined, the per-
    // block cost is memcpy plus a queue push. Measured across runs: 187-554 us
    // median for eight, i.e. ~23-70 us each.
    let mut sorted = costs.clone();
    sorted.sort();
    let median = sorted[sorted.len() / 2];
    assert!(
        median < PERIOD,
        "median block cost {median:?} exceeds the {PERIOD:?} deadline across 8 \
         plugins — under the pipelined design this should be dominated by memcpy \
         (~75 us/plugin measured), so this looks like per-plugin waiting returned"
    );
}

/// **Per-plugin cost must stay an order of magnitude below a waiting budget.**
///
/// Cost does grow with plugin count — each plugin is a real memcpy across a real
/// shared-memory region, so it must. Growth is therefore the wrong thing to
/// assert on: a bound like `8x < 1x * 4` fails on correct behaviour.
///
/// What distinguishes the two designs is the *per-plugin* figure:
///
/// - summing budgets: each plugin costs up to its full wait budget, 667 us at
///   64/48k, and that is the number that accumulates;
/// - pipelined: each plugin costs a memcpy and a queue push.
///
/// Measured on this machine (12-core, debug build, TAL-Reverb-4 x N):
///
/// | plugins | median | per-plugin |
/// |---------|--------|------------|
/// | 1       |  51 us |    51 us   |
/// | 2       | 101 us |    51 us   |
/// | 4       | 250 us |    63 us   |
/// | 8       | 610 us |    76 us   |
///
/// Roughly flat at ~50-75 us each — an order of magnitude under 667 us, and
/// nothing like the flat-at-667 signature a regression would show. The 200 us
/// bound below sits between the two regimes with wide margin on both sides.
#[cfg(any(feature = "clap", feature = "vst3"))]
#[test]
fn per_plugin_cost_stays_far_below_the_old_wait_budget() {
    let _machine = exclusive();
    /// A synchronous design's per-plugin wait at 64 frames / 48 kHz: half a
    /// block period. The number the pipelined path exists to avoid paying N
    /// times.
    const OLD_BUDGET_PER_PLUGIN: Duration = Duration::from_micros(667);
    /// Between the two regimes: ~2.5x headroom over the ~75 us measured, and
    /// ~3.3x under the waiting budget. A regression to waiting misses it by far
    /// more than the noise on this measurement.
    const CEILING_PER_PLUGIN: Duration = Duration::from_micros(200);

    for count in [1usize, 2, 4, 8] {
        let Some((mut units, _handles)) = load_n(count) else {
            return;
        };
        // Let the previous iteration's subprocesses finish exiting before
        // measuring this one; otherwise the two overlap and starve each other.
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            warm_up(&mut units, Duration::from_secs(5)),
            "{count} plugins never started passing audio within 5s"
        );
        let (costs, non_silent) = drive_series(&mut units, 200);
        report(&format!("{count} plugin(s)"), &costs, non_silent);

        // `non_silent` is REPORTED, not asserted, and the distinction matters.
        //
        // It is a property of the harness, not the design: how many blocks
        // carry audio depends on whether the previous iteration's subprocesses
        // have finished tearing down while this one starts. Measured across
        // consecutive identical runs it swung 9/200, 116/200, 199/200 — pure
        // machine contention. Asserting on it would make this a coin flip that
        // fails for reasons having nothing to do with pipelining.
        //
        // The timing assertion below is the real one, and it stays valid
        // regardless: a starved block is *cheap* (no audio copied), so
        // starvation can only bias the per-plugin figure DOWNWARD. It cannot
        // manufacture a pass — only make one less impressive.
        if non_silent < costs.len() / 2 {
            eprintln!(
                "  note: {count} plugins only carried audio on {non_silent}/{} \
                 blocks — subprocess contention, timings below are a lower bound",
                costs.len()
            );
        }

        // Median, not mean: one descheduled block on a shared machine drags a
        // mean by milliseconds and says nothing about the design.
        let mut sorted = costs.clone();
        sorted.sort();
        let median = sorted[sorted.len() / 2];
        let per_plugin = median / count as u32;

        assert!(
            per_plugin < CEILING_PER_PLUGIN,
            "{count} plugins: {per_plugin:?} per plugin (median block \
             {median:?}) exceeds {CEILING_PER_PLUGIN:?}. The old synchronous \
             design paid up to {OLD_BUDGET_PER_PLUGIN:?} per plugin and that is \
             what this figure approaching would mean."
        );
    }
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
    let mut input = BufferVec::<F32>::new(max_ch);
    let mut output = BufferVec::<F32>::new(max_ch);

    // No sleep anywhere: as fast as the loop will go.
    let mut echoed = 0usize;
    const BLOCKS: usize = 200;
    for block in 0..BLOCKS {
        fill_sine(&mut input, max_ch, block);
        let in_peak = peak(&input, max_ch);

        for unit in units.iter_mut() {
            output.clear();
            unit.process(BLOCK, &input.buffer_ref(), &mut output.buffer_mut());
        }

        // The bypass signature: output identical to the input we just fed in.
        let matches_input = (0..max_ch).all(|ch| {
            (0..BLOCK).all(|i| (output.at_scalar(ch, i) - input.at_scalar(ch, i)).abs() < 1e-6)
        });
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
    // The guard used to skip this test on every machine without plugins
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
/// path in `tutti-plugin-server`. AU had no real-plugin coverage at all before
/// this, despite an AU-specific NaN bug being one of the audit's findings.
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
fn load_passthrough(
    path: &str,
) -> Option<(Box<dyn AudioUnit>, tutti_plugin::handles::PluginHandle)> {
    if !std::path::Path::new(path).exists() {
        eprintln!("{path} not installed — skipping");
        return None;
    }
    let built =
        tutti_plugin::catalog::Plugin::open(path, SAMPLE_RATE).map(|plugin| plugin.into_parts());
    Some(built.unwrap_or_else(|e| panic!("{path} is installed but failed to load: {e}")))
}

/// Drive `unit` and require its output to null against the input delayed by the
/// latency it declares.
#[cfg(any(feature = "vst3", feature = "au"))]
fn assert_nulls_at_declared_latency(mut unit: Box<dyn AudioUnit>, path: &str) {
    let unit = &mut unit;
    // Wide enough for *both* directions: fundsp indexes one buffer by channel for
    // whichever side is wider, so sizing to the narrower one panics.
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

    let mut input = BufferVec::new(channels);
    let mut output = BufferVec::new(channels);

    for b in 0..blocks {
        fill_sine(&mut input, channels, b);
        sent.push((0..BLOCK).map(|i| input.at_scalar(0, i)).collect());

        let start = Instant::now();
        unit.process(BLOCK, &input.buffer_ref(), &mut output.buffer_mut());
        got.push((0..BLOCK).map(|i| output.at_scalar(0, i)).collect());

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
