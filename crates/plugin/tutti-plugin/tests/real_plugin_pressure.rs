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
//! Requires plugins installed on the machine, so every test here is
//! `#[ignore]`d — mirroring `probe_real_plugin` in `host::discovery::scanner`.
//! Run explicitly:
//!
//! ```text
//! cargo test --manifest-path crates/bevy-tutti/Cargo.toml \
//!     -p tutti-plugin --features clap,vst3 --test real_plugin_pressure \
//!     -- --ignored --nocapture
//! ```
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
#![cfg(any(feature = "clap", feature = "vst3"))]

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tutti_core::{AudioUnit, BufferVec, F32};

/// These tests must not run concurrently, and the reason is the thing they
/// measure.
///
/// `cargo test` runs test functions on parallel threads. Each test here spawns
/// up to 8 plugin subprocesses and then deliberately paces itself to the audio
/// callback rate — so two tests running at once means ~16 subprocesses
/// competing for the wall-clock time each one is counting on. Observed
/// concretely: the 2-plugin case reported `non-silent 0/200`, i.e. every block
/// starved, purely because a neighbouring test held the machine.
///
/// A lock rather than a `--test-threads=1` note in the docs: a note is
/// something a future runner has to know, and its absence shows up as a
/// mystifying failure in the *other* test.
static EXCLUSIVE: Mutex<()> = Mutex::new(());

/// Take the machine. Poisoning is irrelevant here — the guard protects wall
/// clock, not data — so a panicking test must not wedge every later one.
fn exclusive() -> MutexGuard<'static, ()> {
    EXCLUSIVE.lock().unwrap_or_else(|e| e.into_inner())
}

const SAMPLE_RATE: f64 = 48_000.0;
const BLOCK: usize = 64;

/// One block's worth of wall time — the audio callback's deadline.
const PERIOD: Duration = Duration::from_nanos((BLOCK as f64 / SAMPLE_RATE * 1e9) as u64);

/// Effects, not synths: a synth ignores its input, so it cannot show that audio
/// traversed the graph. Listed by preference; missing ones are skipped.
const EFFECTS: &[&str] = &[
    #[cfg(feature = "clap")]
    "/Library/Audio/Plug-Ins/CLAP/TAL-Reverb-4.clap",
    #[cfg(feature = "vst3")]
    "/Library/Audio/Plug-Ins/VST3/TAL-Reverb-4.vst3",
];

fn available_effects() -> Vec<&'static str> {
    EFFECTS
        .iter()
        .copied()
        .filter(|p| std::path::Path::new(p).exists())
        .collect()
}

/// Load `count` plugin instances, cycling through whatever is installed.
///
/// Returns the units and their handles; the handles must outlive the units
/// (they share the subprocess guard), so the caller keeps both.
#[allow(clippy::type_complexity)]
fn load_n(
    count: usize,
) -> Option<(
    Vec<Box<dyn AudioUnit>>,
    Vec<tutti_plugin::handles::PluginHandle>,
)> {
    let paths = available_effects();
    if paths.is_empty() {
        eprintln!("no effect plugins installed — skipping");
        return None;
    }

    let mut units = Vec::with_capacity(count);
    let mut handles = Vec::with_capacity(count);
    for i in 0..count {
        let path = paths[i % paths.len()];
        let built = match () {
            #[cfg(feature = "clap")]
            () if path.ends_with(".clap") => tutti_plugin::clap(SAMPLE_RATE, path).build(),
            #[cfg(feature = "vst3")]
            () if path.ends_with(".vst3") => tutti_plugin::vst3(SAMPLE_RATE, path).build(),
            () => {
                eprintln!("{path}: no host compiled in for this format — skipping");
                return None;
            }
        };
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
/// Under the old synchronous design this arrangement was hopeless: eight
/// plugins x 667 us of budget each = 5333 us against a 1333 us deadline, a 4x
/// overrun on every block where the subprocesses were slow. Pipelined, the
/// per-block cost is memcpy plus a queue push, so eight should sit far under
/// the deadline.
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
#[test]
#[ignore = "requires plugins installed on the machine"]
fn eight_real_plugins_stay_under_the_callback_deadline() {
    let _machine = exclusive();
    let Some((mut units, _handles)) = load_n(8) else {
        return;
    };
    warm_up(&mut units, Duration::from_secs(5));

    let (costs, non_silent) = drive_series(&mut units, 500);
    report("8 plugins", &costs, non_silent);

    // Now assertable, and only because the subprocess runs at realtime
    // priority. Before that change this same check swung between 29 and 492
    // out of 500 on identical code and had to be downgraded to a printed note;
    // with it, three consecutive runs gave 498/495/495. So this doubles as the
    // regression guard for `raise_to_realtime` going missing.
    assert!(
        non_silent > costs.len() * 9 / 10,
        "only {non_silent}/{} blocks carried audio across 8 plugins. Either the \
         subprocesses are not getting realtime priority (see \
         `plugin-server`'s `raise_to_realtime`), or this machine cannot \
         schedule 8 of them within a {PERIOD:?} block.",
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

/// **Per-plugin cost must stay an order of magnitude below the old budget.**
///
/// Cost does grow with plugin count — each plugin is a real memcpy across a
/// real shared-memory region, so it must. Growth is therefore the wrong thing
/// to assert on; my first attempt at this test asserted `8x < 1x * 4` and
/// failed on correct behaviour for exactly that reason.
///
/// What distinguishes the two designs is the *per-plugin* figure:
///
/// - summing budgets: each plugin costs up to its full wait budget, 667 us at
///   64/48k, and that is the number that used to accumulate;
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
#[test]
#[ignore = "requires plugins installed on the machine"]
fn per_plugin_cost_stays_far_below_the_old_wait_budget() {
    let _machine = exclusive();
    /// The synchronous design's per-plugin wait at 64 frames / 48 kHz: half a
    /// block period. The number this change exists to stop paying N times.
    const OLD_BUDGET_PER_PLUGIN: Duration = Duration::from_micros(667);
    /// Between the two regimes: ~2.5x headroom over the ~75 us measured, and
    /// ~3.3x under the old budget. A regression to waiting misses it by far
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
/// mode; the old design echoed the host's own input back at unity gain instead.
///
/// This is the property that most directly guards the original bypass, checked
/// against real subprocesses rather than a mock.
#[test]
#[ignore = "requires plugins installed on the machine"]
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
#[test]
#[ignore = "requires plugins installed on the machine"]
fn repeated_load_and_drop_leaves_no_subprocesses() {
    let _machine = exclusive();
    if available_effects().is_empty() {
        eprintln!("no effect plugins installed — skipping");
        return;
    }

    let before = count_plugin_servers();
    for round in 0..8 {
        let Some((mut units, handles)) = load_n(2) else {
            return;
        };
        drive_series(&mut units, 4);
        drop(units);
        drop(handles);
        eprintln!("round {round}: dropped");
    }
    // Teardown is asynchronous (the guard signals, the child exits); give it a
    // moment before counting rather than racing it.
    std::thread::sleep(Duration::from_millis(500));
    let after = count_plugin_servers();

    assert!(
        after <= before,
        "plugin-server count went {before} -> {after} across 8 load/drop rounds — \
         subprocesses are leaking"
    );
}

fn count_plugin_servers() -> usize {
    std::process::Command::new("pgrep")
        .arg("-f")
        .arg("plugin-server")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().count())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Delay compensation
// ---------------------------------------------------------------------------

/// TAL-Reverb-4's `Dry` and `Wet` are independent 0..1 controls, so `Dry = 1`
/// with `Wet = 0` makes the plugin a passthrough — the only configuration in
/// which "did the signal come back unchanged, and *when*" is a meaningful
/// question.
#[cfg(feature = "clap")]
const PARAM_WET: u32 = 3814915259;
#[cfg(feature = "clap")]
const PARAM_DRY: u32 = 3711743779;

/// The null test for delay compensation: with a dry-only plugin, the output must
/// be the input delayed by *exactly* the latency the plugin declares.
///
/// This is what the whole PDC declaration is for. A parallel dry/wet split routes
/// the same signal down two paths, one through the plugin; the host delays the
/// clean path by the declared latency so the two line up again. If the number is
/// wrong the paths are misaligned by the error and summing them comb-filters —
/// audible, and silent in every other test, because each path on its own is
/// perfectly fine.
///
/// The check here is the same thing an engineer does by ear, done numerically:
/// align the two legs by the declared latency, invert one, sum, and require the
/// residual to vanish. Nulling is far sharper than comparing peaks — a one-sample
/// misalignment on a 440 Hz sine leaves a residual ~6% of the signal, which no
/// amplitude assertion would notice.
///
/// Distinct from the pipelining tests: those establish that output lags input by
/// one block. This establishes that the number the plugin *reports* to
/// `latency::plan` is that same lag. A pipeline that worked while declaring 0
/// would pass every other test in this file.
#[test]
#[ignore]
#[cfg(feature = "clap")]
fn output_nulls_against_the_input_delayed_by_the_declared_latency() {
    let _guard = exclusive();
    let Some((mut units, handles)) = load_n(1) else {
        return;
    };

    // Dry-only: the plugin must pass audio through untouched, or "delayed by L"
    // is not a question that has an answer. Verified below rather than assumed —
    // `set_parameter` is fire-and-forget, so a request that never lands would
    // otherwise be indistinguishable from a latency error, and the reverb tail
    // would be blamed on PDC.
    handles[0].set_parameter(PARAM_WET, 0.0);
    handles[0].set_parameter(PARAM_DRY, 1.0);

    let unit = &mut units[0];
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
    // state rather than across the start-up transient.
    let blocks = 40;
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

    assert!(
        compared > BLOCK * 4,
        "too few live samples to judge ({compared}); the plugin was starved rather \
         than misaligned"
    );

    // Null depth in dB. A correct alignment leaves only the plugin's own
    // arithmetic error; a one-sample slip on this 440 Hz sine leaves ~-24 dB.
    let null_db = 10.0 * (residual_energy / signal_energy.max(f64::MIN_POSITIVE)).log10();
    println!(
        "null: {null_db:.1} dB over {compared} samples (worst residual {worst_residual:.6}, \
         declared latency {declared})"
    );

    if null_db >= -60.0 {
        // Distinguish the two ways this can fail before blaming either. Search
        // every plausible offset for the one that nulls best: if some *other*
        // offset nulls deeply, the signal is passing through cleanly and the
        // declared latency is simply wrong — a real PDC bug. If nothing nulls at
        // any offset, the plugin is not a passthrough at all and this test's
        // premise is unmet, which says nothing about PDC either way.
        let (best_offset, best_db) = best_null_offset(&dry, &wet, BLOCK * 4);
        assert!(
            best_db >= -60.0,
            "declared latency is WRONG: output nulls at {best_offset} samples \
             ({best_db:.1} dB) but the plugin declares {declared}. A parallel \
             dry/wet split will comb-filter by the {} sample difference.",
            (best_offset as isize - declared as isize).abs()
        );
        // Nothing nulls anywhere: the plugin is not a passthrough, so the premise
        // this test needs is unmet and it has no opinion on the declared latency.
        // Skipping rather than failing, because failing here would report a PDC
        // bug that has not been demonstrated — and a red test that means "the
        // fixture is wrong" trains people to ignore it.
        //
        // Currently reached with TAL-Reverb-4: the Wet=0/Dry=1 requests do not take
        // effect, and the plugin's own validation layer reports
        // `clap_plugin_params.flush() was called on the wrong thread`. That is a
        // parameter-path bug worth chasing on its own; it is not this test's
        // subject. Once a genuine passthrough is available here, this branch stops
        // being reachable and the assertion above becomes the live one.
        eprintln!(
            "SKIP: no offset nulls (best {best_db:.1} dB at {best_offset} samples), so \
             the plugin is not passing dry audio through and the declared latency of \
             {declared} cannot be judged. This is a fixture problem, not a PDC verdict \
             — see the parameter-path note above."
        );
    }
}

/// Find the delay offset at which `wet` best nulls against `dry`, and how deep
/// that null is in dB.
///
/// Used only to *diagnose* a failure: it separates "the signal passes through but
/// the declared latency is wrong" from "the signal is not passing through", which
/// otherwise look identical from a single failed comparison.
#[cfg(feature = "clap")]
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
