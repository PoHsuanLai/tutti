# tutti-buffers

Audio as it sits in memory: the sample formats a file, a device or a plugin port holds, and borrowed views over mono, interleaved and planar runs of them.

A view carries its own width and counts in frames, so a buffer cannot be read at the wrong stride. Converting between formats and between layouts is a function over two views, and every conversion follows one written rule.

It depends only on `tutti-types` (frame counts and channel layouts), so you can use it on its own wherever you move audio between formats.

This crate is part of [tutti](https://github.com/tutti-audio/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/tutti-audio/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/tutti-audio/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
