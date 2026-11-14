# tutti-au-ffi

The low-level code for talking to Audio Unit plugins on macOS, wrapped in safe handles over Apple's AudioToolbox and CoreFoundation.

It holds all the `unsafe` code needed to host Audio Units so that `tutti-au-host` does not have to. It is empty on other platforms. Most people will use it through `tutti-au-host`, `tutti-plugin` or `tutti` rather than directly.

This crate is part of [tutti](https://github.com/tutti-audio/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/tutti-audio/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/tutti-audio/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
