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

Every pull request is reviewed by someone other than its author before it is queued,
and the review is written in the description's "Review" section: who reviewed, the
commit they read, each finding marked blocking, follow-up or nit, and how it was
resolved. A new commit is reviewed again. A change to `unsafe` code, FFI, `tutti-rt`,
the audio thread or a breaking public API gets a second review that reads only
soundness and real-time safety. A pull request is queued once CI is green and no
blocking finding is open. The reviewer checks:

1. **Correctness.** The change does what it says; each new behaviour has a test that
   can fail, a bug fix has a test that failed before it, and no test was weakened.
2. **Soundness.** Every `unsafe` block has a `// SAFETY:` comment naming its invariant,
   every `unsafe fn` a `# Safety` section; a lock-free structure has a loom model of
   the shipped code.
3. **Real time.** Nothing on the audio thread allocates, frees, locks or waits.
4. **Public API.** The [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/):
   naming, the common traits, `#[must_use]`, `#[non_exhaustive]` on what will grow,
   error types that end in `Error` and derive `thiserror::Error`.
5. **Breaking changes.** Marked `!` in the title, with the migration in the
   description, and the old shape deleted in the same pull request.
6. **Documentation.** Every public item documented, with `# Errors` and `# Panics`
   where they apply and an example for an entry point; CHANGELOG.md updated.
7. **Dependencies.** Justified, at the latest stable release, pure Rust, and
   `cargo xtask deny` clean.
8. **Performance.** A hot-path change carries before-and-after benchmark numbers.
9. **Platforms.** `cargo xtask check-windows` for shared code; macOS for AU and
   CoreMIDI.
10. **Scope.** One concern; the CLAUDE.md lines it makes false are updated in it.
11. **Coherence.** One concept has one name and one type across the workspace; the
    change adds no second way to do what an existing item does and duplicates no
    helper, and it reads like the code around it.
12. **Idiomatic Rust.** `rustfmt` and `clippy -D warnings` clean; `?`, iterators and
    borrowing where they fit; no `unwrap` or `expect` in library code except on a
    stated invariant; no `as` cast that can truncate or wrap silently; types that make
    a wrong state unwritable; a lint silenced only with `#[expect(.., reason = "..")]`.

### Landing

Pull requests land through a merge queue: once one is green and reviewed, a
maintainer queues it with `gh pr merge <number> --squash --auto`. CI runs once
more on the pull request merged with the latest `main`, and if that passes it
lands as one commit whose message is the title. There is no need to merge
`main` into a branch just to rerun CI.

## License

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be
dual licensed under MIT or Apache-2.0, without any additional terms or
conditions.
