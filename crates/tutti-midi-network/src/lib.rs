#![doc = include_str!("../README.md")]
//! Network MIDI 2.0 over UDP (M2-124).
//!
//! The crate has four layers, each usable without the ones above it.
//!
//! - The wire format: [`Command`] is one Command Packet as a value, written by
//!   [`Command::write`] and read by [`Command::parse`]; [`parse`] reads a UDP
//!   payload's signature and yields its commands, [`DatagramBuilder`] writes one.
//!   Every refusal is a named [`ParseError`] or [`SignatureError`].
//! - The session machines: [`Client`] and [`Host`] hold the session states of
//!   section 6, authentication, keep-alive Pings, Session Reset, forward error
//!   correction, retransmit and the idle declaration. They take datagrams and
//!   an [`Instant`](std::time::Instant), and hand back [`Transmit`]s and
//!   [`Event`]s ([`Endpoint`]); they own no socket and read no clock.
//! - [`UdpDriver`] runs an [`Endpoint`] over a `std::net::UdpSocket`.
//! - [`Advertisement`] and [`Browser`] publish and find Hosts by mDNS.
//!
//! ```
//! use std::net::SocketAddr;
//! use std::time::Instant;
//! use tutti_midi_device::{EndpointName, ProductInstanceId};
//! use tutti_midi_network::{
//!     Client, Credentials, Endpoint, EndpointIdentity, Event, Host, HostConfig, SessionConfig,
//!     SessionState,
//! };
//!
//! let identity = |name: &str, id: &str| EndpointIdentity {
//!     name: EndpointName::new(name).unwrap(),
//!     instance_id: ProductInstanceId::new(id).unwrap(),
//! };
//! let (host_addr, client_addr): (SocketAddr, SocketAddr) =
//!     ("127.0.0.1:5673".parse().unwrap(), "127.0.0.1:50000".parse().unwrap());
//! let now = Instant::now();
//!
//! let mut host = Host::new(HostConfig::new(identity("Studio", "HOST-1")));
//! let mut client = Client::new(
//!     identity("Laptop", "CLIENT-1"),
//!     Credentials::none(),
//!     SessionConfig::default(),
//!     host_addr,
//!     now,
//! );
//! client.connect(now);
//! // The invitation travels to the host, the acceptance back.
//! while let Some(sent) = client.poll_transmit() {
//!     host.handle_datagram(now, client_addr, &sent.payload);
//! }
//! while let Some(sent) = host.poll_transmit() {
//!     client.handle_datagram(now, host_addr, &sent.payload);
//! }
//! assert_eq!(client.state(), SessionState::Established);
//! assert!(matches!(client.poll_event(), Some(Event::Established { .. })));
//! ```
//!
//! # Not here
//!
//! Neither machine is wired into the engine's MIDI device layer. A Session
//! carries UMPs; the UMP Stream conversation of Endpoint and Function Block
//! discovery is the application's, over [`Event::Ump`] and [`Client::send`].

#![forbid(unsafe_code)]

mod auth;
mod command;
mod config;
mod datagram;
mod discovery;
mod endpoint;
mod event;
mod link;
mod policy;
mod sequence;
mod session;
mod text;
mod udp;

pub use auth::{Digest, Nonce};
pub use command::{
    AuthChallenge, AuthMethods, AuthState, Bye, ByeReason, Command, EndpointIdentity, Header,
    Invitation, MAX_UMP_DATA_WORDS, Nak, NakReason, ParseError, PingId, RetransmitErrorReason,
    RetransmitRefusal, RetransmitRequest, UmpData, UmpDataError, UserAuthChallenge, UserAuthState,
    UserAuthentication,
};
pub use config::SessionConfig;
pub use datagram::{
    Commands, DatagramBuilder, MAX_DATAGRAM_BYTES, Refused, SIGNATURE, SignatureError, parse,
};
pub use discovery::{
    Advertisement, Browser, DiscoveryError, FoundHost, HostEvent, KEY_ENDPOINT_NAME,
    KEY_PRODUCT_INSTANCE_ID, SERVICE_TYPE, instance_name, txt_properties,
};
pub use endpoint::{Client, Endpoint, Host, HostConfig};
pub use event::{AuthFailure, Event, Initiator, SendError, SessionState, Transmit};
pub use policy::{Admission, Credentials};
pub use sequence::SequenceNumber;
pub use text::{BoundedText, Text, TextError, Username};
pub use udp::UdpDriver;
