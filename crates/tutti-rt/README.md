# tutti-rt

Real-time-safe shared-memory primitives for the audio thread.

## What this is

The structures the engine's audio callback touches, and the home of the
engine's lock-free and `unsafe` runtime structures. It names no tutti crate: its dependencies are `arc-lend`, `arrayvec` and
`bytemuck` (and `loom` under `--cfg loom`), so any crate may take it without
taking the unit vocabulary.

- **`AudioThreadCell`** — interior mutability with a one-borrow-at-a-time
  contract checked in every build, reached through `&self`.
- **`RtVec`** — a capped collection through `&mut self`: refuses the write at the
  cap rather than growing.
- **`PosRing`** — frames a writer thread places ahead of the audio thread,
  indexed by position (a disk stream's ring).
- **`PackedAtomic`**, **`SeqWriter`/`SeqReader`**, **`RequestWord`**,
  **`edit_ring`** — a value in one atomic word, a sequence lock, a request
  bitset, and a bounded single-producer single-consumer queue.
- **`RtScope`** — a per-thread "inside a block" marker that a `Drop` impl checks.
- **`SplitMut`/`SplitRw`**, **`TaskGraph`**, **`JobGate`**, **`BlockWaits`** — the
  parallel executor's claims, task list, job lending and wait settings.
- **`ScopedNoDenormals`** — flushes subnormals to zero for the length of a block.
- **`ArcLend`/`Loan`/`Reader`** — the publish cell, re-exported from `arc-lend`:
  a control thread publishes non-scalar state, the audio thread only borrows it.

**The root is the API.** Every item is re-exported at the root
(`tutti_rt::RtVec`); the modules are private.

Each lock-free structure has a loom model in `tests/*_loom.rs` that runs the
shipped code:

```text
cargo test -p tutti-rt --release --test pos_ring_loom   # with the loom cfg flag set
```

## License

MIT OR Apache-2.0
