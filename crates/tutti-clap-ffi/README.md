# tutti-clap-ffi

The low-level code for talking to CLAP plugins, wrapped in safe handles built on the `clap-sys` bindings.

It holds all the `unsafe` code needed to host CLAP so that `tutti-clap-host` does not have to. Most people will use it through `tutti-clap-host`, `tutti-plugin` or `tutti` rather than directly.

This crate is part of [tutti](https://github.com/PoHsuanLai/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/PoHsuanLai/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/PoHsuanLai/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
