# tutti-midi-runtime

This crate carries MIDI through a tutti audio graph: input from devices, clips on a timeline, MIDI clock and timecode sync, and per-note expression (MPE).

It connects the MIDI value crates to the audio graph and depends on tutti's graph crate, so most people will use it through the `tutti` crate.

This crate is part of [tutti](https://github.com/PoHsuanLai/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/PoHsuanLai/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/PoHsuanLai/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
