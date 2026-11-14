# tutti-soundfont

This crate plays SoundFont 2 (.sf2) instruments: you load a SoundFont, pick a preset and send it MIDI notes, and it produces audio.

It is a node for a tutti audio graph, so it needs the rest of the graph crates and most people will use it through the `tutti` crate. Reading the file itself is done by `tutti-sf2`, which is usable on its own.

This crate is part of [tutti](https://github.com/tutti-audio/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/tutti-audio/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/tutti-audio/tutti/blob/main/LICENSE-MIT) at your option.

The voice rendering derives from Nobuaki Tanaka's [rustysynth](https://github.com/sinshu/rustysynth) (after [MeltySynth](https://github.com/sinshu/meltysynth)), which is MIT licensed; see [its license](https://github.com/tutti-audio/tutti/blob/main/crates/tutti-soundfont/LICENSE-RUSTYSYNTH.txt).

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
