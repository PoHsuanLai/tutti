# tutti

A type-driven, real-time audio engine for building DAWs in Rust.

[![CI](https://img.shields.io/github/actions/workflow/status/PoHsuanLai/tutti/ci.yml?branch=main&style=flat&label=CI)](https://github.com/PoHsuanLai/tutti/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT%20or%20Apache--2.0-blue?style=flat)](#license)

Audio software has always felt a little chaotic to us: formats with their own
rules, specs with their own corners, vendor quirks no document mentions, and
systems code and DSP math in the same program. tutti is our attempt to tame
that chaos with Rust's expressive type system.

## Getting started

tutti is not on crates.io yet. Add it from git:

```toml
[dependencies]
tutti = { git = "https://github.com/PoHsuanLai/tutti" }
```

`cargo doc -p tutti --open` builds the API docs, and
[`crates/tutti/examples`](crates/tutti/examples) has whole programs. A Bevy
adapter lives at [bevy-tutti](https://github.com/PoHsuanLai/bevy-tutti).

## Getting help

Open an [issue](https://github.com/PoHsuanLai/tutti/issues).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## Supported Rust versions

tutti supports the stable Rust pinned in
[`rust-toolchain.toml`](rust-toolchain.toml), and CI tests only that
toolchain.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
