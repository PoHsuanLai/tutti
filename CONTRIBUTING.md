# Contributing to tutti

Bug reports, failing tests and pull requests are all welcome.
Everyone taking part follows the [code of conduct](CODE_OF_CONDUCT.md).

## Setup

```bash
git clone --recursive https://github.com/tutti-audio/tutti
```

The VST3 SDK is a submodule. In an existing clone, run
`git submodule update --init --recursive`. On Linux, install the ALSA headers
(`libasound2-dev pkg-config`). The toolchain is pinned in `rust-toolchain.toml`,
and rustup picks it up.

## Building and testing

```bash
cargo build -p tutti-plugin-server -p tutti-plugin-probe   # the suites spawn these
cargo nextest run --workspace
cargo test --doc --workspace                               # nextest skips doctests
cargo xtask ci                                             # everything CI runs
```

`cargo xtask help` lists the other recipes: the static gates, the
feature-gated suites, the Windows lints and the benches.

## The rules

[`CLAUDE.md`](CLAUDE.md) holds the rules every change follows: the graph and
audio-thread rules, unit types, unsafe code, layering and the test policy. The
library style is in [`docs/style-guide.md`](docs/style-guide.md), and the
reasons behind the rules are in
[`docs/engineering-notes.md`](docs/engineering-notes.md). In short:

- A test asserts a spec property, an analytic figure or an invariant, and it
  can fail. A failing test is fixed at its cause, never weakened.
- Nothing allocates, locks or waits on the audio thread.
- `unsafe` lives only in the `-ffi` crates. Every other crate forbids it.
- A comment states what the code does and the invariant it holds, in the
  present tense.

## Pull requests

Make one change per pull request, and title it as a
[Conventional Commit](https://www.conventionalcommits.org/): `fix(graph): …`,
or `feat(session)!: …` for a breaking change. The description says what
changed and, for a breaking change, how to migrate.

### Review

Every pull request is reviewed by someone other than its author before it is queued.
The reviewer reads the diff, this checklist and the design it builds, not the author's
summary of it. A change to `unsafe` code, FFI, `tutti-rt`, the audio thread or a
breaking public API also gets a second review that reads only soundness and real-time
safety. A new commit is reviewed again. The review is written in the description's
"Review" section: who reviewed, the commit they read, each finding marked blocking,
follow-up or nit, and how it was resolved. The maintainer keeps the merge decision.
The reviewer checks:

1. **Correctness.** The change does what it says; each new behaviour has a test that
   can fail, a bug fix has a test that failed before it, and no test was weakened.
2. **Soundness.** `unsafe` only in the crates CLAUDE.md allows; every `unsafe` block
   has a `// SAFETY:` comment naming its invariant and every `unsafe fn` a `# Safety`
   section; a lock-free structure lives in `tutti-rt` with a loom model of the shipped
   code.
3. **Real time.** Nothing on the audio thread allocates, frees, locks or waits; a full
   queue is counted, never waited on; the area's `AllocDisabler` test covers the new
   path.
4. **Public API.** The [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/):
   naming (`as_`/`to_`/`into_` by cost, no `get_` prefix, `try_` for a fallible twin),
   the common traits (`Debug`, `Clone`, `PartialEq`, `Default` where they make sense;
   `From`/`TryFrom` over ad-hoc converters), `#[must_use]` where dropping loses
   something, `#[non_exhaustive]` on what will grow, `Send` and `Sync` stated. By
   tutti's own rules, an error type derives `thiserror::Error` and a standalone one
   ends in `Error`, and no call hands out a lower layer's controller.
5. **Breaking changes.** Marked `!` in the title, the migration in "Breaking changes and
   migration", and the old shape deleted in the same pull request, with no
   compatibility shim.
6. **Documentation.** Every public item documented, with `# Errors` and `# Panics`
   where they apply and an example for an entry point; CHANGELOG.md has the change a
   user sees.
7. **Dependencies.** Justified in the description, at the latest stable release, pure
   Rust, `cargo xtask deny` clean, features additive, and `cargo xtask check-layout`
   holding.
8. **Performance.** A change on a hot path (the executor, a node's `process`, a DSP
   kernel) carries before-and-after numbers from the crate's benches.
9. **Platforms.** `cargo xtask check-windows` for shared code, macOS for AU and
   CoreMIDI, and a platform difference behind one module seam.
10. **Scope.** One concern; the CLAUDE.md lines it makes false are updated in it and
    listed in the description; a departure from the design is raised before merge.
11. **Coherence.** One concept has one name and one type across the workspace (the
    units, the buffer views, `Counter<Losses>`, `PushError`, `Source` for audio only);
    the change adds no second way to do what an existing item does and duplicates no
    helper; it uses the crate's constructor, error and module shapes; a new pattern
    replaces the old one everywhere in the same pull request, or is not introduced.
12. **Idiomatic Rust.** `rustfmt` and `clippy -D warnings` clean; `?`, iterators and
    borrowing where they fit; generics and `impl Trait` as the guidelines advise; no
    `unwrap` or `expect` in library code except on a stated invariant; no `as` cast
    that can truncate or wrap silently; types that make a wrong state unwritable; a
    lint silenced only with `#[expect(.., reason = "..")]`.

### Landing

Pull requests land through a merge queue: once CI is green and no blocking review
finding is open, a
maintainer queues it with `gh pr merge <number> --squash --auto`. CI runs once
more on the pull request merged with the latest `main`, and if that passes it
lands as one commit whose message is the title. There is no need to merge
`main` into a branch just to rerun CI.

## License

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be
dual licensed under MIT or Apache-2.0, without any additional terms or
conditions.
