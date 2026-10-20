//! Limitations, and where memory can grow.
//!
//! # A loan is for one iteration
//!
//! A [`Loan`](crate::Loan) keeps the value it borrowed alive. Held for one
//! iteration of a loop, it delays one free by one publish at most. Parked (kept
//! across iterations, stored in a struct, or passed to `mem::forget`), it
//! pins memory:
//!
//! - **A parked slot loan** pins exactly the value it borrowed, and occupies
//!   one of the cell's `SLOTS` slots until it drops.
//! - **A parked overflow loan** pins every value that was current while its
//!   epoch was open (usually one or two), and occupies that epoch.
//!
//! **Do not hold a loan across iterations of a deadline loop.** The type
//! cannot enforce that; it is `!Send` and borrows the cell, which keeps it on
//! one thread and inside the cell's lifetime, but not inside one iteration.
//!
//! # The overflow path
//!
//! A cell has `SLOTS` reader slots. A lend that finds them all taken does not
//! fail: it registers in the current overflow epoch instead, which is just as
//! wait-free and allocation-free, and coarser in what it pins. A future
//! version replaces this fallback with registered per-reader slots, so that a
//! reader has a single behaviour.
//!
//! # When every epoch is parked
//!
//! The bound on the retired list rests on a free epoch being available at
//! publish time. If every epoch but the current one is sealed and still has a
//! parked overflow loan in it (`EPOCHS - 1` parked loans in different epochs,
//! which takes more than `SLOTS` loans held across several publishes), then
//! a publish cannot advance: the current epoch stays open, and it pins every
//! value retired from then on. Memory grows by one value per publish until one
//! of those loans drops. Nothing is unsound in that state, and the next
//! publish after a drop reclaims everything.
//!
//! The cell reports it rather than acting on it:
//! [`retired_len`](crate::ArcLend::retired_len) grows, and
//! [`epoch_stalls`](crate::ArcLend::epoch_stalls) counts each publish that
//! could not advance. It does not assert, log or panic: what to do about a
//! leak (a debug assertion at a threshold, a metric, nothing) is the host's
//! policy. See [`patterns`](super::patterns).
//!
//! # Where a free can still happen
//!
//! - **The cell's own drop.** The cell owns the current value and the retired
//!   list. Dropping the last `Arc<ArcLend<_>>` frees them on whichever thread
//!   drops it, so a reader that owns the last handle to a cell will free.
//!   Keep a publisher-side handle alive.
//! - **The publisher's thread.** Every retired value's destructor runs in
//!   [`publish`](crate::ArcLend::publish), after the lock is released. A slow
//!   or panicking destructor is the publisher's cost. A panic propagates out
//!   of `publish` and leaves the cell usable.
//!
//! # What the publisher pays
//!
//! [`publish`](crate::ArcLend::publish) is not for a deadline thread:
//!
//! - it takes a mutex, which also serializes concurrent publishers;
//! - it allocates (a free list per publish, and the retired list's growth);
//! - it may spin, backing off to `yield_now`, on a reader caught between the
//!   two halves of a lend (claim and announce, or register and load). It never
//!   waits for a loan to be dropped;
//! - it filters the retired list against every slot and epoch, which is
//!   quadratic in the list's length in the worst case. The list is short
//!   unless loans are parked.
//!
//! # What the reader cannot do
//!
//! - **Keep the value.** There is no owning read, and no way to get an `Arc`
//!   out of a loan. A reader that needs to store a value past its iteration
//!   wants an owning cell (`arc-swap`, or `basedrop` with a collector).
//! - **Move a loan to another thread.** `Loan` is not `Send`.
//! - **Publish.** It can call [`publish`](crate::ArcLend::publish), but that
//!   would lock, allocate and free on the reader, defeating the type.
//!
//! # What the verification covers
//!
//! The loom model runs the shipped code, but loom does not explore load
//! buffering, so one ordering (the overflow reader's `Release` increment of
//! `loaded`) rests on the argument in [`internal`](super::internal) alone; the
//! model's mutation record says so. Miri's weak-memory emulation over the
//! stress test is the second, independent check.
