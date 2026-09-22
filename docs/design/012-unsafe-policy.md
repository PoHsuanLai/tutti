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

There are six sites. Each exists because a safe construct would have cost
something on the audio thread, and each is named here so the list can be
checked against the tree.

- **`tutti-types/src/rt/cell.rs`** — `AudioThreadCell`: an `UnsafeCell` plus
  `unsafe impl Send`/`Sync`, with a `#[cfg(debug_assertions)]` borrow check.
  The RT-safe alternative to `RefCell`, which would pay a counter per access.
  Note its `reset_owner` is a **no-op** and always has been; the cell pins no
  owner thread, and three doc comments that claimed otherwise were corrected
  after a mutation test found the call could be deleted with no effect.
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
- **`tutti-polysynth`, `tutti-analysis`** — mentions in prose only, no
  `unsafe` code.

`tutti-mod` carries `#![forbid(unsafe_code)]`. It is the only crate that does,
and it should not be the last.

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
   detail.** Six sites is small enough to review one at a time; that is worth
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
  argument the shm `unsafe` rests on.
- **miri**, on the non-FFI crates, via `just miri`. **Wired but not yet run
  here** — installing a nightly toolchain on this machine failed repeatedly
  and left a partial install, so the recipe is unverified locally in the same
  way `just check-jack` was before CI compiled it. Run it, or wire it into CI,
  before treating this row as a check that exists. It cannot go everywhere:
  it does not execute FFI at all, and it cannot run x86 intrinsics, so
  `denormals.rs`'s tests are `#[cfg_attr(miri, ignore)]`. What it does cover
  is the pointer arithmetic in `tutti-node`'s buffers and the aliasing in
  `AudioThreadCell`, which is where a mistake would be silent.
- **`assert_no_alloc`** gates, which are about RT-safety rather than memory
  safety but fail on the same kinds of mistake.

## What is deliberately not here

No `#![deny(unsafe_code)]` workspace-wide with per-crate `allow`s. It reads as
a stronger claim than it is: every crate that matters would carry the escape
hatch, and the annotation would say nothing a reader could rely on.
`#![forbid(unsafe_code)]` on crates that genuinely have none — starting with
`tutti-mod` — is the honest version, and it should spread.
