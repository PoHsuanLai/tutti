//! The OS MIDI devices through the session's [`Midi`] view (feature
//! `midi-hardware`): list them, open one as an input or the output, and bring
//! back a saved one. Each call forwards to the session's
//! [`MidiSession`](tutti_midi_hardware::MidiSession); the hot-plug and the
//! negotiation come back as [`MidiEvent`](crate::MidiEvent)s from
//! `Session::update`.

use tutti_midi_hardware::{DeviceId, DeviceInfo, DeviceQuery, Resolved};

use crate::MidiError;
use crate::midi::{Midi, UmpOut};

type Result<T> = std::result::Result<T, MidiError>;

/// What [`Midi::reconnect_saved`] found of a saved device, and did.
///
/// Only a device that is surely the saved one is connected unasked; the rest
/// are for the app to put to its user, then connect by the id of the one
/// chosen (`connect_input(&info.id)`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// The device was the saved one for certain
    /// ([`Resolved::Exact`]) and is connected, in the directions it was saved
    /// with.
    Connected(DeviceId),
    /// One present device answers to the saved name, and the identity is only a
    /// name (ALSA): it may be another unit of the same model. Nothing was
    /// connected; ask.
    SameName(DeviceInfo),
    /// Several present devices could be the saved one, or its ordinal cannot be
    /// trusted across a restart. Nothing was connected; these are the
    /// candidates, ordered by id.
    Ambiguous(Vec<DeviceInfo>),
    /// No present device is the saved one. Nothing was connected.
    Missing,
}

impl Midi<'_> {
    fn hardware(&self) -> Result<&tutti_midi_hardware::MidiSession> {
        self.io.ok_or(MidiError::NoHardware)
    }

    /// The MIDI devices present now, inputs and outputs, ordered by id. Empty on
    /// a session with no OS MIDI layer
    /// ([`SessionBuilder::midi_hardware`](crate::SessionBuilder::midi_hardware)).
    /// A device that came or went since is reported as a
    /// [`MidiEvent`](crate::MidiEvent) from `Session::update`.
    #[must_use]
    pub fn devices(&self) -> Vec<DeviceInfo> {
        self.io.map_or_else(Vec::new, |io| io.devices().list())
    }

    /// Opens an input device, by `&DeviceId` or by name (a case-insensitive
    /// substring: [`DeviceQuery`]). Connecting one that is open succeeds. Its
    /// events reach the hardware input node, which the session's routes
    /// ([`route`](Midi::route)) send on.
    ///
    /// # Errors
    ///
    /// [`MidiError::NoHardware`] on a session with no OS MIDI layer;
    /// [`MidiError::Hardware`] when no input has that name or id, the id is not
    /// an input, the device has no free slot, or the OS refuses it.
    pub fn connect_input(&mut self, device: impl Into<DeviceQuery>) -> Result<()> {
        Ok(self.hardware()?.connect_input(device)?)
    }

    /// Closes an input, by id or name. Whether one was open.
    pub fn disconnect_input(&mut self, device: impl Into<DeviceQuery>) -> bool {
        self.io.is_some_and(|io| io.disconnect_input(device))
    }

    /// Opens an output device, by id or name, closing the one open (a session
    /// sends to one destination). If the session's hardware out has no output
    /// yet ([`set_hardware_out`](Midi::set_hardware_out)), it is set to this
    /// device, stamping as the device's configuration says.
    ///
    /// # Errors
    ///
    /// As [`connect_input`](Self::connect_input), for an output.
    pub fn connect_output(&mut self, device: impl Into<DeviceQuery>) -> Result<()> {
        let io = self.hardware()?;
        io.connect_output(device)?;
        if self.state.out.is_none() {
            let rate = self.graph.prepared().sample_rate();
            let mut out = UmpOut::new(Box::new(io.clone()), rate);
            // A device that has not settled its negotiation sends no JR
            // Timestamps; the `MidiOut` phase follows what it settles.
            if let Some(config) = io.output() {
                out.set_stamping(config.config.rx_jr);
            }
            self.state.out = Some(out);
        }
        Ok(())
    }

    /// Closes the output. Whether one was open. What the hardware out sends is
    /// counted as dropped until another is connected.
    pub fn disconnect_output(&mut self) -> bool {
        self.io
            .is_some_and(tutti_midi_hardware::MidiSession::disconnect_output)
    }

    /// Brings back a device an app saved (a [`DeviceInfo`], kept with `serde`
    /// in the app's own file): looks for it among the devices present
    /// ([`Resolved`]) and connects it **only when it is surely the same one**
    /// ([`BindPolicy::ExactOnly`](tutti_midi_hardware::BindPolicy::ExactOnly)), as an input, an output or both as it was
    /// saved. A device that merely has the same name, or several that might be
    /// it, are returned for the app to ask about; binding one unasked can open
    /// another keyboard of the same model.
    ///
    /// # Errors
    ///
    /// [`MidiError::NoHardware`] on a session with no OS MIDI layer;
    /// [`MidiError::Hardware`] when the device was found and refused to open.
    pub fn reconnect_saved(&mut self, saved: &DeviceInfo) -> Result<Resolution> {
        let resolved = self.hardware()?.devices().resolve(saved);
        // The one arm that binds is `BindPolicy::ExactOnly`'s, spelled out so
        // the three that must ask cannot be bound here.
        let found = match resolved {
            Resolved::Exact(info) => info,
            Resolved::SameName(info) => return Ok(Resolution::SameName(info)),
            Resolved::Ambiguous(candidates) => return Ok(Resolution::Ambiguous(candidates)),
            Resolved::Missing => return Ok(Resolution::Missing),
        };
        if saved.direction.is_input() && found.direction.is_input() {
            self.connect_input(&found.id)?;
        }
        if saved.direction.is_output() && found.direction.is_output() {
            self.connect_output(&found.id)?;
        }
        Ok(Resolution::Connected(found.id))
    }
}
