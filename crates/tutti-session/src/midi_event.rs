//! What the session's MIDI reports from [`Session::update`](crate::Session::update):
//! [`MidiEvent`], nested under [`SessionEvent::Midi`](crate::SessionEvent::Midi).

/// A MIDI event of the session, one per thing that happened.
///
/// A variant about one device carries its `DeviceId`
/// as a field; a count over every input or the output has no device, and is its
/// own variant. Reported in the order they were seen, from `Phase::Poll` and
/// `Phase::MidiOut`.
#[derive(Debug)]
#[non_exhaustive]
pub enum MidiEvent {
    /// A MIDI device was added to the list, in the order the OS reported.
    /// The session reads the devices in `Phase::Poll`; the
    /// list at any time is [`Midi::devices`](crate::Midi::devices).
    #[cfg(feature = "midi-hardware")]
    Added {
        /// What is listed of it; `info.id` is the device.
        info: tutti_midi_hardware::DeviceInfo,
    },
    /// A MIDI device left the list; `info` is its last.
    #[cfg(feature = "midi-hardware")]
    Removed {
        /// Its last listing; `info.id` is the device.
        info: tutti_midi_hardware::DeviceInfo,
    },
    /// A listed device's name or direction changed; its id did not.
    #[cfg(feature = "midi-hardware")]
    Changed {
        /// Its listing now; `info.id` is the device.
        info: tutti_midi_hardware::DeviceInfo,
    },
    /// A device the session held open as an input vanished: its connection
    /// is closed and its port retired, after the [`Removed`](Self::Removed)
    /// that said so. What the device had queued is discarded; the notes,
    /// pedals and bends it held are released downstream by the ingest node.
    #[cfg(feature = "midi-hardware")]
    InputLost {
        /// Its last listing; `info.id` is the device.
        info: tutti_midi_hardware::DeviceInfo,
    },
    /// The device the session held open as its output vanished: the output
    /// is closed, and what the hardware out sends is counted as dropped
    /// ([`OutputDropped`](Self::OutputDropped)) until a host connects another.
    #[cfg(feature = "midi-hardware")]
    OutputLost {
        /// Its last listing; `info.id` is the device.
        info: tutti_midi_hardware::DeviceInfo,
    },
    /// A device the session holds open as an input has settled its UMP Stream
    /// negotiation (M2-104 §7.1.6.1): the Endpoint it declared,
    /// or the OS's description of it where it did not answer, and the Protocol
    /// in use. Reported once per connection, from `Phase::Poll`; the state
    /// stays readable as
    /// [`MidiSession::stream`](tutti_midi_hardware::MidiSession::stream).
    #[cfg(feature = "midi-hardware")]
    StreamSettled {
        /// The device.
        device: tutti_midi_hardware::DeviceId,
        /// What is known of it (boxed: an Endpoint is large).
        stream: Box<tutti_midi_hardware::DeviceStream>,
    },
    /// A MIDI-CI peer of a device the session holds open as an input was
    /// found, changed or lost (M2-101): the session runs a
    /// MIDI-CI Device (Management only) on the input for as long as it is open
    /// and reports each change once, from `Phase::Poll`. The peers now are
    /// [`Midi::ci_peers`](crate::Midi::ci_peers).
    #[cfg(feature = "midi-hardware")]
    CiPeer {
        /// The device.
        device: tutti_midi_hardware::DeviceId,
        /// What happened to the peer.
        change: tutti_midi_hardware::CiChange,
    },
    /// A MIDI-CI peer's Profiles became known or changed, a request to switch one
    /// ended, or the peer's Profiles were dropped with it (M2-101 section 7): the
    /// session is the Initiator with every peer that declares Profile
    /// Configuration, and reports each change once, from `Phase::Poll`. The
    /// Profiles now are [`Midi::profiles`](crate::Midi::profiles).
    #[cfg(feature = "midi-hardware")]
    CiProfiles {
        /// The device.
        device: tutti_midi_hardware::DeviceId,
        /// What happened.
        change: tutti_midi_hardware::ProfilesChange,
    },
    /// A MIDI-CI peer answered Process Inquiry, a MIDI Message Report it was asked
    /// for began, ended, was refused or timed out, or the peer's Process Inquiry was
    /// dropped with it (M2-101 section 9): the session is the Initiator with every
    /// peer that declares Process Inquiry, and reports each change once, from
    /// `Phase::Poll`. The messages of a report are not events of this kind: they
    /// reach the graph as the input's MIDI, between the began and the ended. The
    /// peers that can be asked are
    /// [`Midi::report_peers`](crate::Midi::report_peers).
    #[cfg(feature = "midi-hardware")]
    CiProcess {
        /// The device.
        device: tutti_midi_hardware::DeviceId,
        /// What happened.
        change: tutti_midi_hardware::ProcessChange,
    },
    /// The hardware devices' event inlets dropped events because they were
    /// full: the total over every input so far, reported when it rises, at
    /// most once per [`MIDI_INPUT_DROP_REPORT`](crate::MIDI_INPUT_DROP_REPORT)
    /// of [`PhaseCx::now`](crate::PhaseCx::now). An inlet's ring is emptied
    /// each block; it fills when blocks stop coming (a stalled callback) or
    /// the device sends faster than they empty it.
    #[cfg(feature = "midi-hardware")]
    InputsDropped {
        /// Events dropped so far, over every input.
        total: u64,
    },
    /// The hardware out dropped events for want of an output: the total so
    /// far, reported when it rises, at most once per
    /// [`MIDI_OUT_DROP_REPORT`](crate::MIDI_OUT_DROP_REPORT) of
    /// [`PhaseCx::now`](crate::PhaseCx::now).
    OutputDropped {
        /// Events dropped so far.
        total: u64,
    },
    /// A [`Midi::load_file`](crate::Midi::load_file) ended: the file, or why
    /// not.
    FileLoaded {
        /// The id `load_file` returned.
        load: crate::MidiLoad,
        /// The file read, or the read's error ([`MidiError::File`](crate::MidiError::File))
        /// or the job's ([`MidiError::Job`](crate::MidiError::Job)).
        file: Result<tutti_midi_file::MidiFile, crate::MidiError>,
    },
    /// A `Midi::load_soundfont` ended: the unit, or why not.
    #[cfg(feature = "soundfont")]
    SoundFontLoaded {
        /// The id `load_soundfont` returned.
        load: crate::MidiLoad,
        /// The unit built (boxed: a unit is large), or the font's or the unit's error, or the job's.
        unit: Result<Box<tutti_soundfont::SoundFontUnit>, crate::MidiError>,
    },
    /// A tempo the session follows ([`Midi::follow_tempo`](crate::Midi::follow_tempo))
    /// was published: a Set Tempo of the followed Group, as a step in the
    /// project's tempo map at the start of the next block. A source that
    /// repeats the tempo already in force reports nothing.
    TempoFollowed {
        /// The tempo.
        tempo: tutti_types::Bpm,
        /// The beat the step was placed at, for a host to draw it. A publish
        /// that lands late takes effect from the block that picks it up, so
        /// the change is heard after this beat.
        beat: tutti_types::Beat,
    },
    /// A meter the session follows was published: a Set Time Signature of the
    /// followed Group, from the next bar line at or after the beat the transport
    /// was at.
    MeterFollowed {
        /// The time signature.
        signature: tutti_types::TimeSignature,
        /// The bar line it takes effect on.
        beat: tutti_types::Beat,
    },
    /// The session stopped following ([`Midi::unfollow_tempo`](crate::Midi::unfollow_tempo)):
    /// the project's own tempo and meter are published again, from the next
    /// block, at the beat the transport is then at.
    TempoUnfollowed,
    /// The session locked to the MIDI clock it follows
    /// ([`Midi::follow_clock`](crate::Midi::follow_clock)): Beat Clock after a
    /// quarter note of steady ticks, MTC after three consistent cycles of
    /// Quarter Frames.
    ClockLocked {
        /// The clock followed.
        kind: tutti_midi_runtime::ClockKind,
    },
    /// The session lost the MIDI clock it follows: a gap in Beat Clock's ticks,
    /// or silence past the dropout (half a second of Beat Clock, a quarter of a
    /// second of MTC), which also stops a transport the source had started.
    ClockUnlocked {
        /// The clock followed.
        kind: tutti_midi_runtime::ClockKind,
    },
    /// The session stopped following its MIDI clock
    /// ([`Midi::unfollow_clock`](crate::Midi::unfollow_clock)); the transport
    /// keeps the motion it had, and the project's tempo and meter are published
    /// again if the follower had changed them.
    ClockUnfollowed,
    /// The transport refused commands a followed MIDI clock or Machine Control asked of it, this
    /// many in one update: its list of timed commands was full. Nothing was
    /// applied for them (the source says its position again with its next
    /// message), and each is logged.
    ClockCommandsRefused(u64),
    /// A Machine Control command addressed to the session that it does not act
    /// on ([`Midi::follow_machine_control`](crate::Midi::follow_machine_control)):
    /// record, shuttle, eject, a READ, a speed other than play speed and the
    /// rest the transport has no counterpart for.
    MachineControlIgnored(tutti_midi_protocol::MmcCommand),
    /// The session stopped following MIDI Machine Control
    /// ([`Midi::unfollow_machine_control`](crate::Midi::unfollow_machine_control)).
    MachineControlUnfollowed,
    /// The OS watch for MIDI devices ended on its own (the reason is the
    /// message): the device list is frozen at what it last saw and no further
    /// hot-plug is reported. Reported once.
    #[cfg(feature = "midi-hardware")]
    WatchFailed(tutti_midi_hardware::WatchFailure),
}

#[cfg(feature = "midi-hardware")]
impl MidiEvent {
    /// The event a change of the device list is.
    pub(crate) fn of_device(event: tutti_midi_hardware::DeviceEvent) -> Self {
        use tutti_midi_hardware::DeviceEvent;
        match event {
            DeviceEvent::Added(info) => Self::Added { info },
            DeviceEvent::Removed(info) => Self::Removed { info },
            DeviceEvent::Changed(info) => Self::Changed { info },
        }
    }
}

impl From<MidiEvent> for crate::SessionEvent {
    fn from(event: MidiEvent) -> Self {
        Self::Midi(event)
    }
}
