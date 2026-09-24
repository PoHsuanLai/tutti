//! [`AudioThread`] — a per-thread marker for "this code is inside an audio
//! callback".
//!
//! A *marker*, not an owner: it records that the current thread is running a
//! block right now, for as long as the returned guard lives. It deliberately
//! does not pin one thread as "the" audio thread — `AudioThreadCell`'s docs
//! explain why that stricter check is wrong (CoreAudio migrates its callback
//! between threads). What it answers is the question a destructor needs:
//! "am I about to free memory inside a block?"
//!
//! Cheap enough to set every block: a `const`-initialised thread-local `Cell`,
//! so entering neither allocates nor registers a destructor.

use std::cell::Cell;

thread_local! {
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// The audio-thread marker. See the `rt::audio_thread` module docs (`src/rt/audio_thread.rs`).
pub struct AudioThread;

/// Marks the current thread as running audio until dropped. Nestable.
#[must_use = "the thread is marked only while the guard lives"]
pub struct AudioThreadGuard {
    // `!Send`: the mark is per thread, so the guard must be dropped on the
    // thread that made it.
    _not_send: core::marker::PhantomData<*const ()>,
}

impl AudioThread {
    /// Mark the current thread as the audio thread until the guard drops.
    ///
    /// An executor calls this at the top of every block.
    pub fn enter() -> AudioThreadGuard {
        DEPTH.with(|d| d.set(d.get() + 1));
        AudioThreadGuard {
            _not_send: core::marker::PhantomData,
        }
    }

    /// Whether the current thread is inside an [`enter`](Self::enter) scope.
    pub fn is_current() -> bool {
        DEPTH.with(|d| d.get() > 0)
    }

    /// In debug builds, panic if the current thread is marked — for a `Drop`
    /// impl whose value must be freed on the control thread. `what` names it
    /// in the message. Silent while already unwinding, so a first panic is
    /// not turned into an abort.
    pub fn check_not_current(what: &str) {
        if cfg!(debug_assertions) && Self::is_current() && !std::thread::panicking() {
            panic!("{what} dropped on the audio thread: it must be freed on the control thread");
        }
    }
}

impl Drop for AudioThreadGuard {
    fn drop(&mut self) {
        DEPTH.with(|d| d.set(d.get() - 1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mark lasts exactly as long as the guard, nests, and is per thread.
    ///
    /// Mutation: make `Drop` a no-op → the mark leaks past the guard → fails.
    #[test]
    fn the_mark_is_scoped_nested_and_per_thread() {
        assert!(!AudioThread::is_current());
        {
            let _outer = AudioThread::enter();
            assert!(AudioThread::is_current());
            {
                let _inner = AudioThread::enter();
                assert!(AudioThread::is_current());
            }
            assert!(
                AudioThread::is_current(),
                "the inner guard ends only itself"
            );
            let other = std::thread::spawn(AudioThread::is_current)
                .join()
                .expect("thread ran");
            assert!(!other, "another thread is not marked");
        }
        assert!(!AudioThread::is_current());
    }
}
