//! What does the host do when a plugin **dies mid-render**?
//!
//! The out-of-process design exists for exactly this: a plugin that segfaults
//! takes its own subprocess down and the host survives. That is the claim the
//! whole architecture is built on, and until now nothing tested it against a
//! plugin that actually dies.
//!
//! `hostile_peer_tests.rs` covers the *socket* half thoroughly — a peer that
//! closes, dribbles, sends garbage or an oversized frame — but every one of those
//! is a mock server written in Rust, speaking the wire protocol badly on purpose.
//! None of them is a real `plugin-server` hosting a real plugin whose `process()`
//! never returns, which is what a crash in the field looks like: the server is
//! correct, the plugin is not, and the process is gone between one block and the
//! next.
//!
//! # What the design promises, and what it does not
//!
//! Promised: audio before the crash is correct; the crash is *detected* rather
//! than hanging; the host substitutes **silence**, not garbage and not the last
//! good block on repeat; and the cause is latched so a host can say why.
//!
//! Not promised: recovery. `PluginStatus::Dead` is documented terminal — *"the
//! engine offers no relaunch, so recovery means loading a replacement"* — and
//! this suite asserts that, rather than asserting a respawn that does not exist.
//! A test that failed here after someone added a relaunch path would be a test
//! that had to be rewritten, which is correct: the terminal state is a decision,
//! and changing it should require saying so.
//!
//! # Why the plugin aborts rather than the mock closing a socket
//!
//! An `abort()` inside `process()` is the real shape. The subprocess dies
//! holding the socket, so the host learns about it as an EOF on a read it was
//! already making — the same path a segfault takes — rather than as a tidy
//! shutdown the server chose to perform. It also proves the failure crosses
//! every layer between: the plugin, the loader, the server's audio loop, the
//! socket, and the bridge thread.

#![cfg(feature = "clap")]

#[path = "support/clap_probe.rs"]
mod clap_probe;
use clap_probe::{exclusive, load_probe, render, ProbeEnv, Rig};

use std::time::{Duration, Instant};

use tutti_plugin::handles::PluginStatus;

const SAMPLE_RATE: f64 = 48_000.0;
const BLOCK: usize = 64;

/// One block period, and the pace the driving loop keeps.
///
/// The same reasoning as `clap_pdc_alignment`'s: the pipeline never waits for a
/// reply, so a tight loop starves the subprocess and every block reads back
/// silence — which here would be indistinguishable from the crash this suite is
/// trying to observe.
const PERIOD: Duration = Duration::from_nanos((BLOCK as f64 / SAMPLE_RATE * 1e9) as u64);
const PACE: Duration = PERIOD.saturating_mul(20);

/// Which `process()` call aborts.
///
/// Counted in the **plugin's** `process()` calls, which is not the same as the
/// host calls this test makes: the host submits block N and collects N-1, and
/// `Batcher::collectable` substitutes silence for any block the server has not
/// answered *yet*. A block starved that way does not arrive late — it never
/// arrives — so every host call spends a plugin block whether or not it yields
/// audio.
///
/// That is why this is generous rather than tight. [`GOOD_BLOCKS_REQUIRED`] good
/// blocks have to be *observed* before the abort, and on a cold pipeline the
/// early calls routinely yield nothing: the subprocess is still starting, and
/// under parallel load its first blocks can miss their `PACE` window. Sizing
/// this to the minimum needed on an idle machine turns a slow start into a
/// failure — the budget is spent before the pipeline is warm, and the abort
/// arrives with nothing yet checked. The pre-crash loop stops as soon as it has
/// seen enough, so the extra headroom costs nothing on a fast run.
const CRASH_AT_BLOCK: u32 = 40;

/// How many correct blocks must be seen before the crash.
///
/// Two, not one: a single good block cannot distinguish "the plugin is
/// rendering" from one lucky publish landing in the window.
const GOOD_BLOCKS_REQUIRED: u32 = 2;

/// Ceiling on [`drive_until_dead`]'s loop.
///
/// A bound, not a budget: the loop exits the moment death is observed, so this
/// only decides how long a plugin that never dies is given before the assertion
/// says so. Generous enough that a slow subprocess is never mistaken for a
/// surviving one — each block costs a [`PACE`] sleep, so the whole ceiling is
/// still well under [`NOTICE_TIMEOUT`]'s order of magnitude.
const DRIVE_TO_DEATH_MAX: usize = 60;

/// How long to keep polling for the crash to be noticed.
///
/// The subprocess dies on the audio thread; the host notices on its *bridge*
/// thread, when a read returns EOF. Those are different threads in different
/// processes, so the notice is prompt but not synchronous with the block that
/// killed it. Polling with a deadline rather than sleeping a fixed time: the
/// common case returns in microseconds and only a wedged host waits the full
/// budget. Mirrors `hostile_peer_tests::wait_for_crash`.
const NOTICE_TIMEOUT: Duration = Duration::from_secs(5);

/// The DC value fed to every input channel — a value no failure mode produces.
///
/// Not 1.0 and not 0: silence is what a crashed plugin's output looks like, so
/// an input of 0 would make "the plugin passed my audio through" and "the plugin
/// is dead" the same reading. 7.0 is distinct from the probe's own tag values
/// too, so a stale or misrouted buffer cannot coincide with it.
const INPUT_DC: f32 = 7.0;

/// `probe_tag(0, 0)`, the constant the probe adds to channel 0 in
/// `TagPassthrough`. Mirrored rather than imported: this crate does not link the
/// probe's rlib.
const TAG_P0C0: f32 = 1.0;

/// What a healthy block of channel 0 reads back as.
const EXPECTED_LIVE: f32 = INPUT_DC + TAG_P0C0;

/// Drive one block of DC through `unit` and return output channel 0.
fn drive_block(unit: &mut Rig) -> Vec<f32> {
    let out = unit.run(BLOCK, |_, _| INPUT_DC);
    std::thread::sleep(PACE);
    out.into_iter().next().expect("the probe has outputs")
}

/// Drive blocks until the host reports the plugin dead, and say how many it took.
///
/// The three tests here all need a plugin that has *actually* died before they
/// can assert anything, and all three used to spend a fixed twelve blocks and
/// then assert on the outcome. That is the same shape as the pre-crash budget
/// below, and it fails the same way: a host call whose input the server has not
/// consumed yet spends no plugin block, so on a cold or starved pipeline twelve
/// host calls need not deliver the one block that aborts — and the failure then
/// reads "the plugin should have crashed", blaming the crash path for a block
/// that was never submitted.
///
/// Driving until the observed status flips is the handshake. The bound is
/// [`DRIVE_TO_DEATH_MAX`] blocks, so a plugin that genuinely never dies still
/// ends the loop and fails an assertion rather than spinning.
fn drive_until_dead(unit: &mut Rig, handle: &tutti_plugin::handles::PluginHandle) -> usize {
    for driven in 1..=DRIVE_TO_DEATH_MAX {
        let _ = drive_block(unit);
        if handle.status().is_dead() {
            return driven;
        }
    }
    DRIVE_TO_DEATH_MAX
}

/// Poll until the host reports the plugin dead, or the deadline passes.
///
/// Returns the status either way, so the caller asserts on it and gets a useful
/// message rather than a bare timeout.
fn wait_for_death(handle: &tutti_plugin::handles::PluginHandle) -> PluginStatus {
    let deadline = Instant::now() + NOTICE_TIMEOUT;
    loop {
        let status = handle.status();
        if status.is_dead() || Instant::now() >= deadline {
            return status;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

// ---------------------------------------------------------------------------
// The whole story, in one test.
// ---------------------------------------------------------------------------

/// A plugin that aborts mid-render: good audio before, detection, silence after,
/// and a latched cause.
///
/// One test rather than four because these are four observations of a **single
/// irreversible event**. Splitting them would mean four subprocesses each
/// crashing at its own moment, and each test would then be asserting one
/// property of a *different* crash — with no way to say that the silence and the
/// good audio belong to the same run. The ordering is the content here: audio,
/// then death, then silence, in that order, from one plugin.
#[test]
fn a_plugin_that_aborts_mid_render_leaves_the_host_alive_and_silent() {
    let _lock = exclusive();
    let _env = ProbeEnv::new()
        .render_mode(render::TAG_PASSTHROUGH)
        .crash_on_block(CRASH_AT_BLOCK);
    let probe = load_probe(SAMPLE_RATE);
    let handle = probe.handle.clone();

    assert!(
        !handle.status().is_dead(),
        "the plugin must be alive before any block is driven; a death here \
         means the load failed rather than the render crashing"
    );

    let mut unit = Rig::new(probe.client.bind(), SAMPLE_RATE, BLOCK);

    // --- Before the crash: real audio, not silence and not the input echoed.
    //
    // Drive **until enough good blocks have been seen**, rather than driving a
    // fixed count and checking the tally afterwards. The difference is the whole
    // point: a fixed count is a deadline in disguise. The pipeline holds one
    // block, so the first call collects nothing by construction, and every later
    // call is good only if the subprocess published within one `PACE` — which a
    // cold subprocess under load does not reliably do. Spending a fixed budget
    // and asserting on the tally therefore fails whenever the start-up transient
    // eats it, and cannot recover, because the abort is armed on a block count
    // the starved calls have already spent.
    //
    // Waiting on the observed value instead is the same rule
    // `hostile_peer_tests` follows for the crash notification: wait for the
    // thing you are about to assert on, never for a proxy and never for a
    // wider timeout. `clap_pdc_alignment` warms the pipeline for the same
    // reason — it just knows its warm-up length up front, and this test cannot,
    // because here the warm-up shares a budget with the crash.
    //
    // The loop is still bounded, by the crash block itself: if the plugin never
    // renders, this exits when the budget is gone and the assertion below says
    // so, rather than spinning.
    let mut good_blocks = 0;
    let mut driven = 0;
    while good_blocks < GOOD_BLOCKS_REQUIRED && driven < CRASH_AT_BLOCK - 1 {
        let out = drive_block(&mut unit);
        driven += 1;
        if out.iter().all(|&s| s == EXPECTED_LIVE) {
            good_blocks += 1;
        }
    }
    assert!(
        good_blocks >= GOOD_BLOCKS_REQUIRED,
        "expected {GOOD_BLOCKS_REQUIRED} correct blocks before the crash, got \
         {good_blocks} in {driven} blocks. Zero means the plugin never rendered \
         at all, and every assertion below would then be about a plugin that was \
         never alive."
    );

    // --- The crash. Driving on eventually submits the block that aborts.
    //
    // A loop rather than one call: which block trips the switch depends on how
    // many the pipeline had already submitted, and the point is that the host
    // survives regardless of exactly when it happens.
    //
    // Driven until the death is *observed* rather than for a fixed count: the
    // loop above may have spent anywhere from [`GOOD_BLOCKS_REQUIRED`] to the
    // whole budget getting warm, so a constant here would be too few after a
    // slow start — and "too few" reads as "the host never noticed the death",
    // blaming detection for a block that was never submitted.
    let crash_blocks = drive_until_dead(&mut unit, &handle);

    // --- Detection, with a reason.
    let status = wait_for_death(&handle);
    assert!(
        status.is_dead(),
        "the host did not notice the plugin died within {NOTICE_TIMEOUT:?}, \
         after {driven} warm-up and {crash_blocks} further blocks. A live status \
         here means the bridge is still waiting on a process that is gone — \
         which is the hang this design exists to prevent. If {crash_blocks} is \
         the whole {DRIVE_TO_DEATH_MAX}-block ceiling, suspect instead that the \
         aborting block was never submitted: that is starvation, not a missed \
         detection, and it is the loop above that is too short rather than this \
         wait."
    );
    let cause = status.cause().unwrap_or_default();
    assert!(
        !cause.is_empty(),
        "a dead plugin must carry a latched cause; without one a host can \
         report that something died but never what"
    );

    // --- After the crash: silence. Not garbage, not the last good block.
    //
    // Silence specifically, and asserted on every sample: a host that left its
    // scratch buffer untouched would forward whatever was previously in that
    // memory, which for this rig is the last correct block — audible as a stuck
    // note, and passing any assertion that only checked "not the input".
    for block in 0..4 {
        let out = drive_block(&mut unit);
        for (i, &s) in out.iter().enumerate() {
            assert_eq!(
                s, 0.0,
                "sample {i} of post-crash block {block} is {s}, expected silence. \
                 {EXPECTED_LIVE} means the host is repeating the last good block \
                 (a stale-buffer leak); {INPUT_DC} means it fell back to echoing \
                 its input; anything else is uninitialised memory."
            );
        }
    }

    // --- Terminal. The engine offers no relaunch.
    assert!(
        handle.status().is_dead(),
        "death is terminal: the status must not recover on its own. If a \
         relaunch path was added, this assertion is the one to revisit — \
         deliberately, and not by accident."
    );
}

/// The crash does not hang the caller.
///
/// Separate from the test above because it is a claim about *time*, and the one
/// failure mode it names is the one that makes every other assertion unreachable:
/// a host that blocks waiting for a reply from a dead process never returns to
/// be asserted on at all, so the suite would report a harness timeout rather
/// than a diagnosis.
///
/// The bound is deliberately loose — each block already sleeps [`PACE`], so this
/// measures only that no block waited on the socket. A synchronous design would
/// spend its full reply budget per block on top of that.
#[test]
fn rendering_through_a_dead_plugin_does_not_block() {
    let _lock = exclusive();
    let _env = ProbeEnv::new()
        .render_mode(render::TAG_PASSTHROUGH)
        .crash_on_block(1);
    let probe = load_probe(SAMPLE_RATE);
    let handle = probe.handle.clone();
    let mut unit = Rig::new(probe.client.bind(), SAMPLE_RATE, BLOCK);

    // Get it dead first, so the measured blocks are all post-crash.
    let driven = drive_until_dead(&mut unit, &handle);
    assert!(
        handle.status().is_dead(),
        "the plugin should have crashed on its first block, but was still alive \
         after {driven} blocks"
    );

    const MEASURED: usize = 8;
    let start = Instant::now();
    for _ in 0..MEASURED {
        let _ = drive_block(&mut unit);
    }
    let elapsed = start.elapsed();

    // Only the sleeps, plus slack. Nothing here should touch the socket at all:
    // `Batcher::collectable` checks the crash flag *first*, before it even asks
    // the slab, precisely so a dead bridge costs one relaxed atomic load.
    let budget = PACE * MEASURED as u32 * 3;
    assert!(
        elapsed < budget,
        "{MEASURED} post-crash blocks took {elapsed:?}, over the {budget:?} \
         budget — the audio path appears to be waiting on a plugin that is gone"
    );
}

/// A crashed plugin is still safe to drop.
///
/// The subprocess is already gone, so `ProcessGuard::drop` reaps a child that
/// has exited and unlinks a socket whose peer is dead. Both are the paths least
/// likely to be exercised by hand and most likely to hang or panic — and a
/// teardown that panics turns a clean failure into a poisoned run for everything
/// after it.
#[test]
fn dropping_a_crashed_plugin_is_clean() {
    let _lock = exclusive();
    let _env = ProbeEnv::new()
        .render_mode(render::TAG_PASSTHROUGH)
        .crash_on_block(1);
    let probe = load_probe(SAMPLE_RATE);
    let handle = probe.handle.clone();

    {
        let mut unit = Rig::new(probe.client.bind(), SAMPLE_RATE, BLOCK);
        let driven = drive_until_dead(&mut unit, &handle);
        assert!(
            handle.status().is_dead(),
            "the plugin should have crashed, but was still alive after \
             {driven} blocks"
        );
    }
    // `unit` dropped; the handle still holds the guard.
    drop(handle);
    // Reaching here without a panic or a hang is the assertion.
}
