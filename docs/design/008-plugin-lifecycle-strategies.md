# 008 — Plugin lifecycle: four formats, four shapes, one rule

Status: **describes what is built.** No migration is proposed. This exists because
the four format hosts model their lifecycles differently, that difference looks
like drift, and it is not — each shape is the smallest one its ABI admits.

Written after a survey that got the answer wrong three times. The corrections are
kept in [What a reader is likely to get wrong](#what-a-reader-is-likely-to-get-wrong)
rather than deleted, because each wrong answer is one a future reader will reach
for too.

## The rule

**A lifecycle boundary earns a *type* when crossing it changes what the object
can do and the crossing is rare. It earns a *flag* when the crossing is cheap and
frequent. It earns nothing when the ABI has no such state.**

Every format below follows that rule. They differ because their ABIs differ.

## The one shape underneath

All four have the same underlying progression:

```
Loaded            Active               Processing
(instantiated) →  (buffers allocated) →  (RT running)
```

What varies is which boundaries are real:

| format | Loaded→Active | Active→Processing | pre-activation window? |
|---|---|---|---|
| **VST3** | `Vst3Loaded` → `Vst3Instance<T>` | folded into activation | yes |
| **CLAP** | `ClapLoaded` → `ClapActive<T>` | `flags.processing` bool | yes |
| **AU** | `AuLoaded` → `AuReady` | (no separate step) | yes |
| **VST2** | — | `resumed` bool | **no** |

Three of the four have a genuine pre-activation window because their ABI makes
some negotiation legal *only* while inactive — `setBusArrangements` for VST3,
port enumeration for CLAP, a separately-failable `AudioUnitInitialize` for AU.
VST2 has none: `AEffect` exposes its I/O counts as plain struct fields readable
the moment the library loads, and `Vst2Instance::load` dispatches `resume()` as
part of loading. **VST2's load and activate are the same event**, so it has one
transition, not two, and models it with one bit.

## The transition contract

The three formats with a real transition all express it the same way, and this is
the part worth copying into any fifth format:

**A fallible by-value transition returns the receiver in the `Err`.**

```rust
// CLAP
pub fn activate<T: ClapSample>(self) -> Result<ClapActive<T>, (Self, ClapError)>

// AU
pub fn initialize(self) -> Result<AuReady, (Option<Self>, AuError)>
```

Without that, a failed transition consumes the object and the caller has nothing
to retry with or inspect. Stated generally: **a fallible by-value transition that
drops state on error poisons the object.** AU shipped that bug and fixed it; the
account is under [AU](#au--two-public-types-behind-a-mut-self-façade) below.

CLAP's comment states the ownership reason directly:

> The `Err` variant deliberately hands `self` (a large `ClapLoaded`) back so the
> caller can retry or fall back; boxing it would defeat that ownership return and
> add a heap alloc on the (rare) failure path.

### AU's `Option` is not a wart — it is the more honest signature

AU returns `Option<Self>` where CLAP returns `Self`, and that covers a case CLAP's
signature cannot express. When AU's render-callback install fails, the unit is
*already initialized*, so backing out requires an `AudioUnitUninitialize` that can
itself fail:

```rust
return Err(match ready.uninitialize() {
    Ok(loaded) => (Some(loaded), e),
    Err((_ready, _unwind_err)) => (None, e),   // unit already disposed
});
```

`None` means there is genuinely no recoverable object. AU has a failure mode the
others do not, and its signature says so.

## Why each format is shaped the way it is

### VST3 — two types, and `T` committed at the boundary

`Vst3Loaded` → `Vst3Instance<T>`, with `Deref` so metadata methods are not
duplicated. The type parameter fixes the sample format *at activation* because
`setupProcessing` is where VST3 delivers it and that call happens exactly once per
activation. From `activate_with_mode`:

> The mode is chosen *here*, on the state transition, rather than on the resulting
> instance, because `setupProcessing` is where VST3 delivers it and that call
> happens exactly once per activation. Selecting `Offline` afterwards would
> require re-running setup, which is what the spec's `ProcessSetup`/`ProcessData`
> agreement rule forbids doing silently — so the type-state boundary and the spec
> boundary are made to coincide.

That last clause is the design principle: **the typestate boundary is placed where
the spec's boundary already is.** The realtime↔prefetch pair *is* switchable on a
live instance, and is therefore a method (`set_prefetch`), not a state.

`deactivate` moves `loaded` out by value; a `deactivated: bool` tells `Drop` the
field is gone so it neither re-runs the sequence nor double-drops.

### CLAP — the same two types, plus a processing flag

`ClapLoaded` → `ClapActive<T>`, same reasoning as VST3, same `T`-at-activation
rule (`ClapActive<f64>` requires the plugin to advertise 64-bit support).

Processing is `flags.processing: bool`, **not** a third type. That is deliberate
and correct: start/stop processing flips many times per session on transport
edges, and a typestate would mean moving the object on every one. The rule at the
top decides it — cheap and frequent means a flag.

`AudioScratch` lives only on `ClapActive<T>`, so the RT buffers cannot exist in a
state that cannot process.

### AU — two public types behind a `&mut self` façade

`AuLoaded` and `AuReady` are public, and `AuLoaded::initialize(self)` is a
by-value transition exactly like CLAP's. `AuInstance` is a **façade** holding one
of them in a private enum:

```rust
enum State { Loaded(AuLoaded), Ready(AuReady), Empty }
```

The enum is not the lifecycle model. It is how a `&mut self` convenience wrapper
stores one of two typestates, so a caller that does not want to thread ownership
through every call can use `instance.initialize()?` instead.

**`Empty` is a transient `mem::replace` marker, and its failure handling is
already correct.** Both transitions restore the state they started from:

```rust
Err((recovered, e)) => {
    if let Some(l) = recovered { self.state = State::Loaded(l); }
    Err(e)
}
```

This *was* a live bug and the fix is recorded where it happened:

> Previously the failure arm returned the error while `self.state` was still the
> `Empty` marker `mem::replace` had installed, which turned *every* later method —
> `raw_unit`, `au_type`, even `is_initialized` — into an `unreachable!()` panic. A
> host that scans installed AUs and tolerates one refusing to initialize (some do —
> AUNetReceive, and any unit whose hardware is absent) would crash on the next
> thing it asked.

The one path that leaves `Empty` in place is the `recovered: None` case above,
where the unit has been disposed. Persisting `Empty` there is the honest answer:
every accessor reports the instance as dead rather than pretending.

#### The constraint that makes AU different: a pointer the plugin holds into host memory

`AuReady::scratch` is a `Box<RenderScratch>` and the pinning is load-bearing:

> Heap-pinned so its address is stable across the `State`/`mem::replace` moves in
> `initialize`/`uninitialize`. The AU's input render callback holds a `ref_con`
> pointing at `*scratch`; moving the `Box` moves only its 8-byte pointer, not the
> body, so that `ref_con` stays valid across state transitions. Anything that frees
> this box MUST have already run `AudioUnitUninitialize` so the AU can no longer
> call back into freed memory.

**VST3 and CLAP have no equivalent** — nothing external points into their moved
objects, so their by-value moves are free. Any future change to AU's state
handling must preserve the `Box` indirection; it is not an artifact to clean up.

### VST2 — one bit, idempotent, and that is the whole state machine

`Vst2Instance` carries `resumed: bool` with `is_resumed()`, and both transitions
are **edge-triggered and idempotent**, returning whether a dispatch actually
happened:

> Idempotent: a no-op when already suspended. VST 2.4 does not document
> `effMainsChanged` as idempotent and real plugins free or reallocate buffers on
> each transition, so the host must not issue a redundant one.

That is not a weaker version of the others. It is the correct model for an ABI
with no pre-activation state, and the idempotence is a real guarantee the
typestate formats do not need: for them the type prevents the redundant call, for
VST2 the flag does.

`start_process`'s "only legal while the plugin is resumed" is enforced by
convention rather than by the type. That is the one place VST2 could be
tightened, and it is small: the states exist, they are just not both in the type.

## What a reader is likely to get wrong

These are not hypothetical. Each was asserted during the survey that produced this
document, and each was refuted by reading the file.

| claim | why it is wrong |
|---|---|
| "VST2 tracks no lifecycle state" | It tracks `resumed`, idempotently, with the rationale documented and mutation-tested. The claim came from grepping `tutti-plugin/src/format/vst2_in_process/` — the *adapter* — instead of `tutti-vst2-host`, where the state lives. |
| "CLAP uses a bool, VST3 uses types" | CLAP has `ClapLoaded → ClapActive<T>`, the same typestate as VST3. The `active: bool` is internal bookkeeping *inside* it, not the mechanism. |
| "AU needs migrating to a typestate" | `AuLoaded`/`AuReady` are already public typestates with a by-value, receiver-returning transition. `AuInstance` is a façade over them. |
| "The four should be unified" | They already agree wherever their ABIs agree. A shared type would have to invent a pre-activation state for VST2, which its ABI does not have. |

The pattern in all four: **a shape read from a module listing or a grep, without
checking what the ABI underneath requires.** The formats look inconsistent from a
distance and are consistent up close.

## What is genuinely open

Nothing in the format hosts. Two items live elsewhere:

- **`is_crashed()` at the API layer** is a `bool` that its own documentation says
  can lie — a failed call returns *before* the crash flag is published, and
  `!is_crashed()` does not mean healthy. Every consumer must rebuild the same
  debounce; `bevy-tutti`'s `plugin_host/health.rs` has one
  (`PluginStatus::{Healthy, Failing, Dead}`) that is host-agnostic and in the
  wrong crate. Unrelated to activation.
- **VST2's `start_process` legality** is prose, not type. Small, and only worth
  doing if VST2 grows a second state for some other reason.

## Adding a fifth format

1. Ask what the ABI makes legal only while inactive. If the answer is "nothing",
   you want a flag, not a type — see VST2.
2. If there is a pre-activation window, make two types and put the boundary where
   the spec's boundary is.
3. Make the fallible transition return the receiver in the `Err`. Use `Option` if
   backing out can itself fail.
4. Anything switchable on a live instance is a method, not a state.
5. If the plugin holds a pointer into host memory across the transition, pin it —
   and say so where the pin is, as AU does.
