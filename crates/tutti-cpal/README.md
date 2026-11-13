# tutti-cpal

Connects tutti to a sound card using the cpal library: it lists audio devices, plays audio out and records from a microphone.

It is the only tutti crate that talks to the operating system's audio, so offline rendering and tests never need it. Most people will use it through the `tutti` crate.

This crate is part of [tutti](https://github.com/tutti-audio/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/tutti-audio/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/tutti-audio/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
