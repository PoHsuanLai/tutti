# tutti-au-host

A host for Audio Unit (AUv2) plugins on macOS, built on Apple's AudioToolbox.

It lists the Audio Units installed on the system, loads one, runs audio and MIDI through it, and exposes its parameters, presets, saved state and editor window. Most people will use it through `tutti-plugin`, but it can be used directly if you only want to host Audio Units in your own macOS application; it depends on a few other tutti crates for shared types.

This crate is part of [tutti](https://github.com/tutti-audio/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/tutti-audio/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/tutti-audio/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
