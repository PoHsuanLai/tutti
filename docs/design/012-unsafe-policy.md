# 012 — The `unsafe` policy

Tutti's pitch is that it hosts plugins *safely*, out of process. A project
making that claim should be able to say where its own `unsafe` is, why each
piece is there, and what checks it. This is that statement.

## Where it is

Counted as lines mentioning `unsafe`, so blocks, `unsafe fn` and `unsafe impl`
together — an upper bound, not a block count:

| area | ~lines | what it is |
|---|---|---|
| `crates/plugin/formats/` | 1,519 | VST2/VST3/CLAP/AU C ABIs |
| `crates/plugin/tutti-plugin-server/` | 52 | the subprocess side: shm, signals |
| `crates/plugin/tutti-plugin/` | 46 | shm slab, IPC transport |
| `crates/midi/tutti-midi-hardware/` | 33 | CoreMIDI / ALSA seq-UMP |
| `crates/core/` | 46 | see below |
| `crates/dsp/` | 14 | see below |
| `crates/plugin/tutti-plugin-types/` | 7 | bundle/ABI value types |

**Roughly 95% of it is FFI**, and that is inherent: you cannot host a VST3
without speaking COM, or reach CoreMIDI without calling it. The interesting
question is the other 5%.

## The non-FFI `unsafe`, in full

There are eight sites. Each exists because a safe construct would have cost
something on the audio thread (or, for the last, because an invariant the
type system cannot see must be stated at the call), and each is named here
so the list can be checked against the tree.

- **`tutti-types/src/rt/cell.rs`** — `AudioThreadCell`: an `UnsafeCell` plus
  `unsafe impl Send`/`Sync`, with a `#[cfg(debug_assertions)]` borrow check.
  The RT-safe alternative to `RefCell`, which would pay a counter per access.
  Note its `reset_owner` is a **no-op** and always has been; the cell pins no
  owner thread, and three doc comments that claimed otherwise were corrected
  after a mutation test found the call could be deleted with no effect.
- **`tutti-types/src/rt/publish.rs`** — `RtPublish`: an `AtomicPtr` holding
  an `Arc::into_raw` pointer, reclaimed with `Arc::from_raw` by `publish` and
  `Drop`, and dereferenced by `RtRef` under a hazard-slot protocol. The safe
  alternative (`ArcSwap`) could, rarely, free on the audio thread; this cannot,
  and that is the whole reason for it. The dereference's soundness rests on a
  `SeqCst` fence pair, which is a *concurrency* property — so it is checked by
  a loom model run against the shipped code (`tests/rt_publish_loom.rs`) as
  well as by miri over the module's concurrent stress test.
- **`tutti-types/src/rt/denormals.rs`** — `read_mxcsr` and the FTZ/DAZ
  set. x86 SSE control-register intrinsics; there is no safe spelling.
- **`tutti-node/src/buffer.rs`** — `slice::from_raw_parts{,_mut}` over the
  planar block buffers. The single highest-value target for a checker in this
  list: it is pointer arithmetic over a fixed `MAX_BUFFER_SIZE` stride, in the
  crate every node in the graph processes through.
- **`tutti-sampler/src/butler/prefetch.rs`** — `unsafe impl Send`/`Sync` on
  `SendProd`, the ring producer handed to the disk butler. The file says it
  "concentrates the unsafe impls in one place", which is the right shape.
- **`tutti-cpal/src/driver_seam.rs`** — `unsafe impl Send for CpalStream`.
  cpal's stream handle is not `Send` on every backend; the driver owns it and
  never shares it, which is what makes the claim true.
- **`tutti-core/src/engine.rs`** — `unsafe fn Engine::settle_graph`, and
  its one call, in **`tutti-cpal/src/driver.rs`**'s `Stopped::settle_graph`.
  It contains no unsafe operation: the `unsafe` is a *contract*. It borrows
  the executor and the transport schedule the audio thread owns, from the
  control side, so a device restart can finish a graph re-prepare before the
  first block (doc 013, PR 13). Those live in `AudioThreadCell`s, whose
  concurrent-borrow check is debug-only, so a safe `pub fn` would have let a
  host race a running callback in release with no diagnostic. The caller
  must guarantee no callback runs for the duration; `Stopped` — a token
  constructible only inside `TuttiDriver`'s restart, after the old stream
  is dropped and before the new one starts — is the safe way in, and its
  `SAFETY:` comment argues the invariant (including the one part no type
  checks: a host must not render the callback state by hand besides the
  driver, which `TuttiDriver::from_parts` states). Nothing for miri to
  check: the race is the hazard, and the body is safe code.
- **`tutti-polysynth`, `tutti-analysis`** — mentions in prose only, no
  `unsafe` code.

`tutti-mod` and `tutti-graph` carry `#![forbid(unsafe_code)]`. The second is
the one worth knowing: its executor hands every node `&[f32]` inputs and
`&mut [f32]` outputs out of one shared arena, which is the textbook reason to
reach for raw pointers. It does not — the disjoint borrows come from
`split_at_mut` over sorted slot indices and the reinterpretation from
`bytemuck`'s checked cast (`tutti-graph/src/arena.rs`), and a colouring bug
would be a panic naming the slot rather than aliasing. Rule 4 below, applied.

## The rules

1. **`unsafe` needs a `// SAFETY:` comment stating the invariant and who
   upholds it.** Not what the code does — why the precondition holds here.
2. **Concentrate it.** `SendProd` is the pattern: one wrapper carrying the
   `unsafe impl`, everything else safe around it. A crate with `unsafe`
   scattered across ten files has ten things to re-verify on every change.
3. **A safety argument that depends on a *property* must be checked where the
   property lives.** `AudioThreadCell` is the cautionary case: its debug check
   detects a concurrent borrow, not a foreign thread, and the surrounding docs
   drifted into claiming the stronger thing. If the invariant is not
   mechanically checked, say so at the type.
4. **New non-FFI `unsafe` is a design question, not an implementation
   detail.** Eight sites is small enough to review one at a time; that is worth
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
  argument the shm `unsafe` rests on. **`tutti-types/tests/rt_publish_loom.rs`**
  does the same for `RtPublish`, but against the real code: its atomics switch
  to loom's under the flag, which that crate's dependency closure tolerates.
  Both run in the `loom` CI job and `just loom`, the `RtPublish` models at a
  preemption bound of 4; `just loom-full` runs them exhaustively.
- **miri**, on the non-FFI crates, via `just miri` and the `miri` CI job.
  Installing a nightly toolchain on the machine that wrote this failed
  repeatedly, so — exactly as with `just check-jack` — CI is the first thing
  to run it. It cannot go everywhere:
  it does not execute FFI at all, and it cannot run x86 intrinsics, so
  `denormals.rs`'s tests are `#[cfg_attr(miri, ignore)]`. What it does cover
  is the pointer arithmetic in `tutti-node`'s buffers and the aliasing in
  `AudioThreadCell`, which is where a mistake would be silent.

  **First run: 243 of 245 `tutti-types` tests passed under miri with no
  memory-safety finding at all.** The two failures were float precision, not
  unsafety — miri deliberately perturbs libm results between calls to catch
  code that assumes they reproduce, so `Cents::from_pitch_ratio(2.0)` came
  back 1199.9998 and two calls to the same dB conversion disagreed in the
  last bits. Both tests are exact by design, contain no `unsafe`, and are now
  `#[cfg_attr(miri, ignore)]`d with that reasoning at the test. Worth knowing
  before reading a green miri run as broader than it is: it covers the
  aliasing and the pointer arithmetic, and it is not a numerical check.
- **`assert_no_alloc`** gates, which are about RT-safety rather than memory
  safety but fail on the same kinds of mistake.

## What is deliberately not here

No `#![deny(unsafe_code)]` workspace-wide with per-crate `allow`s. It reads as
a stronger claim than it is: every crate that matters would carry the escape
hatch, and the annotation would say nothing a reader could rely on.
`#![forbid(unsafe_code)]` on crates that genuinely have none — starting with
`tutti-mod` — is the honest version, and it should spread.
