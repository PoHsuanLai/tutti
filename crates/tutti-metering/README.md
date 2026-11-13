# tutti-metering

Measures how busy the audio thread is: how much of the time it has for each piece of audio it actually spends producing it.

It is useful for showing a CPU load meter in an audio application. It depends only on `tutti-types` (units such as hertz and decibels), so it can be used on its own.

This crate is part of [tutti](https://github.com/PoHsuanLai/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/PoHsuanLai/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/PoHsuanLai/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
