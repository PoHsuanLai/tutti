//! [`MainThreadMarker`]: proof that the holder is on the process's main thread.
//!
//! The plugin formats require that the calls other than the real-time audio
//! path (the editor, state, parameter enumeration, timers and the run loop) run
//! on the host's main/UI thread. Violating that is a spec breach that tends to
//! surface as silent state corruption or a crash several blocks later.
//!
//! The rule is in the types. A [`MainThreadMarker`] is `!Send` and `!Sync`, so a value
//! of it exists only on the thread that made it, and an entry that must run on
//! the main thread takes a `MainThreadMarker` by value (it is `Copy` and zero-sized). Holding one is the proof: the entries
//! run no check of their own.
//!
//! # Making one
//!
//! [`MainThreadMarker::new`] asks the OS whether the calling thread is the process's
//! main thread and is `Some` only then:
//!
//! - macOS and the BSDs that have it (FreeBSD, DragonFly, OpenBSD): `pthread_main_np`.
//! - Linux and Android: the thread id equals the process id (the main thread is
//!   the thread group leader).
//! - Windows: a CRT initializer, which the loader runs on the primary thread
//!   before `main`, records that thread's id, and the query compares it with the
//!   caller's.
//! - Any other target: no thread is known to be the main one, so `new` is
//!   always `None` and an in-process plugin host is refused at the call.
//!
//! The app makes the token once, on its main thread, and hands it down. A
//! library crate forbids `unsafe`, so it cannot make one anywhere else; a test
//! harness, which runs off the main thread, makes one with the one `unsafe`
//! constructor, [`MainThreadMarker::new_unchecked`].

use std::marker::PhantomData;

#[cfg(any(target_os = "linux", target_os = "android"))]
#[path = "main_thread/linux.rs"]
mod os;
#[cfg(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd"
))]
#[path = "main_thread/pthread.rs"]
mod os;
#[cfg(windows)]
#[path = "main_thread/windows.rs"]
mod os;
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    windows
)))]
#[path = "main_thread/unsupported.rs"]
mod os;

/// Proof that the holder is on the process's main thread.
///
/// `!Send` and `!Sync`: a copy cannot leave the thread that made it, so `Copy`
/// and `Clone` are free. Made by [`MainThreadMarker::new`].
///
/// ```
/// use tutti_rt::MainThreadMarker;
///
/// // A doctest's `main` runs on the process's main thread.
/// let main = MainThreadMarker::new().expect("on the main thread");
/// let again = main; // `Copy`
/// let _ = (main, again);
/// ```
///
/// It cannot be sent to another thread, so a spawned thread cannot use a
/// main-thread entry:
///
/// ```compile_fail
/// use tutti_rt::MainThreadMarker;
///
/// let main = MainThreadMarker::new().unwrap();
/// std::thread::spawn(move || {
///     drop(main);
/// });
/// ```
///
/// Nor shared by reference (`!Sync`):
///
/// ```compile_fail
/// use tutti_rt::MainThreadMarker;
///
/// let main = MainThreadMarker::new().unwrap();
/// std::thread::scope(|s| {
///     s.spawn(|| {
///         let shared: &MainThreadMarker = &main;
///         drop(*shared);
///     });
/// });
/// ```
///
/// The unchecked constructor is `unsafe`, so a crate that forbids `unsafe`
/// cannot make a token off the main thread:
///
/// ```compile_fail
/// use tutti_rt::MainThreadMarker;
///
/// let _main = MainThreadMarker::new_unchecked();
/// ```
#[derive(Debug, Clone, Copy)]
pub struct MainThreadMarker {
    // A raw pointer is neither `Send` nor `Sync`.
    _not_send: PhantomData<*const ()>,
}

impl MainThreadMarker {
    /// The proof, if the OS says the calling thread is the process's main
    /// thread, and `None` on any other thread.
    #[must_use]
    pub fn new() -> Option<MainThreadMarker> {
        os::is_main_thread().then_some(MainThreadMarker {
            _not_send: PhantomData,
        })
    }

    /// The proof, without asking the OS.
    ///
    /// For a test harness, whose tests run on threads the harness spawned, and
    /// for an embedder whose UI thread is not the OS's main thread (a plugin
    /// running inside a host application).
    ///
    /// # Safety
    ///
    /// Everything the token is passed to runs on the calling thread, and no
    /// other thread runs a main-thread entry of the same plugin at the same
    /// time: the calling thread stands in for the main thread for as long as
    /// the token lives.
    #[must_use]
    pub unsafe fn new_unchecked() -> MainThreadMarker {
        MainThreadMarker {
            _not_send: PhantomData,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A token is made only on the main thread**: the test harness runs this
    /// on a thread it spawned, and the OS says that is not the main thread.
    #[test]
    fn a_spawned_thread_is_not_the_main_thread() {
        assert!(MainThreadMarker::new().is_none());
        assert!(
            std::thread::spawn(|| MainThreadMarker::new().is_none())
                .join()
                .unwrap()
        );
    }
}
