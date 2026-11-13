# tutti-midi-file

This crate reads and writes MIDI files: Standard MIDI Files (.mid) and MIDI 2.0 Clip Files (.midi2).

A file is read into one model and written back without losing data, including SysEx and meta events. It needs only `tutti-types`, `tutti-tempo`, `tutti-midi-protocol` and `tutti-ump`, so you can use it without an audio engine.

This crate is part of [tutti](https://github.com/tutti-audio/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/tutti-audio/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/tutti-audio/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
