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
