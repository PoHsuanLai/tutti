# Tutti conventions

The rules a tutti crate is expected to follow, and the reasons behind them.

Each rule is stated in one sentence, followed by why it exists. A rule you cannot
state in one sentence is not yet a rule. Where a crate deliberately departs from
one of these, the departure is documented at the departure site — an undocumented
divergence is a defect; a documented one is a decision.

These were derived from what the majority of crates already do. This file records
the house style rather than inventing one.

## 1. Naming

**A concept has one name across the workspace.** The same operation with two names
in two crates costs a reader the assumption that they are the same operation.

**A name means one thing.** `Instance`, `VoiceSlot` and `ProcessContext` each name
two different jobs today; a name that means two things is worse than two names,
because the collision is invisible at the call site.

**A crate is named for its job, not its implementation or its dependency.** A
reader choosing a crate reads the name first and the docs second.

## 2. Unit types and vocabulary

**A quantity covered by a `tutti_types` newtype uses the newtype.** The roster and
the omission ledger live in `tutti-types/src/value/units.rs`.

**A newtype survives the round trip.** If a constructor takes `MidiChannel`, the
matching accessor returns `MidiChannel` — a newtype that decodes back to `u8` was
only a type at one end, and every consumer downstream carries the bare integer.

**A boundary where the types stop says so, at the boundary.** C ABI, FFI, WIT and
codec surfaces are legitimate stopping points; an undocumented bare `f32` is
indistinguishable from an oversight.

**An omission ships with its replacement.** Removing an operator or an impl without
writing the named method that replaces it sends call sites back to raw floats.

## 3. Constructing and configuring

**Sample rate is a constructor argument.** A node built at a placeholder rate and
corrected later produces silently wrong-rate audio — not a panic, not silence — if
the correction is missed, and the correction is invisible in a review diff.

**`with_*` is a chainable `mut self` builder, never an alternate constructor.** Both
readings exist in the tree today, sometimes in one `impl` block, so the name alone
cannot tell a reader whether it goes first or last.

**A setter that writes an atomic takes `&self`.** `&mut self` on an atomic-backed
setter claims an exclusivity the type does not need and the audio thread cannot give.

**A fallible constructor acquires something.** `new` returns `Result` when it opens a
device, binds a socket, parses an asset, or rejects input that would otherwise
allocate on the audio thread — and is infallible otherwise.

## 4. The audio thread

**The audio thread never allocates, locks, or frees.** This is the engine's one
non-negotiable rule; everything else in this file is a preference by comparison.

**Non-scalar state reaches the audio thread through `RtPublish`.** The sanctioned
exceptions are the nullable hot-swap slots (`SharedReader`, `InputSlot`, `Midi::out`)
and the append-only port list in `tutti-midi-hardware`
(`core/port/manager.rs`); anything else is a new exception and needs an argument.

**A queue crossing to the audio thread is bounded and drops on full.** An unbounded
queue makes `try_send` infallible, which turns a `let _ =` that reads like a
drop-on-full guard into an allocation on every send.

**A claim of RT-safety is backed by a test that exercises the real path.** A no-alloc
test driving a stub asserts the stub allocates nothing.

**`unsafe impl Send`/`Sync` carries a `SAFETY:` argument.** The argument is the whole
value of the impl; without it a reader cannot check the claim.

## 5. Threads and shutdown

**Whoever spawns a thread joins it in `Drop`.** A detached thread that owns a file
finalizer loses data on an early return, and the loss is silent.

**A detached thread is documented as detached, with the reason.** One-shot work that
hands its result over a channel is a legitimate exception.

## 6. Errors

**One error type per crate, named `Error`, with a `Result<T>` alias.** Prefixed names
(`Vst3Error`, `HrtfBinauralError`) are for crates carrying several genuinely different
failure domains.

**A crate with no error type says why.** Infallibility is a design claim worth stating.

**Errors derive `thiserror`.** A hand-written `Display` drifts from the variant it
describes.

**A public API does not carry a `String` error.** A string is a message, not a
failure a caller can match on.

## 7. Documentation

**The README is the crate doc.** `#![doc = include_str!("../README.md")]` makes drift
between them structurally impossible; the two crates that do this are the only two
with no drift between the two surfaces.

**A crate doc states what the crate does not own, naming the sibling that does.** This
is the single most useful paragraph for a reader deciding where to look next, and the
strongest existing convention in the tree.

**The gate is BOTH workspaces.** The app (`dawai-*`) is a separate cargo
workspace, and no engine command builds it — `--manifest-path
crates/bevy-tutti/Cargo.toml --workspace` stops at the engine boundary. An
engine change that alters a public type must be verified from the repo root as
well, or it lands green and breaks the app's tests. A newtype migration did
exactly that: three assertions and two imports in `dawai-model`, invisible to
every engine gate.

**Every check runs under `--all-features`.** A `cfg`-gated item is invisible to
a default-features run, and this workspace gates a great deal. Running the gate
both ways found, in one pass: two doctests that survived a workspace-wide
rename because nothing compiled them (386 doctests by default, 408 with every
feature), a 15 KiB enum variant, an item declared after a test module, and
fourteen broken intra-doc links in code nobody had built with docs on. None of
these was visible in a green default run.

**Every documented example compiles.** A doctest is the only documentation the
compiler checks; prose examples rot silently.

**A comment states a constraint the code cannot show.** Not what the next line does,
not what the code used to be, not how many call sites a past refactor touched — those
decay into false statements.

**A shared argument is written once and linked.** The same rationale restated in four
crates diverges in four directions.

**A doc does not restate an enumerable fact.** Variant lists and method counts go
stale; link the type instead.

## 8. Layout

**Errors live in `src/error.rs`.**

**A crate's public position and its file position agree.** A type re-exported at the
crate root belongs at the root of the source tree.

**Tests live beside the code and in `tests/`** — unit tests inline, integration tests
in `tests/`.

## 9. Deprecation

**Nothing is deprecated; it is deleted.** These crates are unpublished, so a
deprecation shim carries no consumer and only defers the migration.
