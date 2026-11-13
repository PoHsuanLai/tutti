# tutti-plugin-host

The format-independent part of plugin hosting in tutti: a handle for controlling a loaded plugin, the plumbing that passes parameter changes and transport to it, and a node that runs a plugin inside the application.

It is an internal building block that sits between the format-specific hosts and `tutti-plugin`. Most people will use it through the `tutti` or `tutti-plugin` crate rather than directly.

This crate is part of [tutti](https://github.com/tutti-audio/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/tutti-audio/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/tutti-audio/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
