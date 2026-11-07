//! [`DeviceTable`]: the list of present devices, and the one place a change
//! becomes an event.

use std::collections::{BTreeMap, HashMap};

use crate::id::{DeviceId, DeviceInfo, Direction, EndpointId, Identity};

/// One device as a backend sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OsDevice {
    /// The OS's handle, the key a later [`OsChange::Gone`] names it by.
    pub handle: EndpointId,
    /// What the OS knows it by. Fixed for as long as the handle is present.
    pub identity: Identity,
    /// The name to show.
    pub name: String,
    /// Which way MIDI flows.
    pub direction: Direction,
}

/// What a backend learned from the OS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OsChange {
    /// A device is present with this state. Sent when it appears, when a
    /// re-enumeration finds it, and when its name or direction changes.
    Present(OsDevice),
    /// The device with this handle is gone.
    Gone(EndpointId),
}

/// What a change did to the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceEvent {
    /// A device is listed that was not.
    Added(DeviceInfo),
    /// A device is no longer listed; the info is its last.
    Removed(DeviceInfo),
    /// A listed device's name or direction changed; its id did not.
    Changed(DeviceInfo),
}

impl DeviceEvent {
    /// The device the event is about.
    #[must_use]
    pub fn info(&self) -> &DeviceInfo {
        let (Self::Added(info) | Self::Removed(info) | Self::Changed(info)) = self;
        info
    }
}

#[derive(Debug)]
struct Entry {
    handle: EndpointId,
    info: DeviceInfo,
}

/// The devices present, by id.
///
/// Pure: it takes an [`OsChange`] and returns the [`DeviceEvent`], if any, so
/// a backend's thread and the OS never enter a test. A device is `Added` once
/// per presence: its id is listed until its handle is [`Gone`](OsChange::Gone).
#[derive(Debug, Default)]
pub struct DeviceTable {
    devices: BTreeMap<DeviceId, Entry>,
    by_handle: HashMap<EndpointId, DeviceId>,
}

impl DeviceTable {
    /// Applies `change`, returning the event it causes. A repeat of a known
    /// state, and the departure of an unknown handle, return `None`.
    pub fn apply(&mut self, change: OsChange) -> Option<DeviceEvent> {
        match change {
            OsChange::Present(device) => self.present(device),
            OsChange::Gone(handle) => self.gone(handle),
        }
    }

    fn present(&mut self, device: OsDevice) -> Option<DeviceEvent> {
        if let Some(entry) = self
            .by_handle
            .get(&device.handle)
            .and_then(|id| self.devices.get_mut(id))
        {
            let unchanged =
                entry.info.name == device.name && entry.info.direction == device.direction;
            if unchanged {
                return None;
            }
            entry.info.name = device.name;
            entry.info.direction = device.direction;
            return Some(DeviceEvent::Changed(entry.info.clone()));
        }

        // The first ordinal no listed device holds.
        let id = (0..)
            .map(|ordinal| DeviceId::new(device.identity.clone(), ordinal))
            .find(|id| !self.devices.contains_key(id))?;
        let info = DeviceInfo {
            id: id.clone(),
            name: device.name,
            direction: device.direction,
        };
        self.by_handle.insert(device.handle, id.clone());
        self.devices.insert(
            id,
            Entry {
                handle: device.handle,
                info: info.clone(),
            },
        );
        Some(DeviceEvent::Added(info))
    }

    fn gone(&mut self, handle: EndpointId) -> Option<DeviceEvent> {
        let id = self.by_handle.remove(&handle)?;
        let entry = self.devices.remove(&id)?;
        Some(DeviceEvent::Removed(entry.info))
    }

    /// The devices present, ordered by id.
    #[must_use]
    pub fn list(&self) -> Vec<DeviceInfo> {
        self.devices.values().map(|e| e.info.clone()).collect()
    }

    /// The device with this id, if present.
    #[must_use]
    pub fn get(&self, id: &DeviceId) -> Option<&DeviceInfo> {
        self.devices.get(id).map(|e| &e.info)
    }

    /// The OS handle to open `id` by, if it is present.
    #[must_use]
    pub fn handle(&self, id: &DeviceId) -> Option<EndpointId> {
        self.devices.get(id).map(|e| e.handle)
    }
}

#[cfg(test)]
mod tests;
