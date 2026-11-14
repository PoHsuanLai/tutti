//! What a backend's thread keeps between looks at the OS, and the device list
//! it builds from an enumeration.

use std::collections::{BTreeMap, BTreeSet};

use tutti_midi_device::{EndpointId, Identity, OsChange, OsDevice, PortDirection};

use super::{ChangeSink, SeedError};

/// What a backend's thread keeps between looks at the OS: the handles it has
/// reported, so a handle that has dropped out of a fresh enumeration is
/// reported gone.
///
/// An OS reuses a handle: ALSA hands a new client the number of one that just
/// exited, so two looks that straddle both see one handle. A handle that now
/// names another identity is a different device, and so is one the OS says
/// departed (`departed`): each is reported gone before it is reported present
/// again. A report is applied under one hold of the state. Where the identity is
/// the name (ALSA), a rename is therefore a removal and an addition, and closes a port held on the device; where it
/// survives a rename (CoreMIDI's unique id), the device is `Changed`.
#[derive(Debug)]
pub(crate) struct Reporter {
    sink: ChangeSink,
    known: BTreeMap<EndpointId, Identity>,
}

impl Reporter {
    pub(crate) fn new(sink: ChangeSink) -> Self {
        Self {
            sink,
            known: BTreeMap::new(),
        }
    }

    /// Lists `present` as the devices there already were.
    pub(crate) fn seed(&mut self, present: Vec<OsDevice>) -> std::result::Result<(), SeedError> {
        let known = present
            .iter()
            .map(|d| (d.handle, d.identity.clone()))
            .collect();
        self.sink.seed(present)?;
        self.known = known;
        Ok(())
    }

    /// The handles of `present` that are known under another identity: an OS
    /// that reuses a handle for another device (ALSA does) shows up here.
    pub(crate) fn changed_in(&self, present: &[OsDevice]) -> BTreeSet<EndpointId> {
        present
            .iter()
            .filter(|d| {
                self.known
                    .get(&d.handle)
                    .is_some_and(|known| *known != d.identity)
            })
            .map(|d| d.handle)
            .collect()
    }

    /// Reports `present` as the whole device list: every device in it, and the
    /// departure of each known one that is not, that has another identity now,
    /// or that `departed` names. The table drops what repeats.
    pub(crate) fn report(&mut self, present: Vec<OsDevice>, departed: impl Fn(EndpointId) -> bool) {
        let now: BTreeMap<EndpointId, Identity> = present
            .iter()
            .map(|d| (d.handle, d.identity.clone()))
            .collect();
        let changed = self.changed_in(&present);
        let gone = self
            .known
            .keys()
            .filter(|handle| {
                !now.contains_key(handle) || changed.contains(handle) || departed(**handle)
            })
            .map(|handle| OsChange::Gone(*handle));
        let changes: Vec<_> = gone
            .chain(present.into_iter().map(OsChange::Present))
            .collect();
        self.sink.send_all(changes);
        self.known = now;
    }
}

/// The devices of an enumeration, one per handle: an endpoint listed both ways
/// is [`Duplex`](PortDirection::Duplex).
pub(crate) fn os_devices(
    inputs: &[crate::capability::EndpointInfo],
    outputs: &[crate::capability::EndpointInfo],
) -> Vec<OsDevice> {
    let mut devices: BTreeMap<EndpointId, OsDevice> = BTreeMap::new();
    let as_device = |e: &crate::capability::EndpointInfo, direction| OsDevice {
        handle: e.id,
        identity: e.identity.clone(),
        name: e.name.clone(),
        direction,
    };
    for e in inputs {
        devices.insert(e.id, as_device(e, PortDirection::Input));
    }
    for e in outputs {
        devices
            .entry(e.id)
            .and_modify(|d| d.direction = PortDirection::Duplex)
            .or_insert_with(|| as_device(e, PortDirection::Output));
    }
    devices.into_values().collect()
}
