//! Limitations, and where memory can grow.
//!
//! # A loan is for one iteration
//!
//! A [`Loan`](crate::Loan) keeps the value it borrowed alive. Held for one
//! iteration of a loop, it delays one free by one publish at most. Parked
//! (kept across iterations, or passed to `mem::forget`), it pins exactly the
//! value it borrowed until it drops (a forgotten one, until its reader's slot
//! is handed to a new reader).
//!
//! Parked loans pin **at most one value per registered reader**, however many
//! publishes pass: a reader has at most one live loan, and an announcement
//! pins one value. The type cannot keep a loan inside one iteration (it is
//! `!Send` and borrows its reader, which keeps it on one thread and inside the
//! reader's lifetime, not inside an iteration), so **do not hold a loan across
//! iterations of a deadline loop.**
//!
//! # A reader preempted mid-lend
//!
//! A lend writes a claim, then loads the pointer, then announces what it
//! loaded. Between the claim and the announcement the publisher does not know
//! which value the reader will hold, and does not wait to find out: it keeps
//! every value retired since the scan before the claim. If the reader is
//! descheduled there, every publish in that window adds one retired value the
//! cell cannot free yet. The window is two instructions long, so this needs a
//! preemption at exactly that point; the first publish or
//! [`reclaim`](crate::ArcLend::reclaim) after the reader announces frees all
//! of it but the value the reader holds.
//!
//! This is the cost of a publisher that never waits on a reader. The bound is
//! the number of publishes during the preemption, not a fixed count. A
//! publisher that publishes at a bounded rate bounds it in time.
//!
//! The cell reports rather than acting on it:
//! [`retired_len`](crate::ArcLend::retired_len), and the count
//! [`reclaim`](crate::ArcLend::reclaim) returns, grow. It does not assert, log
//! or panic: what to do about a count that stays high (a debug assertion at a
//! threshold, a metric, nothing) is the host's policy. See
//! [`patterns`](super::patterns).
//!
//! # The slot list keeps its high-water mark
//!
//! Each registered reader owns one slot, a cache line (128 bytes on x86-64 and
//! aarch64, 64 elsewhere). A dropped reader's slot is reused by the next
//! registration, but the list never shrinks: a cell that once had a thousand
//! readers at once keeps a thousand slots, and every publish scans them all
//! (see [`performance`](super::performance)). Registering is the control side:
//! it takes the publisher's mutex and may allocate a block.
//!
//! # Where a free can still happen
//!
//! - **The cell's own drop.** The cell owns the current value and the retired
//!   list, and every [`Reader`](crate::Reader) holds an `Arc` of the cell. The
//!   last `Arc` to drop frees them on whichever thread drops it, which can be
//!   a reader's. Keep a publisher-side handle alive for as long as any reader
//!   exists.
//! - **The publisher's thread.** Every retired value's destructor runs in
//!   [`publish`](crate::ArcLend::publish) or
//!   [`reclaim`](crate::ArcLend::reclaim), after the lock is released. A slow
//!   or panicking destructor is the publisher's cost. A panic propagates out
//!   and leaves the cell usable.
//!
//! # What the publisher pays
//!
//! [`publish`](crate::ArcLend::publish) is not for a deadline thread:
//!
//! - it takes a mutex, which also serializes concurrent publishers and
//!   registrations;
//! - it may allocate: the retired list's amortized growth, and a free buffer
//!   when the reused one is too small or another publisher holds it;
//! - it reads every slot (linear in the high-water mark of readers) and
//!   filters the retired list once (linear in its length, with a logarithmic
//!   lookup per value in the announced addresses);
//! - it never waits on a reader.
//!
//! # What the reader cannot do
//!
//! - **Keep the value.** There is no owning read, and no way to get an `Arc`
//!   out of a loan. A reader that needs to store a value past its iteration
//!   wants an owning cell (`arc-swap`, or `basedrop` with a collector).
//! - **Move a loan to another thread.** `Loan` is not `Send`. The `Reader` is
//!   `Send`, so it can be registered on one thread and moved to the one that
//!   lends.
//! - **Share a reader.** `Reader` is not `Sync`, and lends through
//!   `&mut self`: one live loan per reader. Nesting reads of one cell takes one
//!   reader per level.
//! - **Register on a deadline.** [`ArcLend::reader`](crate::ArcLend::reader)
//!   and `Reader::clone` take the publisher's lock and may allocate. Register
//!   ahead of time.
//! - **Publish.** It can call [`publish`](crate::ArcLend::publish), but that
//!   would lock, allocate and free on the reader, defeating the type.
//!
//! # What the verification covers
//!
//! The loom model runs the shipped code against two or three threads with a
//! slot block of two, so it covers slot reuse and list growth, but only a few
//! publishes per model. Loom does not model everything C++ allows (it does
//! not explore load buffering, and its `SeqCst` fences are stronger), so a
//! passing model supports the argument in [`internal`](super::internal)
//! rather than replacing it. Miri's weak-memory emulation over the small
//! stress test is the second, independent check.
