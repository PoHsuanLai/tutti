# Tutti library style guide

What good looks like for a tutti crate: its API, its code, its docs, its tests
and its packaging. It is the ground for a quality audit. A finding cites a rule
here by its id (`T4`, `D2`), and a rule that does not exist here is not a
finding until it is added.

**Sources, in order of precedence.**
1. [`CLAUDE.md`](../CLAUDE.md): tutti's own rules (units, range-checked values,
   publishing to the audio thread, layering, the testing policy, the dead
   names). This guide does not restate them; it points at them, and where the
   two disagree `CLAUDE.md` wins and this guide is the bug.
2. This guide: the general rules a Rust library is held to, written for tutti.
3. Microsoft's [Pragmatic Rust Guidelines](https://microsoft.github.io/rust-guidelines/)
   (2026.6; ids like `M-STRONG-TYPES`) are **the checklist an audit runs**.
   They include the Rust library team's
   [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/checklist.html)
   by their first rule (`M-UPSTREAM-GUIDELINES`; ids like `C-CASE`), which stay
   the reference for the public API's details. Behind both stand the
   [standard library's conventions](https://doc.rust-lang.org/std/), the
   [rustdoc book](https://doc.rust-lang.org/rustdoc/) and RFC 1574 (doc
   conventions), the [Cargo book](https://doc.rust-lang.org/cargo/) (manifest,
   features, SemVer), and Clippy's lint groups. Where tutti departs from one
   of them on purpose, the departure is written in "Departures" below with its
   reason. A rule here cites the upstream ids it follows.

**Scope.** Every workspace member. The parked `bevy-tutti` is out of scope
until it is un-parked. `vendor/` is third-party and never restyled.

---

## N. Naming

- **N1. Casing** (`C-CASE`): `UpperCamelCase` types and traits, `snake_case`
  functions, modules and crates (crate names `kebab-case` in Cargo),
  `SCREAMING_SNAKE_CASE` constants. Acronyms are words: `Ump`, `Vst3Host`,
  `MidiChannel`, never `UMP` or `VST3Host`.
- **N2. Conversions** (`C-CONV`): `as_` is free and borrows, `to_` costs and
  borrows (or copies), `into_` consumes. `from_*` constructors name the source
  (`from_hz`, `from_rpn_bits`).
- **N3. Getters** (`C-GETTER`): no `get_` prefix (`len()`, `rate()`). The one
  exception is tutti's own: a range-checked or unit newtype's raw value is
  `get()` (`CLAUDE.md`, "Range-checked values").
- **N4. Iterators** (`C-ITER`, `C-ITER-TY`): `iter`, `iter_mut`, `into_iter`;
  an iterator type is named after the method that returns it (`Changes` from
  `take_changes`), and a draining one says so.
- **N5. Word order** (`C-WORD-ORDER`): error and event types are noun then
  verb-ish, consistently across a crate (`ParseError`, `DecodeError`, not a
  mix with `ErrorParse`).
- **N6. Domain names**: the theory or spec name in an algorithm or a spec type,
  the control word in a user-facing node API, *frame* and *sample* as
  `CLAUDE.md` defines them (its "Naming" item).
- **N7. No stale names**: a name in `CLAUDE.md`'s dead-name lists appears
  nowhere but those lists and dated records (`docs/archive`, a design doc's
  history section). A comment or doc that names a dead item is a finding.
- **N8. One name per thing**: a concept has one name across crates (a frame
  count is `Samples` everywhere; a node's key is `NodeKey`). Two names for one
  thing, or one name for two things (`Wave` the buffer vs the old `Wave` the
  waveform), is a finding.

## T. Types and API shape

- **T1. Make the wrong thing unwritable** (tutti's first rule; `M-STRONG-TYPES`, `M-STRONG-TYPES-GUARD`): a value with a
  range is a range-checked type (`CLAUDE.md`, "Range-checked values"); a
  quantity is a unit (`CLAUDE.md`, "Unit types"); a state is a typestate or an
  enum, not a flag plus a doc comment. A runtime check or a doc sentence that a
  type could replace is a finding (severity by how easy the mistake is).
- **T2. No bare primitives for domain values** (`C-NEWTYPE`): an id, a count, a
  rate, a channel, an index into a foreign table is a newtype. A public `usize`
  that means "frames" or "channels" is a finding.
- **T3. No boolean or stringly parameters** (`C-CUSTOM-TYPE`): a `bool` argument
  whose meaning is not obvious at the call site, or a `&str` that selects a mode,
  is an enum. An error is never a `String`.
- **T4. Private fields** (`C-STRUCT-PRIVATE`): a public struct has private fields
  unless it is a plain record with no invariant (and then it says so). A public
  tuple field on a newtype (`NodeKey(pub u64)`) is a finding.
- **T5. Constructors** (`C-CTOR`): `new` for the obvious one, `with_*`/`from_*`
  for others, a builder (`C-BUILDER`) past three or four optional knobs. A
  constructor that can fail returns `Option` (one obvious reason) or `Result`.
  It never clamps silently (`CLAUDE.md`, "Range-checked values").
- **T6. Common traits** (`C-COMMON-TRAITS`, `C-DEBUG`): every public type is
  `Debug` (the workspace warns on `missing_debug_implementations`); value types
  derive `Clone`, `Copy` (if small and plain), `PartialEq`, `Eq`, `Hash`,
  `PartialOrd`/`Ord` where meaningful. `Default` only when the default is a
  valid and unsurprising value: a range-checked type with no in-range default
  has none.
- **T7. Conversions are traits** (`C-CONV-TRAITS`): `From`/`TryFrom`/`AsRef`
  rather than ad-hoc methods where the conversion is the type's obvious one;
  never an infallible `From` that can produce an invalid value; never `.into()`
  across a unit and its raw (`CLAUDE.md`).
- **T8. Arguments** (`C-CALLER-CONTROL`, `C-GENERIC`): borrow what you read,
  take by value what you keep; `impl Into<NodeKey>`-style generics where the
  crate already uses the pattern, not elsewhere; no `&Vec<T>`/`&String`
  parameters (`&[T]`, `&str`).
- **T9. `#[must_use]`** on a pure function's result and on every `Result` or
  handle whose drop loses something (`CLAUDE.md` names the removal calls).
- **T10. Future-proofing** (`C-SEALED`, `C-STRUCT-BOUNDS`): a trait not meant
  for downstream implementation is sealed; no trait bounds on a struct
  definition that only its impls need. `#[non_exhaustive]` follows
  `CLAUDE.md`'s decision per type (not on `Message` while unpublished).
- **T11. Send and Sync** (`C-SEND-SYNC`): a type that should cross threads
  does, and a type that must not (`MainThread`, a `Loan`) is `!Send` by
  construction, with a test or a `static_assertions`-style check.
- **T12. No `Deref` for delegation** (`C-DEREF`): `Deref` only for smart
  pointers; a wrapper exposes what it wraps by methods.
- **T13. One way in** (`M-SINGLE-ITEM-PATH`): one public path per item (`CLAUDE.md`'s import-path
  rule), one constructor per meaning, no two methods that do the same thing,
  and no item kept only for a test or a doc example (aggressive cleanliness).
- **T14. Visibility is minimal**: `pub(crate)` unless something outside needs
  it; a `pub` item no other crate and no doc uses is a finding.
- **T15. No leaked foreign types** (`M-DONT-LEAK-TYPES`, `M-FOREIGN-REEXPORTS`):
  a dependency's type in a public signature, field or error ties the crate's
  API to that dependency's versions; tutti's own type stands in (`DecodeError`
  over symphonia, `CLAUDE.md`). An item is re-exported from the crate that
  defines it, never re-exported from a third party as if it were tutti's.
- **T16. No glob re-exports, no preludes** (`M-NO-GLOB-REEXPORTS`,
  `M-NO-PRELUDE`; owner, 2026-11-13): a crate root names what it re-exports,
  so the surface is reviewable in one file, and no crate defines a `prelude`.
  An application imports from the umbrella's **flat root**
  (`use tutti::{Session, SessionEvent, Root, Samples};`): `tutti` re-exports
  the app-facing items at its root by name, so a user never needs to know
  which engine crate an item lives in; the per-crate modules (`tutti::graph`,
  `tutti::session`, ...) stay for the deep, specialised API.
- **T17. Parameter order is consistent** (`M-PARAMETER-CONSISTENCY`): the same
  kinds of argument come in the same order across a crate (`(spec, target)`
  everywhere, never `(target, spec)` in one place).
- **T18. No global state** (`M-AVOID-STATICS`): a `static` holding mutable
  state is a finding unless it is one of the process-wide facts `CLAUDE.md`
  names (`MainThread::claim`, the play clock's epoch counter).

## E. Errors and panics

- **E1. Good error types** (`C-GOOD-ERR`): every error implements
  `std::error::Error` and `Display`; `Display` is lowercase, without a trailing
  period, and says what failed in the caller's words; the cause is
  `source()`, not pasted into the message.
- **E2. One error type per boundary** (`M-ERRORS-CANONICAL-STRUCTS`, adapted:
  see "Departures"): a crate's (or a module's) operations share an enum whose
  variants are the distinct failures a caller can act on; failures that do not
  overlap are separate types, not variants lumped into one enum; no inner
  `ErrorKind` is exposed; a variant that carries only a `String` is a finding
  unless the text is the only information there is (an OS message). A
  canonical conversion is `From`, so `?` works (`M-FROM-ERROR`).
- **E3. Errors carry the refused value** where it helps (`TryFrom`'s error
  carries the raw, `CLAUDE.md`).
- **E4. No panic in a library path a caller can reach with valid input** (`M-PANIC-ON-BUG`, `M-PANIC-MESSAGE`).
  `unwrap`/`expect` in library code states its invariant in the `expect`
  text; a panic a caller can trigger with a bad argument is documented under
  `# Panics` and is either a programming error (`of` on a literal) or replaced
  by a `Result`.
- **E5. Never on the audio thread**: no allocation, lock, panic path or
  blocking call in `Node::process` or what it calls (`CLAUDE.md`, the
  `no_alloc` gates).
- **E6. Destructors** (`C-DTOR-FAIL`, `C-DTOR-BLOCK`): a `Drop` never fails and
  never blocks on another thread without a bound; a teardown that can fail has
  an explicit method.

## D. Documentation

- **D1. Crate docs** (`C-CRATE-DOC`): every crate root has a doc that says what
  the crate is for, what it is not, its place in the layering, and a short
  example; a crate with a README uses it as its crate doc
  (`#![doc = include_str!("../README.md")]`) so the README's examples are
  doctests.
- **D2. Every public item is documented** (`missing_docs` warns;
  `M-FIRST-DOC-SENTENCE`). The first line is one sentence of about fifteen
  words saying what it is or does, not how; a module has a doc saying what it
  holds (`M-MODULE-DOCS`).
- **D3. Sections** (`C-FAILURE`): `# Errors` on a fallible function, `# Panics`
  on one that can panic, `# Safety` on every `unsafe fn` and `unsafe trait`
  (Clippy's `missing_errors_doc` and `missing_panics_doc` warn).
- **D4. Examples** (`C-EXAMPLE`, `C-QUESTION-MARK`): a type a user starts from
  has a doctest; examples use `?`, not `unwrap`, through a hidden `fn main() ->
  Result`.
- **D5. Links** (`C-LINK`): types and methods are intra-doc links; a link
  rustdoc cannot resolve, a redundant explicit target, or a public doc linking
  a private item fails CI (`cargo xtask doc`, `check-doc-features`).
- **D6. A comment states what the code does and the invariant it holds**
  (`M-NO-META-DESIGN-DOCUMENTATION`): safety, ordering, real-time, in the
  present tense; rustdoc documents the item. Everything else is deleted, not
  rewritten: history, why something was added, past failures, removed names,
  transitional notes ("for now", "not yet", "until"), citations of design docs
  or audits, restatements of a gate or `CLAUDE.md`, and rationale for a
  default. Git holds the history. A finding on this rule is fixed by deleting
  the text, never by rewording it. `cargo xtask check-comments` holds the
  citations it can see to a per-file baseline that only falls.
- **D7. Docs agree with the code** (`CLAUDE.md`: "a comment or doc that
  disagrees with the code is the bug"), and are updated in the PR that changes
  the code.
- **D8. Unit and range in the doc**: a parameter or field whose type does not
  carry its unit or range says it (and is a T1/T2 candidate).
- **D9. Rustdoc is the documentation** (owner, 2026-11-13). What a reader needs
  to use or change an item lives in its rustdoc: the item's doc, its module's
  doc, or its crate's doc. A comment or doc never points elsewhere for it: no
  "doc 013", "design 0NN §5", "as built (T2)", plan item numbers or lane
  names, which rustdoc cannot render and a reader cannot follow. A design
  decision a user must know (why a type refuses something, what a guarantee
  costs) is written where the item is, in a sentence. `docs/design/` is being
  retired: its current-state content moves into the crates' rustdoc and the
  files are deleted (git history keeps them).
- **D10. Rust's own doc style** (the standard library's and RFC 1574's): the
  summary line is a third-person present-tense sentence ending in a period
  ("Returns the number of frames in the wave."), followed by a blank line and
  the detail; sections are `# Examples`, `# Errors`, `# Panics`, `# Safety`
  (and `# Performance` where it matters), in that order; prose is plain
  sentences, not bold fragments or bullet telegrams; code, types and values
  are in backticks and items are intra-doc links; a module or crate doc opens
  with what it provides and how its pieces fit, then an example.

## C. Code

- **C1. Modules are private** with a re-exported root (the per-module path
  gate); a file is one concern. A file past ~1,500 lines, or one that holds
  two unrelated types, is a split candidate; a file's tests live in a sibling
  `tests.rs`, not inline (plan item 43).
- **C2. No duplicated logic**: one copy of each algorithm, walk, parser or
  table (`CLAUDE.md`'s "one bundle walk"). A second copy is a finding even if
  both are tested.
- **C3. `unsafe` only where `CLAUDE.md` puts it** (`check-ffi`; `M-UNSAFE`, `M-UNSOUND`, `M-FFI-TRANSLATES`), every block
  with a `// SAFETY:` comment that states why the precondition holds here, not
  what the code does (`clippy::undocumented_unsafe_blocks` denied in each
  crate that has `unsafe`). It is concentrated: one wrapper carries the
  `unsafe impl` and everything around it is safe. A safety argument that rests
  on a property is checked where the property lives, in every build (a check
  that justifies a `Sync` is never debug-only). New non-FFI `unsafe` is a design
  question for `tutti-rt`, with a loom model, after the safe construct was
  measured; FFI `unsafe` is exempt from that, not from the `SAFETY` comment
  (pointer lifetime, who frees).
- **C4. Lint exceptions are scoped and reasoned** (`M-LINT-OVERRIDE-EXPECT`): `#[expect(lint, reason =
  "...")]` on the smallest item, never a crate-wide `allow` for a local need;
  an `allow` that suppresses nothing (an `expect` would fail) is a finding.
  `dead_code` allowed on library code is a finding: delete it.
- **C5. No magic numbers** (`M-DOCUMENTED-MAGIC`): a constant with a meaning is a named `const` with
  a doc that says where it comes from (a spec section, a measurement).
- **C6. Errors are propagated, not swallowed**: `let _ =` on a `Result`, or
  `.ok()` that discards an error, says why in a comment.
- **C7. Std first**: no hand-rolled version of a std item (`chunks_exact`,
  `split_at`, `u32::from_le_bytes`, `Iterator` adapters); no indexing loop
  where an iterator says it.
- **C8. Casts are checked**: `as` between integer widths only where the range
  is proven next to it; otherwise `try_from` or a named saturating helper.
- **C9. Features are additive** (Cargo; `M-FEATURES-ADDITIVE`): a feature only adds items, never
  changes behaviour or removes them; no `cfg(not(feature))` arm that refuses at
  runtime (`CLAUDE.md`, the codec rule).
- **C10. Layering holds** (`check-layout`): no new edge that points up, and
  no indirection whose only job is to avoid moving code to the right crate.

## X. Tests

The policy is `CLAUDE.md`'s "Testing policy" and "Testing": a test asserts a
property from a spec, an analytic figure or an invariant; it can fail; no
reference-render comparison, no migration test, no golden sprawl. In audit
terms:

- **X1. A test that cannot fail** (`M-TAUTOLOGICAL-TESTS`) (always true, asserts only no panic,
  compares a function with itself) is deleted.
- **X2. Names state the property** (`a_rate_that_is_no_rate_does_not_decode`),
  and the doc comment says which spec or invariant it holds.
- **X3. Layout**: one integration binary per area (`tests/<area>/main.rs`),
  unit tests in sibling files, shared helpers in one `support` module or
  `tutti-fixtures`, never copied between binaries.
- **X4. No test-only seams in library code** (`M-TEST-UTIL`): an item that exists only for a
  test is behind a `testing` feature or deleted (`CLAUDE.md`).
- **X5. Determinism**: no sleep-based timing where a condition can be waited
  on; a test that has flaked is fixed at the cause or recorded in the flake
  list with its cause.
- **X6. Feature-gated suites are listed** where CI runs them
  (`test-features`, the dark-features job).

## P. Packaging

- **P1. Manifest metadata** (`C-METADATA`): `description`, `license`,
  `repository`, `readme` (where there is one), `keywords`, `categories`,
  `rust-version` and `edition` inherited from the workspace; `documentation`
  for a crate meant to publish; `publish = false` stated, with its reason, on
  every crate not meant for crates.io.
- **P2. Features** (`C-FEATURE`): named for what they add (`serde`, `clap`,
  `decode-flac`), never `use-*`/`with-*`; `default` minimal (empty for an
  engine crate); every feature has a comment in the manifest saying what it
  turns on and who needs it; test-only features say so in their name
  (`testing` for a seam other crates' tests use, `contract` for a public conformance suite).
- **P3. Dependencies**: from `[workspace.dependencies]` where shared; the
  fewest crates and the fewest default features that do the job; no C library
  where a pure-Rust one exists (owner rule); `cargo xtask deny` clean.
- **P4. Docs.rs**: a published crate has `[package.metadata.docs.rs]` with its
  features and `--cfg docsrs`, and gated items show their feature
  (`doc_cfg`).
- **P5. SemVer** (Cargo's SemVer reference): a breaking change is marked `!` in
  the PR title and gets a dead-name line in `CLAUDE.md`; the `CHANGELOG.md`
  records user-visible changes per release.
- **P6. Licences** (`C-PERMISSIVE`): `MIT OR Apache-2.0`, both licence files at
  the root; code derived from elsewhere carries its notice and licence file
  (`tutti-soundfont`'s rustysynth notice is the model).

## S. Crates that support `no_std`

A crate that runs on a target with no operating system follows these rules; the
list of such crates is `NO_STD_CRATES` in `xtask/src/recipes.rs`.

- **S1. `#![no_std]`** at the root, with `extern crate alloc` only in a crate
  that allocates. A crate that needs no allocator does not link `alloc`.
- **S2. `std` is a feature that adds** (`C9`): it turns on items that need the
  operating system and never changes what the rest does. It is on by default
  only when the crate already had `std`-only API; a new crate leaves it off.
- **S3. Errors implement `core::error::Error`**, not `std::error::Error`
  (`E1`). `thiserror` 2 with `default-features = false` derives it.
- **S4. Float maths is `libm`**, on every build, never the `std` float methods
  (`f32::sin`, `powf`, ...), so a result does not depend on the target's
  libm.
- **S5. No spin locks.** A `no_std` crate takes no lock; shared state goes
  through `tutti-rt` (`CLAUDE.md`).
- **S6. The build is the check.** `cargo xtask check-no-std` builds each listed
  crate, bare and with each of its features, for `thumbv7em-none-eabihf`
  (`rustup target add thumbv7em-none-eabihf`); CI's dark-features job runs it.
  A `std` use fails to compile there, so no text gate looks for the attribute.

---

## Departures from upstream

Where tutti decided otherwise, on purpose. An audit does not raise these.

- **Error shape** (`M-ERRORS-CANONICAL-STRUCTS`): a tutti error is an enum per
  boundary (E2), with the cause as `source()`, and it captures **no
  backtrace**: some are made on the audio thread, where capturing one
  allocates (E5). The rest of the upstream rule holds.
- **Getters** (`C-GETTER`): a range-checked or unit newtype's raw is `get()`
  (N3).
- **Examples on every item** (`C-EXAMPLE`): required on the types a user
  starts from (D4), not on every item; a method whose doc and signature say it
  all needs none.
- **Serde everywhere** (`C-SERDE`): only where a consumer persists the value,
  behind a `serde` feature, and never on wire storage (`Ump`).

---

## A. Running an audit

**Unit of work.** One crate at a time (or one area of a large crate), read
whole, with its tests, README and the design doc that describes it. A reviewer
who has not read the crate's `CLAUDE.md` entry and design doc is not ready.

**What to read, in order.** The public surface first (`cargo doc --open`, or
the root's re-exports), then the modules behind it, then the tests, then the
manifest and README.

**A finding** is one line in the crate's audit file:

```text
<rule id> | <severity> | <file:line> | <what is wrong> | <the fix>
```

- **Severity**: `bug` (wrong behaviour reachable today), `api` (a public shape
  that will cost a breaking change later: fix before publishing), `clean`
  (internal quality), `doc` (docs or comments only).
- **The fix is concrete**: "make `x` private and add `fn x()`", not "consider
  improving". A finding whose fix is a judgment call is put as a question for
  the owner with a recommendation.
- **Not a finding**: anything `CLAUDE.md` records as a decision (cite it if a
  reader would trip on it), style rustfmt or Clippy already enforces, and a
  preference this guide does not state.

**Mechanical checks first.** Before reading, run what a machine can count, so
the reading time goes to judgment:

| Check | How |
|---|---|
| Lints | `cargo clippy -p <crate> --all-targets --all-features -- -D warnings -W clippy::pedantic` (pedantic is advisory: report, do not gate) |
| Docs | `cargo xtask doc`, `cargo xtask check-doc-features` |
| Public surface | the crate root's `pub use` list against what other crates and docs actually use (T14) |
| Size | files over ~1,500 lines (C1), inline `mod tests {` (C1) |
| Lint exceptions | `#[allow(` / `#![allow(` per crate (C4) |
| Bare primitives | public fields and signatures of `usize`, `u32`, `f32`, `f64`, `bool`, `String` (T2, T3) |
| Panics | `unwrap(`, `expect(`, `panic!`, indexing in non-test code (E4) |
| Dead names | `CLAUDE.md`'s dead-name lists grepped across code and docs (N7) |

**Output.** One file per crate under the audit directory, findings sorted by
severity, then a summary of the whole pass: counts per rule, the `api`
findings in one list (they decide what must happen before a publish), and the
proposed PRs, each a coherent change (one rule across many crates, or one crate
across many rules), ordered `bug`, `api`, `clean`, `doc`.

**Changing this guide.** A rule is added when an audit finds a pattern this
guide does not name and the owner agrees; it gets the next id in its section
and never reuses a retired one.
