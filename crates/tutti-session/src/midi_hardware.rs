//! The OS MIDI devices through the session's [`Midi`] view (feature
//! `midi-hardware`): list them, open one as an input or the output, and bring
//! back a saved one. Each call forwards to the session's
//! [`MidiSession`](tutti_midi_hardware::MidiSession); the hot-plug and the
//! negotiation come back as [`MidiEvent`](crate::MidiEvent)s from
//! `Session::update`.

use std::sync::Arc;

use tutti_midi_hardware::{
    CiPeerInfo, DeviceId, DeviceInfo, DeviceQuery, Learned, Match, Resolved, SavedDevice,
};
use tutti_ump::DeviceIdentity;

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

    /// Turns MIDI-CI on, announcing `identity` in Discovery. **It is off by
    /// default**: a Discovery carries a System Exclusive manufacturer id, tutti
    /// has none, and MIDI 1.0's `7D` is "not to be used on any product released
    /// to the public", so the app gives the id it registered (an identity
    /// under `ManufacturerId::NON_COMMERCIAL` is for development and research
    /// only). It reaches the inputs already open (at the next update, after any
    /// Identity Request in progress) and later ones. Called again with the
    /// identity in use it changes nothing; with another, the running devices end
    /// as [`disable_ci`](Self::disable_ci) ends them and start again under a new
    /// MUID. Found peers are
    /// [`MidiEvent::CiPeer`](crate::MidiEvent::CiPeer) and
    /// [`ci_peers`](Self::ci_peers).
    ///
    /// # Errors
    ///
    /// [`MidiError::NoHardware`] on a session with no OS MIDI layer.
    pub fn enable_ci(&mut self, identity: DeviceIdentity) -> Result<()> {
        self.hardware()?.enable_ci(identity);
        Ok(())
    }

    /// Turns MIDI-CI off: every running device sends an Invalidate MUID for its
    /// own MUID at the next update (M2-101 §5.9: a shutdown is a reason), and
    /// its peers are reported lost.
    ///
    /// # Errors
    ///
    /// [`MidiError::NoHardware`] on a session with no OS MIDI layer.
    pub fn disable_ci(&mut self) -> Result<()> {
        self.hardware()?.disable_ci();
        Ok(())
    }

    /// The MIDI-CI peers found on the open input `id`, by Group then MUID: what
    /// a device that answered the session's Discovery declared (identity,
    /// categories, receivable SysEx size, Function Block). Empty when the
    /// input is not open, the device has no output to talk on, or nothing
    /// answered; a change is a [`MidiEvent::CiPeer`](crate::MidiEvent::CiPeer).
    /// The list is shared, not copied (a read clones a pointer), and stays as it
    /// was when read: read it again after a `CiPeer` event.
    #[must_use]
    pub fn ci_peers(&self, id: &DeviceId) -> Arc<[CiPeerInfo]> {
        self.io.map_or_else(Arc::default, |io| io.ci_peers(id))
    }

    /// Returns what the open input `id` has said about itself so far: its Device
    /// Identity and Product Instance Id (the UMP Endpoint's or the MIDI 1.0
    /// Identity Reply, then a MIDI-CI peer's for what those left). Empty when
    /// the input is not open, has not answered yet, or never will: the answers
    /// arrive over the first seconds after a connect, and a
    /// [`MidiEvent::StreamSettled`](crate::MidiEvent::StreamSettled) or
    /// [`CiPeer`](crate::MidiEvent::CiPeer) reports that something did. A
    /// device identity names a model and a Product Instance Id one unit
    /// (M2-101 §5.8.3.1).
    #[must_use]
    pub fn learned(&self, id: &DeviceId) -> Learned {
        self.io.map_or_else(Learned::new, |io| io.learned(id))
    }

    /// Returns the open input `id` as an app keeps it: the listing's row with what it
    /// has said ([`learned`](Self::learned)), for the app's own file (with the
    /// `serde` feature of `tutti-midi-device`). `None` when it is not open.
    /// Save it once the device has spoken; restore it with
    /// [`reconnect_saved`](Self::reconnect_saved) on
    /// [`SavedDevice::info`] and, once the device has spoken again,
    /// [`confirm`](Self::confirm).
    #[must_use]
    pub fn saved(&self, id: &DeviceId) -> Option<SavedDevice> {
        self.io.and_then(|io| io.saved(id))
    }

    /// Compares the open input `id` with the device `saved` describes, by what it
    /// has said: [`Match::SameUnit`] only on an equal Product Instance Id, never
    /// on a Device Identity (two units of a model share it);
    /// [`Match::Different`] when something known on both sides differs, which
    /// is the app's cue to unbind a device that
    /// [`reconnect_saved`](Self::reconnect_saved) opened by name. Run it again
    /// when [`learned`](Self::learned) changes: until then it is
    /// [`Match::Unknown`].
    #[must_use]
    pub fn confirm(&self, id: &DeviceId, saved: &SavedDevice) -> Match {
        saved.confirm(&self.learned(id))
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
