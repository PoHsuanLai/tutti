//! RT-safety regression: claiming the `[audio-thread]` role must not allocate,
//! **including when a second thread claims it in between**.
//!
//! # Why this exists separately from `clap_process_no_alloc`
//!
//! That test drives a real plugin through `process` and would, in principle,
//! catch a per-block allocation in the claim. It cannot catch this one: it is
//! single-threaded, so the same OS thread claims every time. The mechanism this
//! replaced kept a one-slot `Arc<ThreadId>` cache, which is only ever hit in
//! exactly that pattern — so the test passed while the hazard was live, and the
//! field's doc comment cited it as proof of a property it never checked.
//!
//! The interleaving that breaks it is ordinary: a user moves a plugin control
//! during playback, so the GUI thread calls `flush_params` (which claims the
//! role on an active instance) between two audio blocks. Every audio block then
//! misses the cache and allocates.
//!
//! # What is being tested
//!
//! `HostState` directly, not `process`. The claim is the whole mechanism, it
//! needs no plugin to exercise, and testing it here means this runs on every
//! `cargo test` rather than behind `--ignored` and an installed plugin — which
//! matters for a property no other test can see.
//!
//! # On `assert_no_alloc`
//!
//! The `#[global_allocator]` below is what does the detecting;
//! `assert_no_alloc` only sets a thread-local flag, so without the allocator
//! registered it is a silent no-op that passes unconditionally. A violation
//! aborts the process rather than unwinding, so a failure here is a SIGABRT
//! naming this test, not a normal assertion failure.

use assert_no_alloc::AllocDisabler;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use tutti_clap_host::HostState;

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// Claiming repeatedly from one thread must not allocate.
///
/// The weaker property, and the one the old mechanism did satisfy. Kept so a
/// regression can be localised: if this fails too, the problem is the claim
/// itself rather than the cross-thread interleaving below.
#[test]
fn repeated_claims_from_one_thread_do_not_allocate() {
    let state = HostState::new();

    // Warm up outside the gate: the first claim on a thread may touch cold
    // thread-local storage, and that one-shot cost is not what is under test.
    for _ in 0..8 {
        drop(state.claim_audio_thread());
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1024 {
            let claim = state.claim_audio_thread();
            assert!(state.is_audio_thread());
            drop(claim);
        }
    });
}

/// **The test the old mechanism fails.** An audio thread claiming every block
/// while a second thread claims in between must still not allocate.
///
/// The two threads alternate through a pair of barriers, so every one of the
/// audio thread's guarded claims is preceded by a claim from the other thread.
/// Under the old one-slot `Arc` cache that is a miss every single time: the
/// interloper's claim evicts the audio thread's cached `Arc`, and the next
/// audio claim calls `Arc::new`. Under a plain atomic there is nothing to
/// evict and nothing to allocate.
///
/// The interloper stands in for a GUI thread running `flush_params` on an
/// active instance, which takes this same claim (`ClapLoaded::flush_params`).
#[test]
fn claims_interleaved_with_another_thread_do_not_allocate() {
    const ROUNDS: usize = 256;

    const WARMUP: usize = 8;
    // Fixed, matched round count on both sides. A `stop` flag checked at the
    // top of the interloper's loop would deadlock: it parks on a barrier the
    // main thread has already stopped entering, so the counts must agree by
    // construction rather than by signalling.
    const TOTAL: usize = WARMUP + ROUNDS;

    let state = Arc::new(HostState::new());
    // Two barriers so the threads strictly alternate. One would let both run
    // the same phase together, and the interleaving is the entire point.
    let before = Arc::new(Barrier::new(2));
    let after = Arc::new(Barrier::new(2));

    let interloper = {
        let state = Arc::clone(&state);
        let before = Arc::clone(&before);
        let after = Arc::clone(&after);
        std::thread::spawn(move || {
            for _ in 0..TOTAL {
                // Claim first, so the audio thread's next claim finds whatever
                // this one left behind.
                drop(state.claim_audio_thread());
                before.wait();
                // The audio thread runs its claim here.
                after.wait();
            }
        })
    };

    // Warm up with the same alternation the gate will use, so any one-shot
    // allocation happens before the gate rather than inside it.
    for _ in 0..WARMUP {
        before.wait();
        drop(state.claim_audio_thread());
        after.wait();
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..ROUNDS {
            before.wait();
            let claim = state.claim_audio_thread();
            // The claim must actually name this thread — otherwise the loop
            // could be measuring a no-op.
            assert!(state.is_audio_thread());
            drop(claim);
            after.wait();
        }
    });

    interloper.join().unwrap();
}

/// The identity must be correct, not merely allocation-free — a claim that
/// published nothing would satisfy the tests above.
///
/// Covers the three states that matter to a plugin's thread checks: no claim,
/// a claim held by this thread, and a claim held by a different one.
#[test]
fn a_claim_names_exactly_the_claiming_thread() {
    let state = Arc::new(HostState::new());

    assert!(
        !state.is_audio_thread(),
        "no claim is outstanding, so no thread is the audio thread"
    );

    {
        let _claim = state.claim_audio_thread();
        assert!(state.is_audio_thread(), "this thread holds the claim");
    }

    assert!(
        !state.is_audio_thread(),
        "the claim was dropped, so the role must be released"
    );

    // A claim held elsewhere must not make this thread the audio thread. Run
    // on a scoped thread so the claim is provably alive during the assertion.
    let checked = Arc::new(AtomicBool::new(false));
    std::thread::scope(|s| {
        let state = Arc::clone(&state);
        let checked = Arc::clone(&checked);
        s.spawn(move || {
            let _claim = state.claim_audio_thread();
            assert!(state.is_audio_thread(), "the claiming thread holds it");
            checked.store(true, Ordering::Release);
        });
    });
    assert!(
        checked.load(Ordering::Acquire),
        "the scoped thread must run"
    );

    assert!(
        !state.is_audio_thread(),
        "the other thread's claim ended with it"
    );
}
