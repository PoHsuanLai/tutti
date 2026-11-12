//! [`DeviceTable`]: the list of present devices, and the one place a change
//! becomes an event.

use std::collections::{BTreeMap, HashMap};

use crate::id::{DeviceId, DeviceInfo, EndpointId, Identity, PortDirection};

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
    pub direction: PortDirection,
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

/// What a saved device names among the devices present now.
///
/// Only [`Exact`](Self::Exact) is safe to bind without asking. What an
/// identity can promise differs: CoreMIDI's unique id names one endpoint and a
/// Product Instance Id names one unit, but an ALSA identity is only a name, so
/// another keyboard of the same model answers to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// The identity names a unit and exactly one present device is the saved
    /// one: a CoreMIDI endpoint with the saved unique id, or the port of a
    /// Product Instance Id with the saved name and direction.
    Exact(DeviceInfo),
    /// Exactly one present device answers to the saved identity, and the
    /// identity is only a name (ALSA): it may be the saved device or another
    /// of the same name. An app can bind it or ask.
    SameName(DeviceInfo),
    /// Several present devices could be the saved one, or the saved ordinal is
    /// above 0 and cannot be trusted across a restart. These are the
    /// candidates, ordered by id; an app asks.
    Ambiguous(Vec<DeviceInfo>),
    /// No present device has the saved identity.
    Missing,
}

/// Whether a device that goes `now` still carries what `saved` used it for.
fn serves(now: PortDirection, saved: PortDirection) -> bool {
    (now.is_input() || !saved.is_input()) && (now.is_output() || !saved.is_output())
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
    pub fn list(&self) -> impl ExactSizeIterator<Item = &DeviceInfo> + '_ {
        self.devices.values().map(|e| &e.info)
    }

    /// The device with this id, if present.
    #[must_use]
    pub fn get(&self, id: &DeviceId) -> Option<&DeviceInfo> {
        self.devices.get(id).map(|e| &e.info)
    }

    /// What `saved`, a device listed in an earlier run, names now.
    ///
    /// An ordinal tells alike devices apart within one run only (see
    /// [`DeviceId`]), so it is trusted only as 0, and only where nothing else
    /// answers to the identity.
    ///
    /// - A CoreMIDI identity names one endpoint: `Exact` when it is present.
    /// - A Product Instance Id names a unit, and all its ports share it, so the
    ///   port is told by its name and direction: `Exact` for the one present
    ///   port with both, `Ambiguous` (the unit's ports) when none or several
    ///   have.
    /// - An ALSA identity is the client and port name: `SameName` for the lone
    ///   present device when the saved ordinal is 0 and it still goes the
    ///   saved way (it cannot be told from another unit of the same name),
    ///   `Ambiguous` for a saved ordinal above 0, several present devices, or
    ///   one that no longer goes the saved way.
    /// - No present device with the identity is `Missing`.
    #[must_use]
    pub fn resolve(&self, saved: &DeviceInfo) -> Resolved {
        let identity = saved.id.identity();
        let alike: Vec<&DeviceInfo> = self
            .devices
            .range(DeviceId::new(identity.clone(), 0)..)
            .take_while(|(id, _)| id.identity() == identity)
            .map(|(_, entry)| &entry.info)
            .collect();
        let owned = |infos: &[&DeviceInfo]| infos.iter().copied().cloned().collect::<Vec<_>>();
        match (identity, alike.as_slice()) {
            (_, []) => Resolved::Missing,
            (Identity::CoreMidi(_), [only]) => Resolved::Exact((*only).clone()),
            (Identity::Product(_), _) => {
                let port: Vec<&DeviceInfo> = alike
                    .iter()
                    .copied()
                    .filter(|d| d.name == saved.name && d.direction == saved.direction)
                    .collect();
                match port.as_slice() {
                    [only] => Resolved::Exact((*only).clone()),
                    _ => Resolved::Ambiguous(owned(if port.is_empty() { &alike } else { &port })),
                }
            }
            (Identity::Alsa { .. }, [only])
                if saved.id.ordinal() == 0 && serves(only.direction, saved.direction) =>
            {
                Resolved::SameName((*only).clone())
            }
            _ => Resolved::Ambiguous(owned(&alike)),
        }
    }

    /// The OS handle to open `id` by, if it is present.
    #[must_use]
    pub fn handle(&self, id: &DeviceId) -> Option<EndpointId> {
        self.devices.get(id).map(|e| e.handle)
    }
}

#[cfg(test)]
mod tests;
