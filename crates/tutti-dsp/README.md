# tutti-dsp

This crate is a collection of audio signal-processing algorithms that work on plain slices of samples: filters, compressors and limiters, delay lines, reverb, convolution, waveshapers, a resampler and an FFT.

It aims to give the same results on every CPU. It needs only `tutti-types` (units such as hertz and decibels), so you can use it on its own to build effects without a graph or audio engine.

This crate is part of [tutti](https://github.com/PoHsuanLai/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/PoHsuanLai/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/PoHsuanLai/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
