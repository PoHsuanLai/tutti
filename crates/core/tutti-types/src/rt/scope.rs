//! [`RtScope`] — a per-thread marker for "this code is inside an audio
//! callback".
//!
//! A *marker*, not an owner: it records that the current thread is running a
//! block right now, for as long as the returned guard lives. It deliberately
//! does not pin one thread as "the" audio thread — `AudioThreadCell`'s docs
//! explain why that stricter check is wrong (CoreAudio migrates its callback
//! between threads). What it answers is the question a destructor needs:
//! "am I about to free memory inside a block?" Hence the name: it marks a
//! *scope* (from [`RtScope::enter`] to its guard's drop), not a thread.
//!
//! Cheap enough to set every block: a `const`-initialised thread-local `Cell`,
//! so entering neither allocates nor registers a destructor.

use std::cell::Cell;

thread_local! {
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// A per-thread marker that says "this code is inside an audio callback".
///
/// A *marker*, not an owner: it records that the current thread is running a
/// block, for as long as the [`RtScopeGuard`] from [`enter`](Self::enter)
/// lives. It does not pin one thread as "the" audio thread, because some hosts
/// (CoreAudio) move their callback between threads. What it answers is the
/// question a destructor needs: "am I about to free memory inside a block?"
/// [`check_outside`](Self::check_outside) is that question asked from
/// a `Drop` impl.
///
/// Cheap enough to set every block: entering is a `const`-initialised
/// thread-local counter, so it neither allocates nor registers a destructor.
///
/// # Examples
///
/// ```
/// use tutti_types::RtScope;
///
/// assert!(!RtScope::is_inside());
/// {
///     let _block = RtScope::enter();
///     assert!(RtScope::is_inside());
/// }
/// assert!(!RtScope::is_inside());
/// ```
pub struct RtScope;

/// Marks the current thread as running audio until dropped.
///
/// Returned by [`RtScope::enter`]. Guards nest: the thread stays marked
/// until the last one drops. `!Send`, so it is dropped on the thread that
/// made it.
#[must_use = "the thread is marked only while the guard lives"]
pub struct RtScopeGuard {
    // `!Send`: the mark is per thread, so the guard must be dropped on the
    // thread that made it.
    _not_send: core::marker::PhantomData<*const ()>,
}

impl RtScope {
    /// Marks the current thread as the audio thread until the guard drops.
    ///
    /// An executor calls this at the top of every block. Allocation-free.
    pub fn enter() -> RtScopeGuard {
        DEPTH.with(|d| d.set(d.get() + 1));
        RtScopeGuard {
            _not_send: core::marker::PhantomData,
        }
    }

    /// Returns whether the current thread is inside an [`enter`](Self::enter)
    /// scope.
    pub fn is_inside() -> bool {
        DEPTH.with(|d| d.get() > 0)
    }

    /// Panics, in debug builds, if the current thread is marked.
    ///
    /// For a `Drop` impl whose value must be freed on the control thread.
    /// `what` names the value in the message. Does nothing in release builds.
    ///
    /// # Panics
    ///
    /// In debug builds, when called inside an [`enter`](Self::enter) scope,
    /// unless the thread is already unwinding (so a first panic is not turned
    /// into an abort).
    pub fn check_outside(what: &str) {
        if cfg!(debug_assertions) && Self::is_inside() && !std::thread::panicking() {
            panic!("{what} dropped on the audio thread: it must be freed on the control thread");
        }
    }
}

impl Drop for RtScopeGuard {
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
        assert!(!RtScope::is_inside());
        {
            let _outer = RtScope::enter();
            assert!(RtScope::is_inside());
            {
                let _inner = RtScope::enter();
                assert!(RtScope::is_inside());
            }
            assert!(RtScope::is_inside(), "the inner guard ends only itself");
            let other = std::thread::spawn(RtScope::is_inside)
                .join()
                .expect("thread ran");
            assert!(!other, "another thread is not marked");
        }
        assert!(!RtScope::is_inside());
    }
}
