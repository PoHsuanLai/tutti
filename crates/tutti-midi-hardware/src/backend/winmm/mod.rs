//! The Windows backend: Windows Multimedia (winmm), the MIDI 1.0 API every
//! Windows has.
//!
//! winmm lists ports by position, names them by a product string, and moves
//! MIDI 1.0 bytes: no UMP, no Endpoint discovery, no virtual ports, and no
//! notification when a device comes or goes. This file and its siblings are
//! the calls into it; what the backend decides (identity, packing, timestamps)
//! is [`legacy_port`](super::legacy_port), which runs on every platform.
//!
//! - **Enumeration** reads the driver's name and ids at each position
//!   ([`PortInfo`]); an [`EndpointId`](tutti_midi_device::EndpointId) carries
//!   the position and a digest of what was there, so an id kept across an
//!   unplug fails to open instead of opening the device that took its place.
//! - **Input** is a winmm callback on the driver's thread. A short message is
//!   one MIDI 1.0 UMP (type 2 or type 1) and System Exclusive arrives as
//!   SysEx7 packets; the ingest node widens the first as it does for any
//!   MIDI 1.0 port. Each carries winmm's millisecond stamp since the port
//!   started, placed on the `Instant` clock.
//! - **Output** sends a short message at once and a System Exclusive message
//!   whole, waiting for the driver to take it. winmm has no timestamped send
//!   on a plain output, so an event with a send time goes out when it is
//!   queued.
//! - **The device watch** lists again once a second.
//! - **Virtual ports** do not exist in winmm: they are
//!   [`Error::Unsupported`](crate::Error::Unsupported).

mod input;
mod output;

use crate::InputProducerHandle;
use crate::capability::EndpointInfo;
use crate::devices::ChangeSink;
use crate::devices::reporter::os_devices;
use crate::endpoints::{DeviceOut, InputConnection, MidiEndpoints, Watch};
use crate::error::{Error, Result};
use tutti_midi_device::{EndpointId, OsDevice};

use super::legacy_port::poll;
use super::legacy_port::ports::{PortInfo, Side, endpoints, slot_of};

/// The Windows [`MidiEndpoints`] implementation. Holds nothing: winmm is
/// asked afresh at each call.
#[derive(Debug, Default)]
pub struct WinMmEndpoints;

impl WinMmEndpoints {
    /// The winmm backend.
    pub fn new() -> Self {
        Self
    }
}

/// What the driver reports at position `index` of `side`'s list; `None` for a
/// position the driver will not describe (a device removed since the count).
fn port_info(side: Side, index: u32) -> Option<PortInfo> {
    let caps = match side {
        Side::Input => tutti_winmm_ffi::input_caps(index),
        Side::Output => tutti_winmm_ffi::output_caps(index),
    }
    .inspect_err(|e| tracing::debug!("winmm enumeration: {e}"))
    .ok()?;
    Some(PortInfo {
        name: caps.name,
        manufacturer: caps.manufacturer,
        product: caps.product,
    })
}

/// The endpoints of `side` now.
fn list(side: Side) -> Vec<EndpointInfo> {
    let count = match side {
        Side::Input => tutti_winmm_ffi::input_count(),
        Side::Output => tutti_winmm_ffi::output_count(),
    };
    endpoints(
        side,
        (0..count).filter_map(|index| Some((index, port_info(side, index)?))),
    )
}

/// Every device now.
fn devices() -> Vec<OsDevice> {
    os_devices(&list(Side::Input), &list(Side::Output))
}

/// The position `id` names on `side`, if the port at it is still the one the
/// id was made for.
fn resolve(id: EndpointId, side: Side) -> Result<u32> {
    let slot = slot_of(id);
    if slot.side != side {
        return Err(Error::device(format!(
            "{id:?} is not a WinMM {side:?} endpoint id"
        )));
    }
    match port_info(side, slot.index) {
        Some(found) if slot.matches(&found) => Ok(slot.index),
        _ => Err(Error::device(format!(
            "{id:?} names no WinMM {side:?} port now (the device was removed)"
        ))),
    }
}

impl MidiEndpoints for WinMmEndpoints {
    fn watch(&self, sink: ChangeSink) -> Result<Box<dyn Watch>> {
        Ok(Box::new(poll::start(sink, devices, poll::INTERVAL)?))
    }

    fn inputs(&self) -> Vec<EndpointInfo> {
        list(Side::Input)
    }

    fn outputs(&self) -> Vec<EndpointInfo> {
        list(Side::Output)
    }

    fn open_input(
        &self,
        id: EndpointId,
        producer: InputProducerHandle,
    ) -> Result<Box<dyn InputConnection>> {
        let index = resolve(id, Side::Input)?;
        Ok(Box::new(input::open(index, producer)?))
    }

    fn open_output(&self, id: EndpointId) -> Result<Box<dyn DeviceOut>> {
        let index = resolve(id, Side::Output)?;
        Ok(Box::new(output::open(index)?))
    }
}
