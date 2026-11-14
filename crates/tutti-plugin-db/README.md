# tutti-plugin-db

Finds audio plugins installed on a machine and remembers what it found in a database file.

It walks plugin folders, records each plugin file's details, and notes files that crashed while being scanned so they are skipped next time. It only depends on tutti's plugin type definitions, so it can be used on its own to build a plugin list; saving to a file needs the `json` feature.

This crate is part of [tutti](https://github.com/tutti-audio/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/tutti-audio/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/tutti-audio/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
