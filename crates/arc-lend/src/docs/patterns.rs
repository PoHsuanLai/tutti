//! Common patterns.
//!
//! # One lend per iteration, passed down
//!
//! The reader's loop body takes one [`Loan`](crate::Loan) at the top and
//! passes `&T` to everything below it. The loan drops at the end of the
//! iteration, before the loop sleeps or waits for its next deadline.
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
//! let (input, mut out) = ([1.0f32; 64], [0.0f32; 64]);
//! for _iteration in 0..3 {
//!     let coeffs = cell.lend(); // once, at the top
//!     filter(&coeffs, &input, &mut out); // `&Coeffs` below, never a loan
//! } // dropped before the next iteration
//! assert_eq!(out[0], 0.5);
//! ```
//!
//! Lending per element is correct but wasteful: each lend is a CAS and a
//! fence. Lending the same cell twice in one iteration uses two slots, and
//! past `SLOTS` of them the overflow path's coarser reclamation.
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
//! side for as long as any reader holds one. Whoever drops the *last* handle
//! drops the cell, and with it the current value and the retired list. If
//! that is the reader, the free runs there (see
//! [`limitations`](super::limitations)).
//!
//! # Reclaim without a new value
//!
//! Retired values are freed by the next publish after their readers let go.
//! A publisher that has nothing new to publish but wants the memory back
//! publishes the current value again. It needs its own `Arc` of it:
//!
//! ```
//! use std::sync::Arc;
//! use arc_lend::ArcLend;
//!
//! let cell = ArcLend::new(vec![0u8; 1 << 20]);
//! let held = cell.lend();
//! let latest = Arc::new(vec![1u8; 16]);
//! cell.publish(Arc::clone(&latest)); // the big value is still held
//! drop(held);
//! assert_eq!(cell.retired_len(), 1); // let go of, but not yet freed
//!
//! // Later, from a periodic task on the publisher's side:
//! cell.publish(Arc::clone(&latest)); // same value, and the big one goes
//! assert_eq!(cell.retired_len(), 0);
//! ```
//!
//! # Poll the observables, with your own policy
//!
//! The cell does not assert, log or panic when reclamation stalls. A host that
//! wants to know polls [`retired_len`](crate::ArcLend::retired_len) and
//! [`epoch_stalls`](crate::ArcLend::epoch_stalls) from the publisher's side and
//! decides: a debug assertion at a threshold of its choosing, a metric, a log
//! line.
//!
//! ```
//! # use std::sync::Arc;
//! # use arc_lend::ArcLend;
//! # let cell = ArcLend::new(0u32);
//! # cell.publish(Arc::new(1));
//! const STALLED: usize = 1024; // this host's threshold, not the crate's
//! debug_assert!(
//!     cell.retired_len() < STALLED,
//!     "a loan has been parked across {} publishes",
//!     cell.retired_len(),
//! );
//! ```
//!
//! # A nullable slot
//!
//! A value that may be absent is an `ArcLend<Option<Arc<U>>>`, or, if `U` is
//! small, `ArcLend<Option<U>>`. Both keep the reader's borrow-only property:
//! the inner `Arc` belongs to the published value, which only the publisher
//! frees.
//!
//! # Many cells, one reader
//!
//! The slots are per cell, so a reader lending ten different cells in one
//! iteration uses one slot in each. Prefer that to one cell of a large
//! aggregate when the parts change independently: a publish then copies and
//! retires only the part that changed.
