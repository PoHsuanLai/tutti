//! The hardware inputs: what a driver thread pushes through, and the list of
//! them.
//!
//! A backend pushes inbound events into an [`InputProducerHandle`] from
//! whatever thread the OS delivers on. The handle stamps each event with the
//! frame the engine's clock puts its arrival on and pushes it into the event
//! inlet of its device (`tutti-midi-runtime`'s [`DeviceInlets`]): the audio
//! thread reads an inlet's events on their frames and converts no time. This
//! crate keeps the list of the open ports ([`HardwareMidiInputs`]): their
//! names, which are active, and what each lost.
//!
//! [`DeviceInlets`]: tutti_midi_runtime::DeviceInlets

pub(crate) mod async_port;
mod manager;

pub use async_port::InputProducerHandle;
pub use manager::{HardwareMidiInputs, PortId, PortInfo};
