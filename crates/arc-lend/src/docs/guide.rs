//! How to use [`ArcLend`](crate::ArcLend) well, and what to watch for.
//!
//! # Using it well
//!
//! ## Store the reader, not the reader and the cell
//!
//! A [`Reader`](crate::Reader) holds an `Arc` of its cell, and
//! [`Reader::cell`](crate::Reader::cell) returns it. So a structure that
//! reads a cell can keep the reader alone, and reach the cell through it to
//! publish, to call [`current`](crate::ArcLend::current), or to register a
//! reader for a copy of itself. Keeping both means two handles to keep in
//! step.
//!
//! ```
//! use std::sync::Arc;
//! use arc_lend::{ArcLend, Reader};
//!
//! struct Voice {
//!     window: Reader<(f64, f64)>, // the only handle: the cell is behind it
//! }
//!
//! let voice = Voice { window: Arc::new(ArcLend::new((0.0, 4.0))).reader() };
//! voice.window.cell().publish((1.0, 4.0));
//! assert_eq!(*voice.window.cell().current(), (1.0, 4.0));
//! ```
//!
//! ## Lend once per iteration, and pass `&T` down
//!
//! Take one [`Loan`](crate::Loan) at the top of the reader's loop, and pass
//! `&T` to everything below it. The loan drops at the end of the iteration,
//! before the loop waits for its next turn.
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
//! Lending once per element also works, but each lend costs more than an
//! atomic load, so it adds up.
//!
//! ## One reader per level of nesting
//!
//! A reader has one live loan at a time. Code that reads the same cell at two
//! levels of one iteration, say a voice inside an instrument that both read
//! one table, holds a reader per level. Nested lends then cost what flat ones
//! do.
//!
//! ```
//! use std::sync::Arc;
//! use arc_lend::ArcLend;
//!
//! let table = Arc::new(ArcLend::new(vec![1.0f32, 2.0]));
//! let (mut outer, mut inner) = (table.reader(), table.reader());
//! let a = outer.lend();
//! let b = inner.lend(); // a second reader
//! assert_eq!(a[0] + b[1], 3.0);
//! ```
//!
//! ## Cloning a structure clones its readers
//!
//! Cloning a `Reader` registers a new reader of the same cell, so a cloned
//! structure gets readers of its own. Registering may allocate, so clone on
//! the writer's side. To point a copy at a *different* cell, call
//! [`reader`](crate::ArcLend::reader) on that cell instead.
//!
//! ## Build the next value whole, on the writer's thread
//!
//! Build the next value completely, then publish it. Readers see either the
//! old value or the new one, never a mix, and the allocations happen on the
//! writer. If a reader needs to know that something changed, put a
//! generation number in the value and compare it with the one it last saw.
//!
//! ## Derive the next value from the current one
//!
//! The writer reads back what it published with
//! [`current`](crate::ArcLend::current), rather than keeping a copy beside
//! the cell. The `Arc` it returns frees wherever it is dropped last, so it is
//! for the writer only.
//!
//! ```
//! use std::sync::Arc;
//! use arc_lend::ArcLend;
//!
//! let gains = Arc::new(ArcLend::new(vec![1.0f32, 1.0]));
//! // Halve every gain.
//! let next: Vec<f32> = gains.current().iter().map(|g| g * 0.5).collect();
//! gains.publish(next);
//! assert_eq!(*gains.current(), [0.5, 0.5]);
//! ```
//!
//! ## Reclaim on a tick you already have
//!
//! An old value a reader was still using when it was replaced is freed by a
//! later publish. A writer with nothing new to publish can call
//! [`reclaim`](crate::ArcLend::reclaim) instead. When nothing is waiting it
//! is only a lock and a length check, so it is fine to call it on every tick
//! of a loop the writer already runs, for every cell.
//!
//! ```
//! use std::sync::Arc;
//! use arc_lend::ArcLend;
//!
//! let cell = Arc::new(ArcLend::new(vec![0u8; 1 << 20]));
//! let mut reader = cell.reader();
//! let held = reader.lend();
//! cell.publish(vec![1u8; 16]); // the big value is still on loan
//! drop(held);
//! assert_eq!(cell.retired_len(), 1); // returned, but not yet freed
//!
//! // Later, on the writer's tick:
//! assert_eq!(cell.reclaim(), 0); // the big one is freed here
//! ```
//!
//! ## Decide your own policy for a backlog
//!
//! The cell never asserts, logs or panics when old values pile up. If you
//! want to know, poll [`retired_len`](crate::ArcLend::retired_len), or the
//! count [`reclaim`](crate::ArcLend::reclaim) returns, and decide: a debug
//! assertion, a metric, a log line. Loans keep at most one value per reader
//! waiting, so a count well above the number of readers means a reader was
//! paused mid-read (see below), or a loan is being kept too long.
//!
//! ```
//! # use arc_lend::ArcLend;
//! # let cell = ArcLend::new(0u32);
//! # cell.publish(1);
//! const LAGGING: usize = 1024; // your threshold, not the crate's
//! let waiting = cell.reclaim();
//! debug_assert!(waiting < LAGGING, "{waiting} old values still waiting");
//! ```
//!
//! ## A value that may be absent
//!
//! Use `ArcLend<Option<Arc<U>>>`, or `ArcLend<Option<U>>` if `U` is small.
//! Readers still only borrow: the inner `Arc` belongs to the published value,
//! which only the writer frees.
//!
//! ## Many cells, one thread
//!
//! A thread reading ten cells holds ten readers, one per cell. Prefer that to
//! one cell holding a large aggregate when the parts change independently:
//! each publish then copies and retires only the part that changed.
//!
//! # What to watch for
//!
//! ## Keep a loan for one iteration
//!
//! A [`Loan`](crate::Loan) keeps its value alive. Held for one iteration, it
//! delays one free by one publish at most. Kept longer, or passed to
//! `mem::forget`, it keeps that one value alive until it drops (a forgotten
//! one, until its reader's slot goes to a new reader). That is at most one
//! value per reader, however many times the writer publishes. The types keep
//! a loan on its thread and inside its reader's lifetime, but they cannot
//! keep it inside one iteration: that part is up to you.
//!
//! ## A paused reader makes the writer hold on to old values
//!
//! A lend takes a few instructions. If the reader's thread is paused in the
//! middle of one, the writer cannot tell which value it is about to borrow,
//! and since it never waits, it keeps every value it replaces until the
//! reader moves on. The first publish or reclaim after that frees all of
//! them but the one the reader holds.
//!
//! So the backlog is bounded by how often you publish while a reader is
//! paused, not by a fixed count. A writer that publishes at a bounded rate
//! bounds it in time. This is the price of a writer that never waits on a
//! reader; [`design`](super::design), §6, explains why.
//!
//! ## The cell never gives slots back
//!
//! Each registered reader takes one slot, a cache line of memory. A dropped
//! reader's slot is reused by the next registration, but the cell never
//! shrinks: one that once had a thousand readers at once keeps room for
//! 1024, and every publish looks at all of them.
//!
//! ## Destructors run on the writer
//!
//! Old values are dropped inside [`publish`](crate::ArcLend::publish) and
//! [`reclaim`](crate::ArcLend::reclaim), after the cell's lock is released. A
//! slow destructor slows the writer; one that panics propagates out of the
//! call and leaves the cell usable. The other place a value can be freed is
//! the cell's own drop (see [`ArcLend`](crate::ArcLend), "Dropping the
//! cell").
//!
//! ## What a reader cannot do
//!
//! - **Keep the value.** A loan cannot be turned into an `Arc`. A reader that
//!   needs to store a value past its iteration wants an owning cell, such as
//!   `arc-swap`, or `basedrop` with a collector.
//! - **Send a loan to another thread.** `Loan` is not `Send`. The `Reader`
//!   is, so register it on one thread and move it to the one that reads.
//! - **Share a reader.** `Reader` is not `Sync`, and has one live loan at a
//!   time. Reading one cell at two levels takes two readers.
//! - **Register, clone or publish on its deadline.** All three take the
//!   cell's lock and may allocate. Publishing also frees.
