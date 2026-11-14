# tutti-midi-network

This crate carries MIDI 2.0 over a local network: Universal MIDI Packets in UDP datagrams between a Client and a Host, with the Session that connects them and the discovery that finds a Host.

It reads and writes the wire format, runs both ends of a Session as state machines that own no socket and no clock, runs one over a `std::net::UdpSocket`, and publishes and browses Hosts by mDNS. It depends only on the UMP and MIDI device crates, so you can use it on its own.

This crate is part of [tutti](https://github.com/tutti-audio/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/tutti-audio/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/tutti-audio/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
