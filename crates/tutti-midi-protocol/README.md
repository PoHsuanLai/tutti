# tutti-midi-protocol

This crate translates between MIDI 1.0 and MIDI 2.0 messages, for example turning a 7-bit note velocity into MIDI 2.0's 16 bits, and combining bank select, RPN and NRPN sequences into single messages.

It needs only `tutti-types` and `tutti-ump`, so you can use it on its own wherever you handle both protocols.

This crate is part of [tutti](https://github.com/PoHsuanLai/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/PoHsuanLai/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/PoHsuanLai/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
