# 012 — The `unsafe` policy

Tutti's pitch is that it hosts plugins *safely*, out of process. A project
making that claim should be able to say where its own `unsafe` is, why each
piece is there, and what checks it. This is that statement.

## Where it is

Counted as lines mentioning `unsafe`, so blocks, `unsafe fn` and `unsafe impl`
together — an upper bound, not a block count:

| area | ~lines | what it is |
|---|---|---|
| `tutti-*-host`, `tutti-*-ffi` | 1,519 | VST2/VST3/CLAP/AU C ABIs |
| `crates/tutti-plugin-server/` | 52 | the subprocess side: shm, signals |
| `crates/tutti-plugin/` | 46 | shm slab, IPC transport |
| `crates/tutti-midi-hardware/` | 33 | CoreMIDI / ALSA seq-UMP |
| `tutti-core`, `-rt`, `-graph`, `-io`, `-cpal`, `-mod`, `-export` | 32 | see below (was 46; `tutti-node`'s 14 went with it in doc 013) |
| `tutti-nodes`, `-spatial`, `-sampler`, `-polysynth`, `-soundfont`, `-dsp` | 14 | see below |
| `crates/tutti-plugin-types/` | 7 | bundle/ABI value types |

**Roughly 95% of it is FFI**, and that is inherent: you cannot host a VST3
without speaking COM, or reach CoreMIDI without calling it. The interesting
question is the other 5%.

## The non-FFI `unsafe`, in full

There are seven sites, in `tutti-rt` and `arc-lend`. Each exists because a
safe construct would have cost something on the audio thread, and each is
named here so the list can be checked against the tree
(`grep -rwl unsafe crates/tutti-rt/src crates/arc-lend/src`).

- **`tutti-rt/src/cell.rs`** — `AudioThreadCell`: an `UnsafeCell` plus
  `unsafe impl Sync`, with a borrow check in every build: one atomic word,
  a writer bit plus a reader count, taken by a compare-exchange per borrow
  and released by its guard's drop. Any number of shared borrows (`borrow`,
  `try_borrow`, which need `T: Sync`) or one exclusive borrow (`borrow_mut`,
  `try_borrow_mut`); a conflicting borrow panics, or its `try_` form returns
  `None`, instead of aliasing. The word is what makes the `Sync` true. The
  audio thread and the VST3 COM entry points take the `try_` forms, so a
  conflict there is a skipped step or a failure code, never a panic. The
  RT-safe alternative to a `Mutex`: the check never waits. Its `loom` model
  is `tests/cell_loom.rs`.
- **`tutti-rt/src/edits.rs`** — `edit_ring`: a single-producer
  single-consumer ring whose slots are `UnsafeCell<u64>` words, with an
  `unsafe impl Sync` and two word accesses (`write`, `read`). The producer
  touches a slot only while `head - tail < capacity`, the consumer only while
  `head > tail`, and the `Release`/`Acquire` indices order the hand-over.
  Its `loom` model is `tests/edit_ring_loom.rs` (loom's cells report an
  unordered hand-over); miri runs its unit tests.
- **`tutti-rt/src/split.rs`** — `SplitMut`, `SplitRw` and `ClaimTable`: one
  slice borrowed by several threads at once, each element (or chunk) claimed
  in an atomic table before a raw-pointer reference to it is made, so two
  claims on one element are refused instead of aliased. It holds the
  `unsafe impl Send`/`Sync`, the element and chunk references, and
  `RwView::split`'s hand-out. This is what lets `tutti-graph`'s executor
  hand every node its buffers out of one arena under
  `#![forbid(unsafe_code)]`. Its `loom` model is `tests/split_loom.rs`; miri
  runs its unit tests.
- **`tutti-rt/src/job_gate.rs`** — `JobGate`: the lifetime erasure of a
  borrowed job, so a worker pool's helpers can call a closure that borrows
  the caller's stack. `run` does not return or unwind until the gate is
  closed and no helper is inside the job; the close and a helper's entry are
  a store-load pair with `SeqCst` fences. Its `loom` model is
  `tests/job_gate_loom.rs`; miri runs its unit tests.
- **`tutti-rt/src/denormals.rs`** — `with_no_denormals`: the MXCSR/FPCR
  save, set and restore (`asm!`). There is no safe spelling of it. The function
  is safe by a stated choice and restores on unwind; its `# Soundness` docs
  say what that does and does not fix.
- **`arc-lend/src/current.rs`, `arc-lend/src/reader.rs`** — `ArcLend`
  (`tutti-rt` re-exports it): an `AtomicPtr` holding an `Arc::into_raw`
  pointer (its private `Current`, which holds every `Arc::from_raw`: its
  `swap`, `to_arc` and `Drop`), dereferenced by `Loan` under a hazard-slot
  protocol, plus `Reader`'s `unsafe impl Send`. The safe alternative
  (`ArcSwap`) could, rarely, free on the audio thread; this cannot, and that
  is the whole reason for it. The dereference's soundness rests on a `SeqCst`
  fence pair, which is a *concurrency* property, so it is checked by a loom
  model run against the shipped code (`arc-lend/tests/loom.rs`) as well as by
  miri over the crate's concurrent stress test.

Not sites: `tutti-core/src/engine.rs` (`Engine::apply_pending` is a safe
`pub fn` that panics on a conflicting borrow), `tutti-cpal` (its stream
lives on an owner thread, with no `unsafe`), and
`tutti-sampler/src/butler/prefetch.rs` (no `unsafe`). `tutti-polysynth` and
`tutti-dsp` mention it in prose only. Beyond the seven there is only
platform FFI (the native windows in `bevy-tutti/src/plugin_host/`), a
libc-version probe in `tutti-nodes`' golden test helper, and
`std::env::set_var` in one `bevy-tutti` test.

Every library with no `unsafe` carries `#![forbid(unsafe_code)]`
(`grep -rl 'forbid(unsafe_code)' crates/*/src crates/*/*/src`), `tutti-graph`
among them. It is
the one worth knowing: its executor hands every node `&[f32]` inputs and
`&mut [f32]` outputs out of one shared arena, which is the textbook reason to
reach for raw pointers. It does not — the disjoint borrows come from
`split_at_mut` over sorted slot indices and the reinterpretation from
`bytemuck`'s checked cast (`tutti-graph/src/arena.rs`), and a colouring bug
would be a panic naming the slot rather than aliasing. Rule 4 below, applied.

## The rules

1. **`unsafe` needs a `// SAFETY:` comment stating the invariant and who
   upholds it.** Not what the code does — why the precondition holds here.
2. **Concentrate it.** `edit_ring`'s `Shared` is the pattern: one wrapper carrying the
   `unsafe impl`, everything else safe around it. A crate with `unsafe`
   scattered across ten files has ten things to re-verify on every change.
3. **A safety argument that depends on a *property* must be checked where the
   property lives.** `AudioThreadCell` is the cautionary case, twice: its
   check detects a concurrent borrow, not a foreign thread, and the
   surrounding docs drifted into claiming the stronger thing; and the check
   was debug-only, so the `Sync` it justified held only in debug builds. A
   safe API whose soundness rests on a check keeps the check in every
   build.
4. **New non-FFI `unsafe` is a design question, not an implementation
   detail.** Seven sites is small enough to review one at a time; that is worth
   keeping true. Prefer the safe construct and measure before concluding it is
   too slow — the repo's rule is to check the constraint before designing
   around it.
5. **FFI `unsafe` is exempt from (4) and not from (1).** A C ABI call is
   `unsafe` by construction; the SAFETY comment still has to say what makes
   *this* call sound, especially around pointer lifetime and who frees.

## What checks it

- **Out-of-process hosting is the structural answer for plugin `unsafe`.** A
  plugin that corrupts memory takes down `plugin-server`, not the host; that
  is what `clap_crash_recovery` and `real_stall_tests` exercise against a real
  subprocess. This is the reason the 1,519-line figure above is tolerable.
- **`tutti-shm-model`** is a loom model of the shm header protocol — a
  separate crate because `--cfg loom` is global. That covers the concurrency
  argument the shm `unsafe` rests on. **`arc-lend/tests/loom.rs`**
  does the same for `ArcLend`, but against the real code: its atomics switch
  to loom's under the flag, which that crate's dependency closure tolerates.
  Both run in the `loom` CI job and `just loom`. Six of the seven `ArcLend`
  models run unbounded there (exhaustive, seconds); the seventh,
  `registering_and_dropping_readers`, which grows the slot list and reuses
  a slot, runs at a preemption bound of 4 there and at 5 under
  `just loom-full` (about 6 minutes; at 6 it did not finish in 25).
- **miri**, on the non-FFI crates, via `just miri` and the `miri` CI job.
  Installing a nightly toolchain on the machine that wrote this failed
  repeatedly, so — exactly as with `just check-jack` — CI is the first thing
  to run it. It cannot go everywhere:
  it does not execute FFI at all, and it cannot run x86 intrinsics, so
  `denormals.rs`'s register tests are `#[cfg_attr(miri, ignore)]`. What it does cover
  is the aliasing in `AudioThreadCell`, the claims behind `SplitMut` and
  `SplitRw`, `JobGate`'s job cell, `edit_ring`'s slot words and `ArcLend`'s
  reclamation, which is where a mistake would be silent. (`tutti-rt`'s
  `--lib` run is the unit tests, including the two-thread ones; its loom
  models and doctests are not miri's.) (It covered the pointer arithmetic in
  `tutti-node`'s planar buffers too, until doc 013 deleted them: a
  graph node gets its buffers from `tutti-graph`, which is
  `#![forbid(unsafe_code)]`.)

  **First run: 243 of 245 `tutti-types` tests passed under miri with no
  memory-safety finding at all.** The two failures were float precision, not
  unsafety — miri deliberately perturbs libm results between calls to catch
  code that assumes they reproduce, so `Cents::from_pitch_ratio(2.0)` came
  back 1199.9998 and two calls to the same dB conversion disagreed in the
  last bits. Both tests are exact by design and contain no `unsafe`; they were
  `#[cfg_attr(miri, ignore)]`d until `tutti-types` forbade `unsafe` and left
  the miri job (which runs `tutti-rt` and `arc-lend`). Worth knowing
  before reading a green miri run as broader than it is: it covers the
  aliasing and the pointer arithmetic, and it is not a numerical check.
- **`assert_no_alloc`** gates, which are about RT-safety rather than memory
  safety but fail on the same kinds of mistake.

## What is deliberately not here

No `#![deny(unsafe_code)]` workspace-wide with per-crate `allow`s. It reads as
a stronger claim than it is: every crate that matters would carry the escape
hatch, and the annotation would say nothing a reader could rely on.
`#![forbid(unsafe_code)]` on crates that genuinely have none is the honest
version, and a new crate without `unsafe` takes it.
`tutti-types` is one: all of its `rt` module moved to `tutti-rt` for that, so the
vocabulary crate carries the forbid and the lock-free structure sits in one
place with its loom models.
