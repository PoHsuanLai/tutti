//! The Windows backend: Windows Multimedia (winmm), the MIDI 1.0 API every
//! Windows has.
//!
//! winmm lists ports by position, names them by a product string, and moves
//! MIDI 1.0 bytes: no UMP, no Endpoint discovery, no virtual ports, and no
//! notification when a device comes or goes. This file and its siblings are
//! the calls into it; what the backend decides (identity, packing, timestamps)
//! is [`legacy_port`](super::legacy_port), which runs on every platform.
//!
//! - **Enumeration** names each port by the device interface path the driver
//!   reports (`DRV_QUERYDEVICEINTERFACE`), which survives a replug, and by its
//!   product name and ids when it reports none. An
//!   [`EndpointId`](tutti_midi_device::EndpointId) carries the position and a
//!   digest of that name: opening looks for the device where the id says and,
//!   when the list has shifted, where it went, and fails when it is gone
//!   instead of opening the device that took its place. The name shown is
//!   winmm's product name (31 characters at most): the longer one lives in the
//!   device's property store, which reading takes a second API and a query per
//!   port for.
//! - **Input** is a winmm callback on the driver's thread. A short message is
//!   one MIDI 1.0 UMP (type 2 or type 1) and System Exclusive arrives as
//!   SysEx7 packets; the ingest node widens the first as it does for any
//!   MIDI 1.0 port. Each carries winmm's millisecond stamp since the port
//!   started, placed on the `Instant` clock.
//! - **Output** is a thread of its own: it holds each event until its time
//!   (a high-resolution waitable timer; one already due goes at once) and
//!   sends them in order, a SysEx message whole, waiting for the driver to
//!   take it.
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
use tutti_midi_device::{EndpointId, OsDevice, WinMmDevice};

use super::legacy_port::poll;
use super::legacy_port::ports::{PortInfo, Side, endpoints, locate, slot_of};

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
/// The device is named by its interface path where the driver gives one.
fn port_info(side: Side, index: u32) -> Option<PortInfo> {
    let (caps, interface) = match side {
        Side::Input => (
            tutti_winmm_ffi::input_caps(index),
            tutti_winmm_ffi::input_interface(index),
        ),
        Side::Output => (
            tutti_winmm_ffi::output_caps(index),
            tutti_winmm_ffi::output_interface(index),
        ),
    };
    let caps = caps
        .inspect_err(|e| tracing::debug!("winmm enumeration: {e}"))
        .ok()?;
    let device = match interface {
        Some(path) => WinMmDevice::Interface(path),
        None => WinMmDevice::Named {
            name: caps.name.clone(),
            manufacturer: caps.manufacturer,
            product: caps.product,
        },
    };
    Some(PortInfo {
        name: caps.name,
        device,
    })
}

/// Every port of `side`, with its position.
fn ports(side: Side) -> impl Iterator<Item = (u32, PortInfo)> {
    let count = match side {
        Side::Input => tutti_winmm_ffi::input_count(),
        Side::Output => tutti_winmm_ffi::output_count(),
    };
    (0..count).filter_map(move |index| Some((index, port_info(side, index)?)))
}

/// The endpoints of `side` now.
fn list(side: Side) -> Vec<EndpointInfo> {
    endpoints(side, ports(side))
}

/// Every device now.
fn devices() -> Vec<OsDevice> {
    os_devices(&list(Side::Input), &list(Side::Output))
}

/// The position of the port `id` names on `side` now, wherever the list has
/// moved it; an error when the device is gone.
fn resolve(id: EndpointId, side: Side) -> Result<u32> {
    let slot = slot_of(id);
    if slot.side != side {
        return Err(Error::device(format!(
            "{id:?} is not a WinMM {side:?} endpoint id"
        )));
    }
    locate(slot, ports(side)).ok_or_else(|| {
        Error::device(format!(
            "{id:?} names no WinMM {side:?} port now (the device was removed)"
        ))
    })
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
