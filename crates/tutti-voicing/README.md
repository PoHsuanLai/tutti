# tutti-voicing

This crate decides what a note-driven instrument should do with each MIDI message: start a note, release it, cut it, take over a voice when all are busy, or bend the pitch.

It makes no sound and holds no audio, so a synthesizer or sampler can use it for its note logic and do its own rendering. It needs only `tutti-ump`, `tutti-midi-protocol` and `tutti-types`, so you can use it on its own.

This crate is part of [tutti](https://github.com/PoHsuanLai/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/PoHsuanLai/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/PoHsuanLai/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
