//! [`HardwareMidiInputs`]: every open input port, and the event inlet each
//! one pushes into.
//!
//! The list is control-side state: the audio thread reads nothing here. A port
//! is a slot of the shared [`DeviceInlets`], named by its [`DeviceSlot`] (the
//! one id: the slot's index and which device took it, so a stale id names
//! nothing); opening one takes the slot and returns the [`InputProducerHandle`]
//! the driver thread pushes through, which holds the slot until it drops.
//! Removing the port retires its entry in this list.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use tutti_graph::Losses;
use tutti_midi_runtime::{DeviceInlets, DeviceSlot, NoFreeSlot};

use super::async_port::{HardwareMidiInput, InputProducerHandle, StreamDrain};

/// A snapshot view of one port, computed on demand from the underlying
/// `HardwareMidiInput`. The port is the single source of truth for `name` and
/// `active` — `PortInfo` just bundles them with the port's id/type for
/// listing. `active` is a point-in-time value, not a live handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortInfo {
    /// The port's identity, and the key every other method on
    /// [`HardwareMidiInputs`] takes.
    pub id: DeviceSlot,
    /// The endpoint name the port was created with — for an open connection,
    /// the name the OS reported for that device.
    pub name: String,
    /// Whether the port delivers. An inactive port's events are discarded at
    /// the driver's thread, not queued.
    pub active: bool,
    /// What the port's inlet has lost since the port opened, in events:
    /// `lost` for want of room in its ring, `evicted` from its waiting list,
    /// `refused` by the graph's port, and `late`: events that reached the graph
    /// after their frame had begun and sounded at the first frame of the block
    /// that found them (late, not lost; the lead the inlets place arrivals with
    /// ([`DeviceInlets`]) is what prevents it).
    pub losses: Losses,
    /// How many UMP Stream messages the port's stream ring had no room for,
    /// since the port opened.
    pub stream_lost: u64,
}

impl PortInfo {
    fn of(id: DeviceSlot, port: &HardwareMidiInput) -> Self {
        Self {
            id,
            name: port.name().to_string(),
            active: port.is_active(),
            losses: port.losses(),
            stream_lost: port.stream_lost(),
        }
    }
}

/// Every open hardware MIDI input.
///
/// A backend creates one port per connection with
/// [`open_input_port`](Self::open_input_port) and pushes into the
/// [`InputProducerHandle`] it returns from the driver's own thread; each port
/// is an event inlet of the [`DeviceInlets`] the list was built over, which the
/// graph's owner wires to the ingest node. When the connection closes,
/// [`remove_input_port`](Self::remove_input_port) retires the port, and
/// dropping the handle gave its slot back, so hot-plug cycles do not grow the
/// set.
///
/// Cheap to share: hold it in an `Arc` and hand clones to the session.
pub struct HardwareMidiInputs {
    inlets: Arc<DeviceInlets>,
    ports: Mutex<Ports>,
}

/// The port list, keyed by the slot each port holds (in index order), and the
/// losses of the ports removed so far. They share one lock so a port moves
/// from the one to the other in one step, and a [`losses`](HardwareMidiInputs::losses)
/// read never counts it twice or not at all.
#[derive(Default)]
struct Ports {
    live: BTreeMap<DeviceSlot, Arc<HardwareMidiInput>>,
    /// A retired port's counts stay in [`losses`](HardwareMidiInputs::losses).
    retired: Losses,
}

impl HardwareMidiInputs {
    /// A list with no ports, each port an inlet of `inlets`.
    #[must_use]
    pub fn new(inlets: Arc<DeviceInlets>) -> Self {
        Self {
            inlets,
            ports: Mutex::new(Ports::default()),
        }
    }

    /// The inlets the ports are slots of: the graph's owner takes their
    /// changes from it.
    #[must_use]
    pub fn inlets(&self) -> &Arc<DeviceInlets> {
        &self.inlets
    }

    fn ports(&self) -> std::sync::MutexGuard<'_, Ports> {
        // A poisoned lock guards nothing inconsistent: every change is whole.
        self.ports.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What the ports' inlets have lost, summed over all ports, current and
    /// removed, since this list was built (see [`PortInfo::losses`]). An inlet
    /// fills only when events arrive faster than blocks take them for long
    /// enough to fill it. Control side.
    #[must_use]
    pub fn losses(&self) -> Losses {
        let ports = self.ports();
        let live: Losses = ports.live.values().map(|p| p.losses()).sum();
        live + ports.retired
    }

    /// Creates an input port named `name` and returns the **only** producer
    /// handle for it; [`InputProducerHandle::slot`] is its identity.
    ///
    /// The port takes the lowest free slot of the inlets, a generation no
    /// port before it had, and starts **active**. Control side: it allocates
    /// the inlet. Safe to call from several threads at once.
    ///
    /// There is no other way to reach a port's producer: give the handle to the
    /// one thread that pushes. **The slot is the handle's: dropping it frees
    /// the slot** (and queues the inlet's removal), but leaves the port's entry
    /// in the list until [`remove_input_port`](Self::remove_input_port)
    /// retires it, and the entry then reads as a port fed by nothing.
    ///
    /// # Errors
    ///
    /// [`NoFreeSlot`] when every slot of the inlets is held: no port is made.
    #[must_use = "the handle is the port's only producer, and holds its slot: dropping it leaves the port fed by nothing"]
    pub fn open_input_port(
        &self,
        name: impl Into<String>,
    ) -> Result<InputProducerHandle, NoFreeSlot> {
        let name = name.into();
        let mut ports = self.ports();
        let input = self.inlets.open()?;
        let slot = input.slot();
        let (port, handle) = HardwareMidiInput::new(name, input);
        ports.live.insert(slot, Arc::new(port));
        Ok(handle)
    }

    /// Retires the port `id` names. Returns `false`, and changes nothing, when
    /// `id` names no live port: one already removed, including one whose
    /// index a newer port has taken.
    ///
    /// Events still in its inlet are discarded with it. Close the port's driver
    /// connection first (drop its [`InputProducerHandle`], which frees the
    /// slot), so nothing is still pushing; a handle that outlives the port is
    /// harmless, and whatever it pushes is lost. Control side.
    pub fn remove_input_port(&self, id: DeviceSlot) -> bool {
        let mut ports = self.ports();
        match ports.live.remove(&id) {
            Some(port) => {
                ports.retired += port.losses();
                true
            }
            None => false,
        }
    }

    /// How many ports the list holds.
    #[cfg(test)]
    pub(crate) fn port_count(&self) -> usize {
        self.ports().live.len()
    }

    /// The live port `id` names, if any: a stale id names none.
    fn port(&self, id: DeviceSlot) -> Option<Arc<HardwareMidiInput>> {
        self.ports().live.get(&id).cloned()
    }

    /// A snapshot of one port, or `None` when `id` names no live port.
    pub fn port_info(&self, id: DeviceSlot) -> Option<PortInfo> {
        self.port(id).map(|port| PortInfo::of(id, &port))
    }

    /// Every input port, in index order.
    pub fn list_input_ports(&self) -> Vec<PortInfo> {
        self.ports()
            .live
            .iter()
            .map(|(id, port)| PortInfo::of(*id, port))
            .collect()
    }

    /// The UMP Stream messages the port `id` names has held since the last
    /// read, oldest first, taken as the iterator is read: what the device said
    /// about itself (`InputProducerHandle::push` keeps them out of the inlet).
    /// Nothing for a stale id. **No lock is held between steps**, so the
    /// caller may do what it likes with each message. Control side, one reader
    /// at a time.
    pub fn drain_stream(&self, id: DeviceSlot) -> StreamDrain {
        StreamDrain::new(self.port(id))
    }

    /// Sets whether the port `id` names delivers. Returns `false`, and changes
    /// nothing, when `id` names no live port.
    ///
    /// An inactive port discards what the driver pushes: nothing is queued
    /// while it is paused, so there is no burst when it is active again.
    pub fn set_port_active(&self, id: DeviceSlot, active: bool) -> bool {
        match self.port(id) {
            Some(port) => {
                port.set_active(active);
                true
            }
            None => false,
        }
    }

    /// Whether the stream ring of the port `id` names also takes a copy of the
    /// SysEx7 packets the device sends (`drain_stream` then yields them beside
    /// the Stream messages), for a control side that is waiting for an Identity
    /// Reply. The packets still reach the graph. `false` for a stale id.
    pub fn hear_sysex(&self, id: DeviceSlot, hear: bool) -> bool {
        match self.port(id) {
            Some(port) => {
                port.set_hear_sysex(hear);
                true
            }
            None => false,
        }
    }

    /// Whether the port `id` names delivers. `false` for a stale id.
    pub fn is_port_active(&self, id: DeviceSlot) -> bool {
        self.port(id).is_some_and(|port| port.is_active())
    }
}

impl core::fmt::Debug for HardwareMidiInputs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HardwareMidiInputs")
            .field("num_input_ports", &self.ports().live.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
