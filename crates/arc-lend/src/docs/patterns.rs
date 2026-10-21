//! Common patterns.
//!
//! # Register ahead, lend on the deadline
//!
//! A thread that reads the cell gets a [`Reader`](crate::Reader) from
//! [`ArcLend::reader`](crate::ArcLend::reader) before its deadline loop
//! starts: registering takes the publisher's lock and may allocate, lending
//! never does. The reader is `Send`, so the usual shape registers it on the
//! control side and moves it into the thread, or into the long-lived struct
//! the deadline code runs in.
//!
//! # One lend per iteration, passed down
//!
//! The loop body takes one [`Loan`](crate::Loan) at the top and passes `&T`
//! to everything below it. The loan drops at the end of the iteration,
//! before the loop sleeps or waits for its next deadline.
//!
//! ```
//! use std::sync::Arc;
//! use arc_lend::ArcLend;
//!
//! struct Coeffs { taps: Vec<f32> }
//!
//! fn filter(c: &Coeffs, input: &[f32], out: &mut [f32]) {
//!     for (o, i) in out.iter_mut().zip(input) {
//!         *o = i * c.taps[0];
//!     }
//! }
//!
//! let cell = Arc::new(ArcLend::new(Coeffs { taps: vec![0.5] }));
//! let mut reader = cell.reader(); // ahead of the loop
//! let (input, mut out) = ([1.0f32; 64], [0.0f32; 64]);
//! for _iteration in 0..3 {
//!     let coeffs = reader.lend(); // once, at the top
//!     filter(&coeffs, &input, &mut out); // `&Coeffs` below, never a loan
//! } // dropped before the next iteration
//! assert_eq!(out[0], 0.5);
//! ```
//!
//! Lending per element is correct but wasteful: each lend is a fence and
//! four stores.
//!
//! # Nesting: one reader per level
//!
//! A reader has one live loan (`lend` takes `&mut self`). Code that reads the
//! same cell at two levels of one iteration, say a voice inside an
//! instrument that both read one table, holds a reader per level. Each level
//! has its own slot, so nested lends cost what flat ones do.
//!
//! ```
//! use std::sync::Arc;
//! use arc_lend::ArcLend;
//!
//! let table = Arc::new(ArcLend::new(vec![1.0f32, 2.0]));
//! let (mut outer, mut inner) = (table.reader(), table.reader());
//! let a = outer.lend();
//! let b = inner.lend(); // a second reader, a second slot
//! assert_eq!(a[0] + b[1], 3.0);
//! ```
//!
//! # Readers in a structure that is forked
//!
//! `Reader::clone` registers a new reader of the same cell, with a slot of
//! its own: a cloned structure holds independent readers. Cloning takes the
//! publisher's lock, so clone on the control side, as you would any other
//! allocation. To hand a structure a reader of a *different* cell, call
//! [`reader`](crate::ArcLend::reader) on that cell.
//!
//! # Build off the reader, publish whole
//!
//! The publisher builds the next value completely, wraps it in an `Arc`, and
//! publishes. Readers see either the old value or the new one, never a mix,
//! and the build's allocations happen on the publisher. If the reader needs
//! to know that something changed, put a generation number in the value and
//! compare it with the one it last saw.
//!
//! # Keep a publisher-side handle
//!
//! Share the cell as `Arc<ArcLend<T>>`, and keep one clone on the publisher's
//! side for as long as any reader exists. Each reader holds an `Arc` of the
//! cell, and whoever drops the *last* one drops the cell, with the current
//! value and the retired list. If that is a reader, the free runs there (see
//! [`limitations`](super::limitations)).
//!
//! # Reclaim without a new value
//!
//! Retired values are freed by the next publish after their readers let go.
//! A publisher with nothing new to publish calls
//! [`reclaim`](crate::ArcLend::reclaim) from a periodic task; it returns how
//! many retired values are still pinned. With nothing retired it returns at
//! once (the lock and a length check), so a publisher that calls it every
//! tick of its control loop, for every cell it publishes to, pays for a scan
//! only when there is something to free.
//!
//! ```
//! use std::sync::Arc;
//! use arc_lend::ArcLend;
//!
//! let cell = Arc::new(ArcLend::new(vec![0u8; 1 << 20]));
//! let mut reader = cell.reader();
//! let held = reader.lend();
//! cell.publish(Arc::new(vec![1u8; 16])); // the big value is still held
//! drop(held);
//! assert_eq!(cell.retired_len(), 1); // let go of, but not yet freed
//!
//! // Later, from a periodic task on the publisher's side:
//! assert_eq!(cell.reclaim(), 0); // the big one goes, here
//! ```
//!
//! # Poll the observables, with your own policy
//!
//! The cell does not assert, log or panic when reclamation lags. A host that
//! wants to know polls [`retired_len`](crate::ArcLend::retired_len), or the
//! count [`reclaim`](crate::ArcLend::reclaim) returns, from the publisher's
//! side and decides: a debug assertion at a threshold of its choosing, a
//! metric, a log line. Parked loans pin at most one value per reader, so a
//! threshold above the reader count is a parked loan plus a preempted lend,
//! or a leak in the host.
//!
//! ```
//! # use std::sync::Arc;
//! # use arc_lend::ArcLend;
//! # let cell = ArcLend::new(0u32);
//! # cell.publish(Arc::new(1));
//! const LAGGING: usize = 1024; // this host's threshold, not the crate's
//! let pinned = cell.reclaim();
//! debug_assert!(pinned < LAGGING, "{pinned} retired values still pinned");
//! ```
//!
//! # A nullable slot
//!
//! A value that may be absent is an `ArcLend<Option<Arc<U>>>`, or, if `U` is
//! small, `ArcLend<Option<U>>`. Both keep the reader's borrow-only property:
//! the inner `Arc` belongs to the published value, which only the publisher
//! frees.
//!
//! # Many cells, one thread
//!
//! Slots are per cell, so a thread reading ten cells holds ten readers, one
//! slot in each. Prefer that to one cell of a large aggregate when the parts
//! change independently: a publish then copies and retires only the part that
//! changed.
