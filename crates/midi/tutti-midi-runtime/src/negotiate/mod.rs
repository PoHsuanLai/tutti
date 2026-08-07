//! The handshakes two endpoints run *before* either knows what the other
//! speaks.
//!
//! Two protocols, one purpose. They are separate because MIDI 2.0 introduced a
//! second discovery mechanism rather than replacing the first:
//!
//! - [`capability_inquiry`] — **MIDI-CI** (M2-101), carried over Universal
//!   SysEx. It predates UMP and works over a MIDI-1.0 wire, which is precisely
//!   why it is SysEx: it has to reach a device that cannot yet be asked whether
//!   it speaks MIDI 2.0.
//! - [`endpoint`] — **UMP Stream** messages (M2-104 §7.1), native to the UMP
//!   transport. Discovers endpoint name, product instance id, and function
//!   blocks — things MIDI-CI has no message for.
//!
//! A host runs both: CI to negotiate protocol and profiles, UMP Stream to learn
//! the endpoint's shape once a UMP transport exists.
//!
//! # Reassembly lives elsewhere
//!
//! MIDI-CI arrives as SysEx7, so this module's inbound path depends on
//! [`Sysex7PacketReassembler`](crate::Sysex7PacketReassembler) — which is in
//! [`crate::sysex`], not here. Rejoining fragments is a transport concern that
//! serves any SysEx payload, not just a negotiation one.

pub mod capability_inquiry;
pub mod endpoint;

pub use capability_inquiry::{CiInitiator, CiProperty, CiResponder, DiscoveredCiDevice};
pub use endpoint::{
    DeviceIdentity, DiscoveredEndpoint, EndpointInquiry, EndpointNegotiator, FunctionBlock,
};
