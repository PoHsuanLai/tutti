# tutti-coremidi-ffi

This crate is a small wrapper that lets tutti call parts of Apple's CoreMIDI and the host clock from safe Rust, such as MIDI 2.0 virtual ports and device endpoint information.

It is an internal building block of `tutti-midi-hardware` and is empty off macOS. Most people will use it through the `tutti` crate rather than directly.

This crate is part of [tutti](https://github.com/tutti-audio/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/tutti-audio/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/tutti-audio/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
