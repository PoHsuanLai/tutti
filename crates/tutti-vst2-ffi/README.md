# tutti-vst2-ffi

The low-level code for hosting VST2 plugins: the VST 2.4 data types, the plugin loader, and the callback a plugin uses to talk to its host.

It holds all the `unsafe` code needed to host VST2 so that `tutti-vst2-host` does not have to. Most people will use it through `tutti-vst2-host`, `tutti-plugin` or `tutti` rather than directly.

This crate is part of [tutti](https://github.com/tutti-audio/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/tutti-audio/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/tutti-audio/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
