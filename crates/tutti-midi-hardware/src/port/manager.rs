//! [`HardwareMidiInputs`]: every open input port, and the event inlet each
//! one pushes into.
//!
//! The list is control-side state: the audio thread reads nothing here. A port
//! is a slot of the shared [`DeviceInlets`]; opening one takes the slot and
//! returns the [`InputProducerHandle`] the driver thread pushes through, and
//! removing it gives the slot back.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tutti_midi_runtime::{DeviceInlets, NoFreeSlot};

use super::async_port::{HardwareMidiInput, InputProducerHandle};

/// One port's identity: its slot in the manager, and which of the ports ever
/// created there it is.
///
/// A removed port's slot is reused by the next port created, so the slot
/// alone does not name a port: an id kept past its port's removal would
/// otherwise reach whichever port took the slot. The generation tells them
/// apart. Every method taking a `PortId` checks both, and a stale id (its port
/// removed, whether or not the slot was reused) matches nothing: it removes
/// nothing, toggles nothing, and reads as no port.
///
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PortId {
    index: usize,
    generation: u64,
}

impl PortId {
    /// The slot this port occupies. Unique among the ports open at one time,
    /// and reused after the port is removed; compare `PortId`s, not slots, to
    /// tell ports apart.
    #[must_use]
    pub fn index(self) -> usize {
        self.index
    }

    /// Which port created in this manager this is: unique for the manager's
    /// life, increasing in creation order.
    #[must_use]
    pub fn generation(self) -> u64 {
        self.generation
    }
}

/// A snapshot view of one port, computed on demand from the underlying
/// `HardwareMidiInput`. The port is the single source of truth for `name` and
/// `active` — `PortInfo` just bundles them with the port's id/type for
/// listing. `active` is a point-in-time value, not a live handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortInfo {
    /// The port's identity, and the key every other method on
    /// [`HardwareMidiInputs`] takes.
    pub id: PortId,
    /// The endpoint name the port was created with — for an open connection,
    /// the name the OS reported for that device.
    pub name: String,
    /// Whether the port delivers. An inactive port's events are discarded at
    /// the driver's thread, not queued.
    pub active: bool,
    /// How many events the port's inlet had no room for, since the port
    /// opened.
    pub dropped: u64,
    /// How many events reached the graph after their frame had begun, and
    /// sounded at the first frame of the block that found them: late, not
    /// lost. The lead the inlets place arrivals with
    /// ([`DeviceInlets`]) is what prevents it.
    pub late: u64,
}

impl PortInfo {
    fn of(port: &HardwareMidiInput) -> Self {
        Self {
            id: port.id(),
            name: port.name().to_string(),
            active: port.is_active(),
            dropped: port.dropped(),
            late: port.late(),
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
/// [`remove_input_port`](Self::remove_input_port) retires the port and frees
/// its slot for the next one, so hot-plug cycles do not grow the set.
///
/// Cheap to share: hold it in an `Arc` and hand clones to the session.
pub struct HardwareMidiInputs {
    inlets: Arc<DeviceInlets>,
    ports: Mutex<Ports>,
    /// The drops of the ports removed so far, so a retired port's count stays
    /// in [`dropped_events`](Self::dropped_events).
    retired_dropped: AtomicU64,
}

/// The port list: one slot per index, `None` where a port was removed and no
/// new one has taken its place yet.
#[derive(Default)]
struct Ports {
    slots: Vec<Option<Arc<HardwareMidiInput>>>,
    next_generation: u64,
}

impl HardwareMidiInputs {
    /// A list with no ports, each port an inlet of `inlets`.
    #[must_use]
    pub fn new(inlets: Arc<DeviceInlets>) -> Self {
        Self {
            inlets,
            ports: Mutex::new(Ports::default()),
            retired_dropped: AtomicU64::new(0),
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

    /// Events the ports' inlets had no room for, over all ports, current and
    /// removed, since this list was built. An inlet fills only when events
    /// arrive faster than blocks take them for long enough to fill it. Control
    /// side.
    #[must_use]
    pub fn dropped_events(&self) -> u64 {
        let live: u64 = self
            .ports()
            .slots
            .iter()
            .flatten()
            .map(|p| p.dropped())
            .sum();
        live + self.retired_dropped.load(Ordering::Relaxed)
    }

    /// Creates an input port named `name` and returns the **only** producer
    /// handle for it; [`InputProducerHandle::port_id`] is its identity.
    ///
    /// The port takes the lowest free index (one a removed port left), or the
    /// next one past the end, a generation no port before it had, and a slot
    /// of the inlets. It starts **active**. Control side: it allocates the
    /// inlet. Safe to call from several threads at once.
    ///
    /// There is no other way to reach a port's producer: give the handle to the
    /// one thread that pushes. Dropping it leaves the port fed by nothing;
    /// [`remove_input_port`](Self::remove_input_port) retires it.
    ///
    /// # Errors
    ///
    /// [`NoFreeSlot`] when every slot of the inlets is held: no port is made.
    #[must_use = "the handle is the port's only producer; dropping it leaves the port fed by nothing"]
    pub fn open_input_port(
        &self,
        name: impl Into<String>,
    ) -> Result<InputProducerHandle, NoFreeSlot> {
        let name = name.into();
        let mut ports = self.ports();
        let input = self.inlets.open()?;
        let index = ports
            .slots
            .iter()
            .position(Option::is_none)
            .unwrap_or(ports.slots.len());
        let id = PortId {
            index,
            generation: ports.next_generation,
        };
        ports.next_generation += 1;
        let (port, handle) = HardwareMidiInput::new(name, input, id);
        let port = Some(Arc::new(port));
        match ports.slots.get_mut(index) {
            Some(slot) => *slot = port,
            None => ports.slots.push(port),
        }
        Ok(handle)
    }

    /// Retires the port `id` names, freeing its slot for the next
    /// [`open_input_port`](Self::open_input_port). Returns `false`, and
    /// changes nothing, when `id` names no live port: one already removed,
    /// including one whose index a newer port has taken.
    ///
    /// Events still in its inlet are discarded with it. Close the port's driver
    /// connection first, so nothing is still pushing; a handle that outlives
    /// the port is harmless, and whatever it pushes is lost. Control side.
    pub fn remove_input_port(&self, id: PortId) -> bool {
        let mut ports = self.ports();
        let removed = match ports.slots.get_mut(id.index) {
            Some(slot) if slot.as_ref().is_some_and(|port| port.id() == id) => slot.take(),
            _ => None,
        };
        // Trailing empty slots go too, so the list shrinks back once the
        // last port is gone rather than keeping its high-water mark.
        while ports.slots.last().is_some_and(Option::is_none) {
            ports.slots.pop();
        }
        match removed {
            Some(port) => {
                self.retired_dropped
                    .fetch_add(port.dropped(), Ordering::Relaxed);
                self.inlets.close(port.slot());
                true
            }
            None => false,
        }
    }

    /// How many indices the list holds, live or free: bounded by the most
    /// ports ever open at once.
    #[cfg(test)]
    pub(crate) fn slot_count(&self) -> usize {
        self.ports().slots.len()
    }

    /// The live port `id` names, if any: a stale id names none.
    fn port(&self, id: PortId) -> Option<Arc<HardwareMidiInput>> {
        self.ports()
            .slots
            .get(id.index)
            .and_then(Option::clone)
            .filter(|port| port.id() == id)
    }

    /// A snapshot of one port, or `None` when `id` names no live port.
    pub fn port_info(&self, id: PortId) -> Option<PortInfo> {
        self.port(id).map(|port| PortInfo::of(&port))
    }

    /// Every input port, in index order.
    pub fn list_input_ports(&self) -> Vec<PortInfo> {
        self.ports()
            .slots
            .iter()
            .flatten()
            .map(|port| PortInfo::of(port))
            .collect()
    }

    /// Sets whether the port `id` names delivers. Returns `false`, and changes
    /// nothing, when `id` names no live port.
    ///
    /// An inactive port discards what the driver pushes: nothing is queued
    /// while it is paused, so there is no burst when it is active again.
    pub fn set_port_active(&self, id: PortId, active: bool) -> bool {
        match self.port(id) {
            Some(port) => {
                port.set_active(active);
                true
            }
            None => false,
        }
    }

    /// Whether the port `id` names delivers. `false` for a stale id.
    pub fn is_port_active(&self, id: PortId) -> bool {
        self.port(id).is_some_and(|port| port.is_active())
    }
}

impl core::fmt::Debug for HardwareMidiInputs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HardwareMidiInputs")
            .field(
                "num_input_ports",
                &self.ports().slots.iter().filter(|p| p.is_some()).count(),
            )
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
