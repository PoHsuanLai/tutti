# tutti-alsa-ffi

This crate is a small wrapper that lets tutti call the Linux ALSA sequencer and its MIDI 2.0 (UMP) interface from safe Rust.

It is an internal building block of `tutti-midi-hardware`. It needs ALSA 1.2.10 or newer and is empty on other systems. Most people will use it through the `tutti` crate rather than directly.

This crate is part of [tutti](https://github.com/PoHsuanLai/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/PoHsuanLai/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/PoHsuanLai/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
