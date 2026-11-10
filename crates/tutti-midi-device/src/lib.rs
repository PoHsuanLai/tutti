//! The pure core of MIDI device detection (design 017 §5), and the MIDI-CI codec.
//!
//! Detection first. The CI codec ([`CiMessage`], [`Muid`], [`DiscoveryData`])
//! turns Discovery, Invalidate MUID and NAK into SysEx payload bytes and back;
//! it holds no state and no transport.
//!
//! A backend reports what the OS says as an [`OsChange`]; [`DeviceTable::apply`]
//! returns the [`DeviceEvent`] that change causes, or `None` when it alters
//! nothing. The table holds no OS handle, thread or clock, so every transition
//! is a plain test.
//!
//! ```
//! use tutti_midi_device::{
//!     DeviceEvent, DeviceTable, Direction, EndpointId, Identity, OsChange, OsDevice,
//! };
//!
//! let mut table = DeviceTable::default();
//! let keys = OsDevice {
//!     handle: EndpointId::from_raw(1),
//!     identity: Identity::alsa("Keystep", "Keystep MIDI 1"),
//!     name: "Keystep: Keystep MIDI 1".into(),
//!     direction: Direction::Input,
//! };
//! let Some(DeviceEvent::Added(info)) = table.apply(OsChange::Present(keys.clone())) else {
//!     unreachable!()
//! };
//! assert_eq!(table.list(), [info]);
//! // The same report again changes nothing.
//! assert_eq!(table.apply(OsChange::Present(keys)), None);
//! assert!(matches!(
//!     table.apply(OsChange::Gone(EndpointId::from_raw(1))),
//!     Some(DeviceEvent::Removed(_))
//! ));
//! ```

//! The UMP [`Endpoint`] and [`Connection`] are the other half: an endpoint's declared
//! state with its answers ([`Endpoint::reply`]), and the discoverer's steps over
//! received messages and time as a value (M2-104 §7.1, M2-100 Table 4).
//!
//! ```
//! use std::time::{Duration, Instant};
//! use tutti_midi_device::{Action, Connection, Ending, STEP_TIMEOUT};
//!
//! let start = Instant::now();
//! let (mut connection, discovery) = Connection::new(start);
//! // Send `discovery`. A peer that never answers is sent it again after each
//! // `STEP_TIMEOUT`, and is a MIDI 1.0 device after the third.
//! assert_eq!(connection.on_timeout(start + STEP_TIMEOUT), Action::Send(discovery));
//! connection.on_timeout(start + 2 * STEP_TIMEOUT);
//! assert_eq!(
//!     connection.on_timeout(start + 3 * STEP_TIMEOUT + Duration::from_millis(1)),
//!     Action::Settled(Ending::Midi1Fallback)
//! );
//! ```
//!
#![forbid(unsafe_code)]

mod ci;
mod connection;
mod endpoint;
mod id;
mod name;
mod table;

pub use ci::{
    CI_DEVICE_ID_FUNCTION_BLOCK, CI_SUB_ID_1, CI_UNIVERSAL_SYSEX, CI_VERSION, CiCategories,
    CiHeader, CiMessage, Destination, DiscoveryData, Muid, NO_FUNCTION_BLOCK, Nak, NotAMuid,
    SUB_ID2_DISCOVERY, SUB_ID2_DISCOVERY_REPLY, SUB_ID2_INVALIDATE_MUID, SUB_ID2_NAK,
};

pub use connection::{Action, Connection, Ending, MAX_ATTEMPTS, NAME_GRACE, STEP_TIMEOUT};
pub use endpoint::{Capabilities, Endpoint, FunctionBlock, MAX_FUNCTION_BLOCKS, UMP_VERSION};
pub use id::{DeviceId, DeviceInfo, Direction, EndpointId, Identity};
pub use name::{BlockName, EndpointName, Name, ProductInstanceId};
pub use table::{DeviceEvent, DeviceTable, OsChange, OsDevice, Resolved};
