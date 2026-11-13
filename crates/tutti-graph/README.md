# tutti-graph

An audio graph: you add processors, connect them, and the graph renders audio block by block, on one thread or several.

You can use it to host your own audio processors and change the graph while it plays, or to write a processor that runs inside tutti. Most people will reach it through the `tutti` crate.

This crate is part of [tutti](https://github.com/tutti-audio/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/tutti-audio/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/tutti-audio/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
