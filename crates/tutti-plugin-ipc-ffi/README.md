# tutti-plugin-ipc-ffi

The low-level operating-system code that tutti's plugin helper process needs, wrapped in safe functions.

This includes the memory shared between the application and the helper, and a few platform calls for waiting on sockets and checking whether a process is alive. It exists so that the crates above it can avoid `unsafe` code. Most people will use it through the `tutti` or `tutti-plugin` crate rather than directly.

This crate is part of [tutti](https://github.com/tutti-audio/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/tutti-audio/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/tutti-audio/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
