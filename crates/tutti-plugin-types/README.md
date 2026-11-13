# tutti-plugin-types

Shared types for hosting audio plugins: plugin descriptions, parameters, audio buffers, transport information, editor windows and presets.

These types are common to every plugin format, so the VST3, CLAP, Audio Unit and VST2 host crates all speak the same vocabulary. Most people will meet them through `tutti-plugin` rather than depending on this crate directly.

This crate is part of [tutti](https://github.com/tutti-audio/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/tutti-audio/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/tutti-audio/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
