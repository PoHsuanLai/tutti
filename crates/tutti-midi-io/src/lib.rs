pub mod error;
pub use error::{Error, Result};

mod midi_io;
pub use midi_io::{MidiDevice, MidiIo};
pub use io::MidiInputRecord;

// --- Re-exports from tutti-midi ---

pub use tutti_midi_types::note;
pub use tutti_midi_types::{hz_to_note, note_to_hz};
pub use tutti_midi_types::{decode, midi2, midly, MidiEvent, MidiTarget, Note, SemanticEvent};

#[cfg(feature = "mpe")]
pub use tutti_midi_types::mpe::{MpeMode, MpeZone, MpeZoneConfig};

// --- Port management (ring-buffer I/O between hardware and audio thread) ---

pub mod port;
pub use port::{InputProducerHandle, MidiPortManager, PortInfo, PortType};

pub use tutti_midi_types::cc::mapping::{CCMapping, CCNumber, CCTarget, MappingId, MidiChannel};

// --- Local modules ---

pub(crate) mod io;

#[cfg(all(target_os = "macos", feature = "virtual-midi"))]
pub use io::{VirtualMidiDestination, VirtualMidiSource};

pub(crate) mod file;
pub use file::{
    encode_midi_file, write_midi_file, MidiWriteOptions, ParsedMidiFile, SmfTimedEvent,
};

pub use tutti_midi_types::sync::{ClockTransportState, MidiClockDecoder, MtcDecoder, SmpteTimecode};

pub use crossbeam_channel;
