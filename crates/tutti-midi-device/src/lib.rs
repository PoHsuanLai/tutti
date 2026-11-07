//! The pure core of MIDI device detection (design 017 §5).
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

#![forbid(unsafe_code)]

mod id;
mod table;

pub use id::{DeviceId, DeviceInfo, Direction, EndpointId, Identity};
pub use table::{DeviceEvent, DeviceTable, OsChange, OsDevice, Resolved};
