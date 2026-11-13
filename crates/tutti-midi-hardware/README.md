# tutti-midi-hardware

This crate talks to MIDI devices through the operating system: it lists them, watches for plug and unplug, and sends and receives MIDI 2.0 packets using CoreMIDI on macOS and ALSA on Linux.

It builds on other tutti crates, so most people will use it through the `tutti` crate. On other platforms it finds no devices.

This crate is part of [tutti](https://github.com/PoHsuanLai/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/PoHsuanLai/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/PoHsuanLai/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
