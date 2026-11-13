# tutti-midi-device

This crate holds the logic for keeping track of MIDI devices and for MIDI-CI, the MIDI 2.0 way for devices to discover each other and agree on profiles.

It does no input or output itself: it takes in what the operating system reports and says what changed. It needs only `tutti-ump`, so you can use it on its own.

This crate is part of [tutti](https://github.com/tutti-audio/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/tutti-audio/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/tutti-audio/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
