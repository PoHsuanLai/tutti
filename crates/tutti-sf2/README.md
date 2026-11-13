# tutti-sf2

This crate reads SoundFont 2 (.sf2) files and gives you their contents as data: presets, instruments, sample headers and the sample audio.

It does not play anything; `tutti-soundfont` does that. It needs only `tutti-types` and `tutti-ump`, so you can use it on its own to inspect or convert SoundFont files.

This crate is part of [tutti](https://github.com/PoHsuanLai/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/PoHsuanLai/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/PoHsuanLai/tutti/blob/main/LICENSE-MIT) at your option.

The reading code derives from Nobuaki Tanaka's [rustysynth](https://github.com/sinshu/rustysynth) (after [MeltySynth](https://github.com/sinshu/meltysynth)), which is MIT licensed; see [its license](https://github.com/PoHsuanLai/tutti/blob/main/crates/tutti-sf2/LICENSE.txt).

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
