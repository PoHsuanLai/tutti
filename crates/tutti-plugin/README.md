# tutti-plugin

Loads VST3, CLAP, Audio Unit and VST2 plugins and turns each one into a node you can place in a tutti audio graph.

VST3, CLAP and Audio Unit plugins can run in a separate helper process, so a plugin that crashes does not take your application down; VST2 plugins run inside the application. This is the crate an application uses to host plugins, and it is also available through the `tutti` crate.

This crate is part of [tutti](https://github.com/PoHsuanLai/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/PoHsuanLai/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/PoHsuanLai/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
