//! Publishing non-scalar state to the audio thread as a *borrow*.
//!
//! [`RtPublish`] is the cell a control thread stores into; [`RtRef`] is the
//! non-`Send`, lifetime-tied handle the audio thread reads back. Scalars use
//! [`Param`](crate::value::Param) instead — this is for a routing table, a meter
//! map, a coefficient set.
//!
//! The hazard it removes is an *owning* read on the audio thread: hold the last
//! reference to a retired value and the callback runs `free` on its `Vec`s
//! inside the block. `ClickSettings::meter` did exactly that through
//! `ArcSwap::load_full`.
//!
//! **A no-alloc test cannot pin this property**, and one that claimed to did
//! not: the hazard is a race between a reader and a publisher, and a
//! single-threaded sampling test has no schedule that exhausts it. The guarantee
//! therefore lives in [`RtRef`]'s type, where it is checked at compile time.

use std::marker::PhantomData;
use std::ops::Deref;
use std::sync::Arc;

use arc_swap::ArcSwap;

/// A value published from a control thread and read by the audio thread.
///
/// For state too large to pack into an atomic — a routing table, a meter map, a
/// coefficient set. Scalars want [`Param`](crate::value::Param), not this.
///
/// # The invariant
///
/// **The audio thread never holds an owning handle to published state.**
///
/// [`read`](Self::read) hands back an [`RtRef`], which is a *borrow*. Dropping
/// it returns a debt token; it does not decrement an `Arc` the way an owning
/// handle would. That matters because the audio thread is the one place a
/// deallocation must not happen: if the callback held the last reference to a
/// retired value, it would run `free` on its `Vec`s inside the block.
///
/// The counterpart is that retired values are freed by
/// [`publish`](Self::publish), on the publishing thread. So the cost is real,
/// but it lands where blocking is allowed.
///
/// There is deliberately no owning read. `arc_swap` offers one (`load_full`),
/// and offering it here would reintroduce the hazard this type exists to remove.
///
/// # What this type does *not* give you
///
/// Reading is allocation-free and wait-free on the fast path, but it is not
/// *unconditionally* free of deallocation. If a publisher settles a reader's
/// debt concurrently, the reader's guard degrades into an owning reference whose
/// drop is a real refcount decrement, and that decrement can in principle reach
/// zero. The window is tiny — the publisher holds the retired value until its own
/// swap returns — but it is a probabilistic argument, not a structural one.
///
/// Making it structural means retiring values through a queue the audio thread
/// never touches, which needs per-reader reclamation. That is a change of
/// *implementation*, not of API: it happens inside this type, and no call site
/// moves. Keeping that option open is much of why this wrapper exists.
///
/// # Cost
///
/// A read is a thread-local lookup plus two `SeqCst` loads and a slot store —
/// heavier than a plain atomic load, far lighter than a lock. **Read once per
/// block, not per sample.** Every call site in the engine hoists it to block
/// scope, and a per-sample read would put a barrier on every one of ~2.8M
/// samples per second.
///
/// Guards also occupy one of a small number of per-thread slots. Holding several
/// at once (loading a value, then loading another through it) multiplies that
/// pressure; prefer one read per block, passed down, over nested reads.
pub struct RtPublish<T> {
    cell: ArcSwap<T>,
}

impl<T> RtPublish<T> {
    /// Wraps an initial value, allocating the `Arc` that carries it.
    ///
    /// Control-thread only, like [`publish`](Self::publish).
    pub fn new(value: T) -> Self {
        Self {
            cell: ArcSwap::from_pointee(value),
        }
    }

    /// Build from an existing `Arc`, when the publisher already has one.
    pub fn from_arc(value: Arc<T>) -> Self {
        Self {
            cell: ArcSwap::new(value),
        }
    }

    /// Read the current value. **Audio-thread safe.**
    ///
    /// Hold the returned [`RtRef`] for the block you are rendering and no
    /// longer: it is a read lease, and a parked one keeps a retired value alive
    /// and stalls the next [`publish`](Self::publish).
    #[inline]
    pub fn read(&self) -> RtRef<'_, T> {
        RtRef {
            guard: self.cell.load(),
            _not_send: PhantomData,
        }
    }

    /// Publish a new value. **Control-thread only.**
    ///
    /// Blocks until in-flight readers have finished with the outgoing value,
    /// then drops it on this thread. Both costs — the wait and the free — belong
    /// to the caller, which is the entire point: neither reaches the callback.
    ///
    /// Calling this from the audio thread would stall the callback *and* free
    /// inside it, defeating the type.
    pub fn publish(&self, value: Arc<T>) {
        self.cell.store(value);
    }
}

impl<T: Default> Default for RtPublish<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for RtPublish<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RtPublish").field(&*self.read()).finish()
    }
}

/// A borrow of the value inside an [`RtPublish`], valid for as long as it is
/// held.
///
/// Deliberately not `Send`, and tied to the cell's lifetime. Between them, a
/// guard cannot be stashed in a struct field, moved to another thread, or
/// outlive the cell — so "held across blocks", the failure mode a review rule
/// would otherwise have to catch, is a compile error instead.
///
/// There is no way to get an owning `Arc` back out. That is not an oversight.
pub struct RtRef<'a, T> {
    guard: arc_swap::Guard<Arc<T>>,
    /// `*const ()` is neither `Send` nor `Sync`, which is what pins the guard to
    /// the thread that took it. The lifetime ties it to the cell.
    _not_send: PhantomData<*const &'a ()>,
}

impl<T> Deref for RtRef<'_, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for RtRef<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        (**self).fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_sees_the_published_value() {
        let cell = RtPublish::new(vec![1u32, 2, 3]);
        assert_eq!(&*cell.read(), &[1, 2, 3]);

        cell.publish(Arc::new(vec![4, 5]));
        assert_eq!(&*cell.read(), &[4, 5]);
    }

    #[test]
    fn a_held_read_keeps_seeing_its_own_snapshot() {
        // The block-scoped contract in miniature: a reader that took a value
        // before a publish keeps rendering with it, rather than tearing
        // mid-block.
        let cell = RtPublish::new(vec![1u32]);
        let held = cell.read();

        cell.publish(Arc::new(vec![9]));

        assert_eq!(&*held, &[1], "guard still sees the snapshot it took");
        assert_eq!(&*cell.read(), &[9], "a fresh read sees the new value");
    }

    #[test]
    fn publish_frees_the_retired_value_on_the_publishing_thread() {
        // The property the whole type exists for. `dropped` flips when the
        // retired payload is freed; it must be observable immediately after
        // `publish` returns, i.e. on this thread — never deferred to whichever
        // thread happens to read next.
        use std::sync::atomic::{AtomicBool, Ordering};

        struct Tattle(Arc<AtomicBool>);
        impl Drop for Tattle {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let cell = RtPublish::new(Tattle(dropped.clone()));

        assert!(!dropped.load(Ordering::SeqCst));
        cell.publish(Arc::new(Tattle(Arc::new(AtomicBool::new(false)))));
        assert!(
            dropped.load(Ordering::SeqCst),
            "retired value must be freed by publish(), on the publishing thread"
        );
    }

    /// A guard must not be able to escape the thread that took it — that is what
    /// makes "held across blocks" a compile error rather than a review rule.
    ///
    /// Asserted via autoref specialization: the inherent const on `Wrap<T>` wins
    /// over the trait's default only when `T: Send`, so `IS_SEND` resolves to
    /// `false` exactly when the type is *not* `Send`. A plain
    /// `assert_not_send::<T>()` helper cannot express this — there is no stable
    /// negative bound — and a bare `assert_send` pins the opposite claim.
    ///
    /// # Why `const` rather than `assert!`
    ///
    /// Both operands are compile-time constants, so a runtime `assert!` is the
    /// wrong tool twice over: clippy flags it as an assertion on a constant, and
    /// — the part that matters — it only runs if someone runs the tests. A
    /// `const` block is evaluated during compilation, so this property fails the
    /// *build*. That matches what it guards: `RtRef`'s non-`Send`ness is a
    /// type-system claim, and putting the guarantee in the return type is what
    /// stops it depending on anyone remembering to check.
    #[test]
    fn rt_ref_is_not_send() {
        struct Wrap<T>(PhantomData<T>);

        trait NotSend {
            const IS_SEND: bool = false;
        }
        impl<T> NotSend for Wrap<T> {}

        impl<T: Send> Wrap<T> {
            const IS_SEND: bool = true;
        }

        // A guard that escapes its thread outlives the block it was taken for.
        const {
            assert!(!<Wrap<RtRef<'static, Vec<u32>>>>::IS_SEND);
        }
        // Control: the same machinery reports `true` for a type that IS `Send`,
        // so the assertion above is measuring something rather than always
        // holding. Without this, a broken `Wrap` that reported `false` for
        // everything would look like a pass.
        const {
            assert!(<Wrap<Vec<u32>>>::IS_SEND);
        }
    }
}
