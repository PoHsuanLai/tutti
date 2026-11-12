//! What the session's MIDI reports from [`Session::update`](crate::Session::update):
//! [`MidiEvent`], nested under [`SessionEvent::Midi`](crate::SessionEvent::Midi)
//! (design 030, convention 1).

/// A MIDI event of the session, one per thing that happened.
///
/// A variant about one device carries its `DeviceId`
/// as a field; a count over every input or the output has no device, and is its
/// own variant. Reported in the order they were seen, from `Phase::Poll` and
/// `Phase::MidiOut`.
#[derive(Debug)]
#[non_exhaustive]
pub enum MidiEvent {
    /// A MIDI device was added to the list, in the order the OS reported
    /// (design 017 §5). The session reads the devices in `Phase::Poll`; the
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
    /// that said so. What the device had queued is discarded; notes it held
    /// are not released (design 017 §8.3 is not built).
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
    /// negotiation (design 017 §5, M2-104 §7.1.6.1): the Endpoint it declared,
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
