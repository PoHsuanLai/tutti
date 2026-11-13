# arc-lend

arc-lend is a shared-value cell built for threads that run against a deadline: one thread replaces the value while others read it without waiting or freeing memory.

Such a thread might be an audio callback, a game's fixed update or a control loop. The writer keeps ownership of old values and frees them later, so readers only borrow.

arc-lend was built for [tutti](https://github.com/PoHsuanLai/tutti), a real-time audio engine for Rust, but does not depend on it.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/PoHsuanLai/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/PoHsuanLai/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
