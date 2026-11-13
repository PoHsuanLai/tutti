# tutti-vst2-host

A host for VST2 plugins written in Rust, with audio, MIDI, parameters, saved state and the plugin's own editor window.

VST2 plugins always run inside the application. Most people will use it through `tutti-plugin`, but it can be used directly if you only want to host VST2 plugins in your own application; it depends on a few other tutti crates for shared types.

This crate is part of [tutti](https://github.com/PoHsuanLai/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/PoHsuanLai/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/PoHsuanLai/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
