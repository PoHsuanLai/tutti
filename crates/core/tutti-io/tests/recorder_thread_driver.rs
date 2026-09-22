//! [`ThreadDriver`] — the production driver — under test for the first time.
//!
//! Every existing `Recorder` test drives a [`ManualDriver`], deliberately:
//! `recorder.rs`'s header records that the old versions slept 20–50 ms and one
//! asserted `frames >= 3` against a floor "tuned on a loaded machine", which is
//! an assertion about the test host rather than the engine. Counting passes
//! replaced that, and it was the right trade.
//!
//! But it left the driver a host actually gets with **no coverage at all**.
//! Nothing ran `std::thread::spawn`, the `Acquire`/`Release` stop handshake,
//! the [`PumpPass::Ended`] break, the [`IDLE_PARK`] on a starving source, or
//! `impl RunningPump for JoinHandle` — including its "recording thread
//! panicked" arm. `Recorder::start`, the entry point every consumer calls, was
//! reached by exactly one test, and only to check a width mismatch it rejects
//! before spawning anything.
//!
//! These four cover that surface without reintroducing the tuned sleep. The
//! first three sequence on a fact the pump thread *publishes about itself* —
//! drained, polled N times — via [`wait_until`], so no assertion depends on a
//! duration. The fourth needs none of that, and that is its point:
//! [`Recorder::wait`] was added because of what the first one had to do.
//! The one property that genuinely cannot be asserted is "a live take never
//! ends on its own": its counter-example is a hang, and its only bound is
//! nextest's per-test timeout. That is said plainly at the test rather than
//! dressed up as a check.
//!
//! Mutations run against `ThreadDriver`, and which test each one broke. Every
//! row was executed, not reasoned about:
//!
//! | mutation in `recorder.rs` | fails |
//! |---|---|
//! | `PumpPass::Ended => {}` | `a_finite_source_…` (poll count keeps climbing after drain) |
//! | `while false && running.load(…)` | all three (nothing is ever pumped) |
//! | `PumpPass::Starved => break` | `a_starving_source_…` (times out in `wait_until`) |
//! | `.unwrap_or_else(…)` → `.unwrap()` | `a_panicking_source_…` |
//! | `Recorder::wait` calls `shutdown()` rather than `join_pump()` | `wait_returns_a_complete_finite_take_…` (short file) |
//!
//! The first row is worth keeping: an earlier draft of `a_finite_source_…`
//! **passed** under `PumpPass::Ended => {}`. Waiting for the source to run dry
//! and then checking the file proves nothing about the break, because a driver
//! that spins on a dry source still finalizes correctly once `stop()` clears
//! the flag. The only externally visible consequence of the break is that the
//! source stops being polled, so that is what the test now asserts. A test
//! that cannot fail is worse than no test; this one could not, until it could.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use tutti_core::ChannelLayout;
use tutti_io::{AudioIn, BitDepth, OnEmpty, Recorder, WavOut};

/// Block until `cond` holds, or fail the test at `deadline`.
///
/// Not a `sleep(n)` in disguise: the wait *length* carries no meaning and no
/// assertion depends on it. The condition is a fact the pump thread publishes
/// about itself, so this converts "the other thread got there" into something
/// the test can sequence on. The deadline exists only so a broken driver fails
/// with a message instead of hanging until nextest's timeout.
///
/// Only the live-source tests need this now. [`Recorder::wait`] awaits a
/// *finite* take's natural end directly — see
/// `wait_returns_a_complete_finite_take_without_sequencing_on_the_pump`, which
/// is what this helper's absence looks like.
fn wait_until(what: &str, cond: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !cond() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// A finite source: hands out `frames` frames of a known ramp, then reports
/// end-of-stream. `ThreadDriver` must break its own loop on that.
///
/// Re-declared here rather than shared with `recorder.rs`'s fixture of the same
/// shape: that one is `#[cfg(test)]`-private to the crate, and an integration
/// binary cannot see it. `tutti-sampler`'s `MockTransport` hits the same wall
/// and answers it the same way (`tests/tier_parity.rs:119`).
struct Ramp {
    frames: usize,
    pos: usize,
    /// Set on the poll that returns 0 — i.e. the poll the driver turns into
    /// [`PumpPass::Ended`]. Once this is visible the thread has already left
    /// its loop and is committed to finalizing, so the test can `stop()` (and
    /// join) without truncating the take.
    drained: Arc<AtomicBool>,
    /// Total polls, including the drained ones. Once the driver has broken on
    /// [`PumpPass::Ended`] this stops advancing; a driver that does NOT break
    /// spins on the dry source and it climbs without bound. That difference is
    /// the only externally visible consequence of the break, so it is what the
    /// test asserts.
    polls: Arc<AtomicUsize>,
    /// Wall time burned per poll, to give a take a measurable *duration*.
    ///
    /// [`std::time::Duration::ZERO`] everywhere except
    /// `wait_returns_a_complete_finite_take_…`, whose mutation is "clear the
    /// run flag before joining" — and at `SCRATCH_FRAMES` = 1024 per poll an
    /// instant source can drain in so few passes that a truncating `wait`
    /// might still catch the whole take by luck. No assertion reads this or
    /// any clock; it only widens the window the mutation has to land in, so
    /// the difference between a complete take and a truncated one is not a
    /// coin flip.
    delay: std::time::Duration,
}

impl AudioIn for Ramp {
    const ON_EMPTY: OnEmpty = OnEmpty::EndOfStream;

    fn layout(&self) -> ChannelLayout {
        ChannelLayout::STEREO
    }

    fn poll_into(&mut self, out: &mut [f32]) -> usize {
        self.polls.fetch_add(1, Ordering::Release);
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        let n = (self.frames - self.pos).min(out.len() / 2);
        if n == 0 {
            self.drained.store(true, Ordering::Release);
            return 0;
        }
        for i in 0..n {
            // Distinct per frame and per channel, so a dropped, duplicated or
            // channel-rotated frame is visible in the VALUES, not just a count.
            let f = (self.pos + i) as f32;
            out[i * 2] = f / 100_000.0;
            out[i * 2 + 1] = -(f / 100_000.0);
        }
        self.pos += n;
        n
    }
}

/// A source that never ends: yields a frame every other poll, so the driver
/// takes its `Starved` branch about half the time.
struct HalfStarving {
    /// Shared so the test can sequence on "the driver came back after a
    /// starve" rather than on a duration.
    polls: Arc<AtomicUsize>,
}

impl AudioIn for HalfStarving {
    const ON_EMPTY: OnEmpty = OnEmpty::Starved;

    fn layout(&self) -> ChannelLayout {
        ChannelLayout::STEREO
    }

    fn poll_into(&mut self, out: &mut [f32]) -> usize {
        let n = self.polls.fetch_add(1, Ordering::Release) + 1;
        if n.is_multiple_of(2) {
            return 0;
        }
        let frames = out.len() / 2;
        out[..frames * 2].fill(0.25);
        frames
    }
}

/// Panics on its first poll, on the pump thread.
struct Exploding {
    /// Observed by the test so it can assert the thread really got that far,
    /// rather than the panic coming from somewhere else.
    polled: Arc<AtomicUsize>,
}

impl AudioIn for Exploding {
    const ON_EMPTY: OnEmpty = OnEmpty::Starved;

    fn layout(&self) -> ChannelLayout {
        ChannelLayout::STEREO
    }

    fn poll_into(&mut self, _out: &mut [f32]) -> usize {
        self.polled.fetch_add(1, Ordering::Release);
        panic!("the source fell over mid-take");
    }
}

fn sink(path: &std::path::Path) -> WavOut {
    WavOut::create(path, 48_000.0, 2u16, BitDepth::Float32).expect("sink opens")
}

/// **A finite take ends itself, on a real thread, and the file is readable.**
///
/// This is the whole production path in one test: `Recorder::start` spawns via
/// `ThreadDriver`, the loop runs `pump_once` until `Ramp` reports
/// [`OnEmpty::EndOfStream`], the driver breaks, finalizes where it owns the
/// loop, and the `io::Result` rides the joined `JoinHandle` back to `stop`.
///
/// The frame count is exact, and the values are checked too — a count alone
/// would pass on duplicated or channel-rotated frames.
///
/// **This test keeps its workaround on purpose.** It asserts the poll count
/// freezes *after* the drain and *before* the join, so it has to observe the
/// thread mid-flight — [`Recorder::wait`] would join it and destroy the
/// window. The gap that workaround originally exposed is closed:
/// `stop()` still clears the flag before joining and still truncates a finite
/// take called promptly (on this test's first draft, to zero frames), but
/// `wait()` now exists for exactly that case and
/// `wait_returns_a_complete_finite_take_without_sequencing_on_the_pump`
/// covers it with no fixture flag at all.
#[test]
fn a_finite_source_ends_its_own_take_and_the_file_is_readable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("finite.wav");

    const FRAMES: usize = 5000;
    let drained = Arc::new(AtomicBool::new(false));
    let polls = Arc::new(AtomicUsize::new(0));
    let rec = Recorder::start(
        Ramp {
            frames: FRAMES,
            pos: 0,
            drained: Arc::clone(&drained),
            polls: Arc::clone(&polls),
            delay: std::time::Duration::ZERO,
        },
        sink(&path),
    )
    .expect("stereo source into a stereo sink");

    wait_until("the finite source to run dry", || {
        drained.load(Ordering::Acquire)
    });

    // The driver must now have LEFT its loop, not merely noticed the zero.
    // Sample the poll count, give a spinning loop ample room to betray itself,
    // and require it not to have moved. Without the `Ended => break` arm the
    // thread re-polls a dry source as fast as it can and this climbs into the
    // thousands; with it, the count is frozen at the drained poll.
    let at_drain = polls.load(Ordering::Acquire);
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert_eq!(
        polls.load(Ordering::Acquire),
        at_drain,
        "the driver must break its loop on EndOfStream, not keep polling a dry \
         source — a live-spinning take burns a core until someone calls stop()"
    );

    rec.stop().expect("a healthy finite take finalizes cleanly");

    let mut reader = hound::WavReader::open(&path).expect("finalized WAV should be readable");
    assert_eq!(reader.spec().channels, 2);
    assert_eq!(
        reader.len() as usize,
        FRAMES * 2,
        "every frame the source produced must reach the file, and no frame twice"
    );

    let samples: Vec<f32> = reader.samples::<f32>().map(|s| s.unwrap()).collect();
    for (i, pair) in samples.as_chunks::<2>().0.iter().enumerate() {
        let expect = i as f32 / 100_000.0;
        assert!(
            (pair[0] - expect).abs() < 1e-9 && (pair[1] + expect).abs() < 1e-9,
            "frame {i} should be ({expect}, {}) but was ({}, {})",
            -expect,
            pair[0],
            pair[1]
        );
    }
}

/// **A live take ends only when someone stops it.**
///
/// `HalfStarving` reports [`OnEmpty::Starved`] and never runs dry, so the
/// driver must park and re-poll rather than break — that flag is the only
/// thing that ends a microphone take, and the reason `Drop` exists at all.
///
/// Honest about its bound: the counter-example to "only `stop` ends it" is a
/// take that *never* ends, and no assertion can observe that. This test's
/// bound is nextest's per-test timeout, not an `assert`. What it does assert
/// positively is that frames landed before the stop and the header was
/// back-patched — so a driver that broke early on `Starved` (writing nothing)
/// fails here loudly rather than hanging.
#[test]
fn a_starving_source_is_ended_only_by_stop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live.wav");

    let polls = Arc::new(AtomicUsize::new(0));
    let rec = Recorder::start(
        HalfStarving {
            polls: Arc::clone(&polls),
        },
        sink(&path),
    )
    .expect("stereo source into a stereo sink");

    // Wait for the source to have been polled through at least one full
    // yield/starve cycle, so the driver has demonstrably taken its `Starved`
    // branch and come back. The *number* is the fact being waited on, not a
    // duration — see `wait_until`.
    wait_until(
        "the live source to survive a starve and be polled again",
        || polls.load(Ordering::Acquire) >= 3,
    );

    rec.stop().expect("stopping a live take finalizes cleanly");

    let reader = hound::WavReader::open(&path)
        .expect("a stopped live take must leave a readable, header-patched WAV");
    assert!(
        reader.len() > 0,
        "a source that yields on every other poll must have written something \
         before the stop — zero frames means the driver broke on Starved"
    );
}

/// **A panic on the pump thread becomes an error, not a lost take.**
///
/// `impl RunningPump for JoinHandle` maps a poisoned join to
/// `io::Error::other("recording thread panicked")`. Nothing exercised that arm,
/// so a panicking source would have surfaced as… whatever `unwrap` does.
///
/// Safe to run only because this repo uses nextest, which gives every test its
/// own process: the panic is caught by the join here, but a mutation that
/// replaces `unwrap_or_else` with `unwrap` aborts the *binary*, and
/// process-per-test keeps that from taking the other suites' results with it.
/// Under plain `cargo test` this would be a much worse neighbour — see
/// `CLAUDE.md` on why nextest is not optional here.
#[test]
fn a_panicking_source_surfaces_as_a_recording_thread_panic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("panicked.wav");
    let polled = Arc::new(AtomicUsize::new(0));

    let rec = Recorder::start(
        Exploding {
            polled: Arc::clone(&polled),
        },
        sink(&path),
    )
    .expect("stereo source into a stereo sink");

    // Sequence on the panic having happened, not on a duration: `stop()`
    // called first would clear the flag and the thread would exit cleanly
    // without ever polling, which is the race the first draft of this test hit.
    wait_until("the source to be polled and panic", || {
        polled.load(Ordering::Acquire) >= 1
    });

    let err = rec
        .stop()
        .expect_err("a panicked pump thread must be reported, not silently joined");

    assert!(
        err.to_string().contains("recording thread panicked"),
        "expected the join-poison message, got: {err}"
    );
    assert!(
        polled.load(Ordering::Acquire) >= 1,
        "the panic must have come from the source being polled on the pump \
         thread — if it was never polled, this test is proving something else"
    );
}

/// **`wait()` returns a complete finite take, with nothing to sequence on.**
///
/// The counterpart to `a_finite_source_ends_its_own_take_…` above, and the
/// reason [`Recorder::wait`] exists. That test has to publish the source's
/// own exhaustion through an `AtomicBool` and spin on it, because `stop()`
/// clears the run flag *before* joining and calling it promptly truncates the
/// take — on the first draft, to zero frames. A consumer recording a finite
/// source had the same problem and no better answer.
///
/// So the body here is the assertion: `start`, `wait`, read the file. No
/// `wait_until`, no published flag, no sleep in the test. If `wait` did not
/// hold the loop open to the source's own [`OnEmpty::EndOfStream`], the frame
/// count would come up short — which is exactly the mutation.
///
/// *Mutation:* make `Recorder::wait` call `shutdown()` instead of
/// `join_pump()` — i.e. give it `stop`'s flag clear. The file comes back at
/// one or two 1024-frame polls instead of all 20, and the count assertion
/// fails. (`Ramp::delay` is what stops that from being a coin flip; see the
/// field's own comment.)
#[test]
fn wait_returns_a_complete_finite_take_without_sequencing_on_the_pump() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("awaited.wav");

    // 20 full scratch buffers, paced so a truncating `wait` cannot finish the
    // take by accident.
    const FRAMES: usize = 1024 * 20;
    let rec = Recorder::start(
        Ramp {
            frames: FRAMES,
            pos: 0,
            drained: Arc::new(AtomicBool::new(false)),
            polls: Arc::new(AtomicUsize::new(0)),
            delay: std::time::Duration::from_millis(1),
        },
        sink(&path),
    )
    .expect("stereo source into a stereo sink");

    rec.wait().expect("a finite take finalizes cleanly");

    let mut reader = hound::WavReader::open(&path).expect("finalized WAV should be readable");
    assert_eq!(
        reader.len() as usize,
        FRAMES * 2,
        "wait() must hold the loop open to the source's own end; a short file \
         means it cut the take off the way stop() does"
    );

    // The values, not just the count: a take that is the right length but
    // assembled from repeated or rotated frames is still wrong.
    let samples: Vec<f32> = reader.samples::<f32>().map(|s| s.unwrap()).collect();
    for (i, pair) in samples.as_chunks::<2>().0.iter().enumerate() {
        let expect = i as f32 / 100_000.0;
        assert!(
            (pair[0] - expect).abs() < 1e-6 && (pair[1] + expect).abs() < 1e-6,
            "frame {i} came back as {pair:?}, expected [{expect}, {}]",
            -expect
        );
    }
}
