# tutti-clap-host

A host for CLAP plugins written in Rust, with no `unsafe` code needed on your side.

It loads a CLAP plugin, runs audio and MIDI through it, and exposes its parameters, saved state and editor window. Most people will use it through `tutti-plugin`, but it can be used directly if you only want to host CLAP plugins in your own application; it depends on a few other tutti crates for shared types.

This crate is part of [tutti](https://github.com/tutti-audio/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/tutti-audio/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/tutti-audio/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
