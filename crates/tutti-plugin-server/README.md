# tutti-plugin-server

The helper program that tutti starts to run one VST3, CLAP or Audio Unit plugin in its own process.

If the plugin crashes, only this process goes down and the application can report the error and carry on. You normally do not run it yourself; `tutti-plugin` starts it when a plugin is loaded.

This crate is part of [tutti](https://github.com/tutti-audio/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/tutti-audio/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/tutti-audio/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
