//! The OS MIDI devices through the session's [`Midi`] view (feature
//! `midi-hardware`): list them, open one as an input or the output, and bring
//! back a saved one. Each call forwards to the session's
//! [`MidiSession`](tutti_midi_hardware::MidiSession); the hot-plug and the
//! negotiation come back as [`MidiEvent`](crate::MidiEvent)s from
//! `Session::update`.

use std::sync::Arc;

use tutti_midi_hardware::{
    CiPeerInfo, DeviceId, DeviceInfo, DeviceQuery, Learned, Match, PeerProfiles, ProfileSwitch,
    ReportAsk, ReportPeer, Resolved, SavedDevice,
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
        self.io.as_deref().ok_or(MidiError::NoHardware)
    }

    fn hardware_mut(&mut self) -> Result<&mut tutti_midi_hardware::MidiSession> {
        self.io.as_deref_mut().ok_or(MidiError::NoHardware)
    }

    /// The MIDI devices present now, inputs and outputs, ordered by id. Empty on
    /// a session with no OS MIDI layer
    /// ([`SessionBuilder::midi_hardware`](crate::SessionBuilder::midi_hardware)).
    /// A device that came or went since is reported as a
    /// [`MidiEvent`](crate::MidiEvent) from `Session::update`.
    #[must_use]
    pub fn devices(&self) -> Vec<DeviceInfo> {
        self.io
            .as_deref()
            .map_or_else(Vec::new, |io| io.devices().list())
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
        Ok(self.hardware_mut()?.connect_input(device)?)
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
        self.hardware_mut()?.enable_ci(identity);
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
        self.hardware_mut()?.disable_ci();
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
        self.io
            .as_deref()
            .map_or_else(Arc::default, |io| io.ci_peers(id))
    }

    /// The Profiles the MIDI-CI peers found on the open input `id` support, by
    /// Group then MUID (M2-101 section 7): only a peer that declared Profile
    /// Configuration has an entry, and the session asks it for its Profiles when
    /// it is found. Empty when the input is not open or no peer did; a change is
    /// a [`MidiEvent::CiProfiles`](crate::MidiEvent::CiProfiles). Shared like
    /// [`ci_peers`](Self::ci_peers), and as it was when read.
    ///
    /// A peer that has the MPE Profile (M2-120) enabled puts that input's ingest
    /// on the Profile's rules in its zones: the Manager Channel of each is the
    /// Channel it is enabled on, and the Members the Channels the Enabled
    /// Report counted above it
    /// ([`PeerProfiles::mpe_zones`](tutti_midi_hardware::PeerProfiles::mpe_zones)).
    /// Disabling it, or the peer's going, puts the input back on
    /// [`Midi::mpe`](crate::Midi::mpe)'s.
    ///
    /// Each Profile is a [`ListedProfile`](tutti_midi_hardware::ListedProfile),
    /// the handle a [`ProfileSwitch`] is made from, so a request can only name a
    /// Profile at an address the peer listed.
    #[must_use]
    pub fn profiles(&self, id: &DeviceId) -> Arc<[PeerProfiles]> {
        self.io
            .as_deref()
            .map_or_else(Arc::default, |io| io.profiles(id))
    }

    /// Asks a peer to enable or disable a Profile it lists, on the input the
    /// switch was made from. The request goes out at the next update, and its end (the peer's
    /// Enabled or Disabled Report, its NAK, or a timeout with no answer) is a
    /// [`MidiEvent::CiProfiles`](crate::MidiEvent::CiProfiles) with
    /// [`ProfilesChange::Resolved`](tutti_midi_hardware::ProfilesChange::Resolved).
    ///
    /// # Errors
    ///
    /// [`MidiError::NoHardware`] on a session with no OS MIDI layer;
    /// [`MidiError::Profile`] when the input the switch was made from has closed, the peer is gone,
    /// or it no longer lists the Profile there.
    pub fn set_profile(&mut self, switch: ProfileSwitch) -> Result<()> {
        Ok(self.hardware_mut()?.set_profile(switch)?)
    }

    /// The MIDI-CI peers of the open input `id` that declared Process Inquiry and
    /// answered that they support MIDI Message Report, by Group then MUID (M2-101
    /// section 9): empty when the input is not open or no peer did. The session
    /// asks each such peer for its capabilities when it is found; a
    /// [`MidiEvent::CiProcess`](crate::MidiEvent::CiProcess) says one answered.
    /// Shared like [`ci_peers`](Self::ci_peers), and as it was when read.
    ///
    /// Each is a [`ReportPeer`], the handle a [`ReportAsk`] is made from, so a
    /// report can only be asked of a peer that supports it.
    #[must_use]
    pub fn report_peers(&self, id: &DeviceId) -> Arc<[ReportPeer]> {
        self.io
            .as_deref()
            .map_or_else(Arc::default, |io| io.report_peers(id))
    }

    /// Asks a peer for a MIDI Message Report, on the input the request was made
    /// from. The request goes out at the next update; the peer's Reply and End, its
    /// NAK, or a timeout are [`MidiEvent::CiProcess`](crate::MidiEvent::CiProcess)
    /// events, and the report's MIDI messages reach the graph between the first
    /// and the last as the input's MIDI.
    ///
    /// # Errors
    ///
    /// [`MidiError::NoHardware`] on a session with no OS MIDI layer;
    /// [`MidiError::Report`] when the input the request was made from has closed or
    /// the peer is gone.
    pub fn report(&mut self, ask: ReportAsk) -> Result<()> {
        Ok(self.hardware_mut()?.report(ask)?)
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
        self.io
            .as_deref()
            .map_or_else(Learned::new, |io| io.learned(id))
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
        self.io.as_deref().and_then(|io| io.saved(id))
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

    /// Closes an input, by id or name.
    ///
    /// # Errors
    ///
    /// [`MidiError::NoHardware`] on a session with no OS MIDI layer;
    /// [`MidiError::InputNotOpen`] when no open input answers to `device`.
    /// Nothing changes.
    #[must_use = "a device that was not open is an error"]
    pub fn disconnect_input(&mut self, device: impl Into<DeviceQuery>) -> Result<()> {
        let device = device.into();
        if self.hardware_mut()?.disconnect_input(device.clone()) {
            Ok(())
        } else {
            Err(MidiError::InputNotOpen(device))
        }
    }

    /// Opens an output device, by id or name, and makes it the hardware out's
    /// sink: the output is the hardware out's own, replacing the sink it had (a
    /// session sends to one destination, and the stamp stream carries on across
    /// the change). It stamps as the device's configuration says, which each
    /// update's poll follows.
    ///
    /// # Errors
    ///
    /// As [`connect_input`](Self::connect_input), for an output.
    pub fn connect_output(&mut self, device: impl Into<DeviceQuery>) -> Result<()> {
        let output = self.hardware()?.connect_output(device)?;
        match &mut self.state.out {
            Some(out) => out.set_device(output),
            None => {
                let rate = self.graph.prepared().sample_rate();
                self.state.out = Some(UmpOut::of_device(output, rate));
            }
        }
        Ok(())
    }

    /// The device output the hardware out sends into, if it is one: its id,
    /// name and the Protocol and JR Timestamps it sends in.
    #[must_use]
    pub fn output(&self) -> Option<&tutti_midi_hardware::OpenOutput> {
        self.state.out.as_ref().and_then(UmpOut::device)
    }

    /// Closes the device output: the hardware out has no sink, and what it
    /// sends is counted as dropped until another is connected
    /// ([`connect_output`](Self::connect_output) or
    /// [`set_hardware_out`](Midi::set_hardware_out)).
    ///
    /// # Errors
    ///
    /// [`MidiError::NoHardware`] on a session with no OS MIDI layer;
    /// [`MidiError::OutputNotOpen`] when the hardware out is not a device's.
    #[must_use = "an output that was not open is an error"]
    pub fn disconnect_output(&mut self) -> Result<()> {
        self.hardware()?;
        if self.output().is_some() {
            self.state.out = None;
            Ok(())
        } else {
            Err(MidiError::OutputNotOpen)
        }
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
