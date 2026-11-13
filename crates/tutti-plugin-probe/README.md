# tutti-plugin-probe

A small program that opens a plugin file just far enough to read its name and details, writes them to a file, and exits.

tutti starts it while scanning for plugins, so that a plugin which crashes on load is lost in this short-lived process rather than in your application. You normally do not run it yourself.

This crate is part of [tutti](https://github.com/PoHsuanLai/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/PoHsuanLai/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/PoHsuanLai/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
