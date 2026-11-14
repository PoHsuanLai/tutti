# tutti-ump

This crate stores and reads MIDI 2.0's Universal MIDI Packet, the 32-bit-word format that carries both MIDI 1.0 and MIDI 2.0 messages.

It has typed values for groups, channels and controller numbers, decodes and encodes messages, parses MIDI 1.0 byte streams and handles SysEx. It does not depend on the rest of tutti, so you can use it on its own.

This crate is part of [tutti](https://github.com/tutti-audio/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/tutti-audio/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/tutti-audio/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
