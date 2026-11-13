# tutti-measure

This crate measures audio you already hold as samples: loudness (EBU R128), pitch, onsets, stereo correlation, spectra, tempo, key, chords and silent regions.

It reads audio and returns numbers; it does not change the sound. It is handy for drawing waveforms or analysing a recording. It needs only `tutti-types` and `tutti-dsp`, so you can use it without the rest of tutti.

This crate is part of [tutti](https://github.com/PoHsuanLai/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/PoHsuanLai/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/PoHsuanLai/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
