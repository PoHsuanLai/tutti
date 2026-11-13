# tutti-rt

Real-time-safe shared-memory primitives for the audio thread.

## What this is

The structures the engine's audio callback touches, and the home of the
engine's lock-free and `unsafe` runtime structures. It names no tutti crate: its dependencies are
`arc-lend`, `arrayvec` and `bytemuck` (and `loom` under `--cfg loom`), so any crate may take it without
taking the unit vocabulary.

- **`AudioThreadCell`** — interior mutability with a one-borrow-at-a-time
  contract checked in every build, reached through `&self`.
- **`RtVec`** — a capped collection through `&mut self`: refuses the write at the
  cap rather than growing.
- **`PosRing`** — frames a writer thread places ahead of the audio thread,
  indexed by position (a disk stream's ring).
- **`PackedAtomic`**, **`SeqWriter`/`SeqReader`**, **`RequestWord`**,
  **`edit_ring`**, **`DirtySet`** — a value in one atomic word, a sequence lock, a request
  bitset, a bounded single-producer single-consumer queue, and a sized set of
  dirty flags (an atomic bit per index) with no capacity to overflow.
- **`RtScope`** — a per-thread "inside a block" marker that a `Drop` impl checks.
- **`SplitMut`/`SplitRw`**, **`TaskGraph`**, **`JobGate`**, **`BlockWaits`** — the
  parallel executor's claims, task list, job lending and wait settings.
- **`with_no_denormals`** — runs a closure with subnormals flushed to zero and restores the mode after, unwinding included.
- **`level_cell()`** — a peak and an energy the audio thread folds each block
  into and one reader takes whole since its last take (a level meter's lane).
- **`Doorbell`** — wakes one parked thread from the audio thread (a futex wake only
  if a sleeper is parked), with no wake-up lost between the sleeper's check and its park.
- **`request_priority`** — asks the OS to raise the calling thread's priority and returns
  what it granted (never real time on Linux), or the refusal.
- **`ArcLend`/`Loan`/`Reader`** — the publish cell, re-exported from `arc-lend`:
  a control thread publishes non-scalar state, the audio thread only borrows it.

**The root is the API.** Every item is re-exported at the root
(`tutti_rt::RtVec`); the modules are private.

Each lock-free structure has a loom model in `tests/*_loom.rs` that runs the
shipped code:

```text
cargo test -p tutti-rt --release --test pos_ring_loom   # with the loom cfg flag set
```

## Safety

Outside the `-ffi` crates (the plugin formats, the OS MIDI APIs, the plugin
process edge), this crate and `arc-lend` hold all of the engine's `unsafe`;
every other library carries `#![forbid(unsafe_code)]`. Each site exists because the safe construct would
have cost something on the audio thread:

- `AudioThreadCell` (`cell.rs`): an `UnsafeCell` with `unsafe impl Sync`, made
  true by one atomic word (a writer bit and a reader count) checked on every
  borrow in every build. A conflicting borrow panics, or its `try_` form
  returns `None`, instead of aliasing.
- `edit_ring` (`edits.rs`): single-producer single-consumer slots of
  `UnsafeCell<u64>`, each touched only on its side of the `Release`/`Acquire`
  indices.
- `SplitMut`, `SplitRw` and `ClaimTable` (`split.rs`): one slice borrowed by
  several threads, each element or chunk claimed in an atomic table before a
  reference to it is made, so a second claim is refused rather than aliased.
  They are why `tutti-graph`'s executor can hand every node its buffers out of
  one arena under `#![forbid(unsafe_code)]`.
- `JobGate` (`job_gate.rs`): erases a borrowed job's lifetime for a pool's
  helpers; `run` neither returns nor unwinds until no helper is inside the job
  (a store-load pair with `SeqCst` fences).
- `handoff` (`handoff.rs`): one `MaybeUninit` slot moved between the two ends
  under a state word.
- `with_no_denormals` (`denormals.rs`): saves, sets and restores the FPU
  control register with `asm!`, which has no safe spelling; its `# Soundness`
  docs say what it does and does not guarantee.
- `monotonic_ns` (`clock.rs`): the OS clock calls.

`arc-lend`'s `ArcLend` holds the rest: an `AtomicPtr` to an `Arc::into_raw`
pointer that a `Loan` dereferences under a hazard-slot protocol, so the audio
thread can never free a value (the reason it exists instead of `ArcSwap`).

What checks it: a loom model per lock-free structure, run against the shipped
code (`tests/*_loom.rs` here, `arc-lend/tests/loom.rs`), and miri over this
crate's unit tests and `arc-lend`'s, in CI's `loom` and `miri` jobs (`cargo
xtask loom`, `cargo xtask miri`). Miri executes no FFI and no x86 intrinsics,
so the denormal register tests are skipped under it, and it perturbs libm
results, so a green run says nothing about numerics. The plugin formats'
`unsafe` is answered structurally instead: a plugin that corrupts memory
takes down the out-of-process `plugin-server`, not the host.

## License

MIT OR Apache-2.0
