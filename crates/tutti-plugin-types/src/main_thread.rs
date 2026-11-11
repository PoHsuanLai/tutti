//! The main-thread token, shared by every plugin-host crate.
//!
//! All native plugin formats (VST2, VST3, CLAP, AU) require that the calls
//! other than the real-time audio path (the editor, state, parameter
//! enumeration, timers and the run loop) run on the host's main/UI thread.
//! Violating that is a spec breach that tends to surface as silent state
//! corruption or a crash several blocks later, the hardest class of bug to
//! trace back.
//!
//! The rule is in the types. A [`MainThread`] is `!Send` and `!Sync`, so a
//! value of it exists only on the thread that claimed it, and an entry that
//! must run on the main thread takes a `&MainThread`. Holding one is the proof:
//! the entries run no check of their own, in debug or in release.
//!
//! # Claiming
//!
//! [`MainThread::claim`] is the only way to make one. The first thread to
//! claim becomes the process's main thread; a later claim on that same thread
//! succeeds again (a copy of the same proof) and a claim on any other thread is
//! refused with [`NotMainThread`]. The host claims once, on its UI thread, at
//! startup, and hands the token down.
//!
//! **Who claims.** The claim is first-come, and nothing checks that the first
//! claimer is the UI thread: `claim` does not ask the OS (a test runner's
//! threads are not the OS main thread, and every test claims). So the host must
//! claim from its UI or OS main thread, first, at startup; a thread that claims
//! earlier becomes "the main thread" for the process, and the real one is then
//! refused. `bevy-tutti`'s `TuttiHostingPlugin::build` claims and panics if it
//! is refused. A format whose soundness needs the OS's main thread still asks
//! the OS itself (the AU editor re-checks `pthread_main_np`).
//!
//! The claim is **once per process**, not once per test. Under `cargo nextest`
//! every test is its own process, so a test claims freely; under plain
//! `cargo test` the tests of one binary share a process and run on different
//! threads, so only the first to claim would win. The workspace runs its tests
//! with nextest (`CLAUDE.md`), and a doctest is a process of its own.

use std::marker::PhantomData;
use std::sync::OnceLock;
use std::thread::ThreadId;

static MAIN: OnceLock<ThreadId> = OnceLock::new();

/// Proof that the holder is on the process's main thread.
///
/// `!Send` and `!Sync`: a copy cannot leave the thread that claimed it, so
/// `Copy` and `Clone` are free. Made by [`MainThread::claim`].
///
/// ```
/// use tutti_plugin_types::MainThread;
///
/// let main = MainThread::claim().unwrap();
/// let again = main; // `Copy`
/// let _ = (main, again);
/// ```
///
/// It cannot be sent to another thread, so a spawned thread cannot use a
/// main-thread entry:
///
/// ```compile_fail
/// use tutti_plugin_types::MainThread;
///
/// let main = MainThread::claim().unwrap();
/// std::thread::spawn(move || {
///     drop(main);
/// });
/// ```
///
/// Nor shared by reference (`!Sync`):
///
/// ```compile_fail
/// use tutti_plugin_types::MainThread;
///
/// let main = MainThread::claim().unwrap();
/// std::thread::scope(|s| {
///     s.spawn(|| {
///         let shared: &MainThread = &main;
///         drop(*shared);
///     });
/// });
/// ```
// Mutation record (the two `compile_fail` doctests above): replacing the
// marker `PhantomData<*const ()>` by `PhantomData<()>` makes the type `Send`
// and `Sync`, and both doctests then compile, so both fail as doctests.
#[derive(Debug, Clone, Copy)]
pub struct MainThread {
    // A raw pointer is neither `Send` nor `Sync`.
    _not_send: PhantomData<*const ()>,
}

impl MainThread {
    /// Claims the calling thread as the process's main thread.
    ///
    /// The first call in the process wins; later calls on the same thread
    /// succeed again.
    ///
    /// # Errors
    ///
    /// [`NotMainThread`] if another thread has claimed already.
    pub fn claim() -> Result<MainThread, NotMainThread> {
        let this = std::thread::current().id();
        if *MAIN.get_or_init(|| this) == this {
            Ok(MainThread {
                _not_send: PhantomData,
            })
        } else {
            Err(NotMainThread)
        }
    }

    /// Whether the process's main thread has been claimed by a thread other than
    /// the caller. `false` while nobody has claimed, so a debug check built on
    /// it is silent in a process with no main thread (a headless rig), and trips
    /// only for a free that happens on the wrong thread.
    ///
    /// For the paths that cannot be handed a token (a `Drop`, a session's free
    /// of a retired unit), where it backs a `debug_assert!`. An entry that can
    /// take a [`MainThread`] takes one instead, and checks nothing.
    #[must_use]
    pub fn claimed_elsewhere() -> bool {
        MAIN.get()
            .is_some_and(|main| *main != std::thread::current().id())
    }

    /// The proof, if the calling thread is the one that claimed, and `None`
    /// otherwise, including while nobody has claimed. It never claims.
    ///
    /// For the code that cannot be handed a token, a `Drop` above all: it can
    /// run on whichever thread releases the last owner, and does main-thread
    /// work only when that happens to be the main thread.
    #[must_use]
    pub fn current() -> Option<MainThread> {
        (MAIN.get() == Some(&std::thread::current().id())).then_some(MainThread {
            _not_send: PhantomData,
        })
    }
}

/// Work a thread that is not the main thread hands to it: what a `Drop` off the claimed
/// thread cannot do itself (a plugin's `effClose`, say) and must not skip.
type Deferred = Box<dyn FnOnce(&MainThread) + Send>;

static DEFERRED: std::sync::Mutex<Vec<Deferred>> = std::sync::Mutex::new(Vec::new());

/// Hands `work` to the main thread, to run at its next [`run_deferred`].
///
/// **Someone must call [`run_deferred`] on the main thread.** `Session::update`
/// does, in `Phase::Collect`, but it is a main-thread call itself (it
/// debug-asserts that it is not off a claimed main thread), so a host that
/// drives the session from another thread must call [`run_deferred`] from its
/// main-thread loop. Without a pump, work posted here never runs; a caller that
/// waits for it (the in-process plugin handle's off-main calls) gets its
/// "unavailable" answer when its deadline expires.
///
/// For a `Drop` that finds itself off the claimed thread and owns something that must be
/// released there: it cannot be handed a token, so it hands over the release.
///
/// # Never from an audio thread
///
/// This takes a `std::sync::Mutex` (it can block) and boxes the work (it allocates). Callable
/// from any control-side or pool thread, **not from the audio callback**. The callers keep to
/// that by an invariant of the graph, not by a check here: a unit leaves the plan on the
/// control side. The executor moves a retired, replaced or crossfaded-out unit into the commit
/// box it sends back (`tutti-graph`'s `exec.rs`: `c.retired.push` in `apply_pending`, the
/// `faded` ring for a crossfade's outgoing unit), `Editor::collect` drops it on the control
/// thread, and a `NodeBox`, `Crossfade` or `Commit` dropped inside the audio scope panics in a
/// debug build (`RtScope::check_outside`). When a session ends, the stream's owner thread is
/// joined before the driver releases the last `Arc` of the callback state (`tutti-cpal`'s
/// `TuttiDriver` field order, `owner.rs`'s join), so the executor and its units drop on the
/// control thread. So the `RenderHandle` of a plugin node (in its `Processor`) is never dropped
/// on the audio thread, and its deferral here never runs there. The in-process VST2 suite pins
/// it: `tutti-plugin/tests/vst2_in_process_retire.rs`.
pub fn defer_to_main(work: impl FnOnce(&MainThread) + Send + 'static) {
    DEFERRED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(Box::new(work));
}

/// Runs the work other threads handed over by [`defer_to_main`], on the calling (main) thread,
/// and says how many ran. A main-thread tick the host already has calls this: the session's
/// `Phase::Collect` (which runs on the claiming thread, as `Session::update` must). A host that
/// does not call `Session::update` on its main thread calls this from its main-thread loop.
/// Work that defers more work leaves it for the next call.
pub fn run_deferred(main: &MainThread) -> usize {
    let work = std::mem::take(
        &mut *DEFERRED
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    );
    let ran = work.len();
    for work in work {
        work(main);
    }
    ran
}

/// A [`MainThread::claim`] on a thread other than the one that claimed first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the process's main thread was claimed by another thread")]
pub struct NotMainThread;

#[cfg(test)]
mod tests {
    use super::*;

    // Mutations (run): `claim` always returning `Ok` (drop the thread
    // comparison) fails the refusal assertion; `current` returning `Some`
    // whenever anything is claimed fails the `elsewhere` assertion; `claimed_elsewhere` answering `false` always
    // fails the `off` assertion.
    #[test]
    fn a_claim_is_refused_on_another_thread_and_repeats_on_the_first() {
        assert!(MainThread::current().is_none(), "nobody has claimed yet");
        assert!(!MainThread::claimed_elsewhere(), "nobody has claimed yet");
        let first = MainThread::claim();
        assert!(first.is_ok());
        assert!(MainThread::claim().is_ok(), "the same thread claims again");
        // The token itself cannot cross back, so the thread reports the error.
        let other = std::thread::spawn(|| MainThread::claim().err())
            .join()
            .unwrap();
        assert_eq!(other, Some(NotMainThread));
        // `current` follows the claim and never makes one.
        assert!(MainThread::current().is_some());
        let elsewhere = std::thread::spawn(|| MainThread::current().is_none())
            .join()
            .unwrap();
        assert!(elsewhere, "only the claiming thread holds the proof");
        assert!(
            !MainThread::claimed_elsewhere(),
            "the claimer is not elsewhere"
        );
        let off = std::thread::spawn(MainThread::claimed_elsewhere)
            .join()
            .unwrap();
        assert!(off, "another thread is elsewhere once a thread has claimed");
    }

    // Mutation (run): `run_deferred` returning without draining (or `defer_to_main` dropping
    // the work) leaves the flag unset on the main thread and the count at 0: both fail.
    #[test]
    fn work_deferred_off_the_main_thread_runs_on_it() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let main = MainThread::claim().unwrap();
        let ran_on_main = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&ran_on_main);
        let main_id = std::thread::current().id();
        std::thread::spawn(move || {
            defer_to_main(move |_main| {
                flag.store(std::thread::current().id() == main_id, Ordering::SeqCst);
            });
        })
        .join()
        .unwrap();
        assert!(
            !ran_on_main.load(Ordering::SeqCst),
            "not run by the deferring thread"
        );
        assert_eq!(run_deferred(&main), 1);
        assert!(ran_on_main.load(Ordering::SeqCst), "run on the main thread");
        assert_eq!(run_deferred(&main), 0, "and once");
    }
}
