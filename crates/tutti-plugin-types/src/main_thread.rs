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

/// A [`MainThread::claim`] on a thread other than the one that claimed first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the process's main thread was claimed by another thread")]
pub struct NotMainThread;

#[cfg(test)]
mod tests {
    use super::*;

    // Mutations (run): `claim` always returning `Ok` (drop the thread
    // comparison) fails the refusal assertion; `current` returning `Some`
    // whenever anything is claimed fails the `elsewhere` assertion.
    #[test]
    fn a_claim_is_refused_on_another_thread_and_repeats_on_the_first() {
        assert!(MainThread::current().is_none(), "nobody has claimed yet");
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
    }
}
