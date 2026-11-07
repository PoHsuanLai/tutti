use tutti_ump::{MidiChannel, NoteNumber};

/// What is holding a voice open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    /// The key is down.
    Key,
    /// A pedal holds the note after its key came up.
    Pedal,
    /// Released: the envelope is running out.
    Releasing,
}

/// One voice's gate: the decision-level view two instruments are compared in
/// (not audio). Diagnostic; building the list allocates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Sounding {
    /// The voice's id, in the order voices were started.
    pub seq: u32,
    /// The channel the note arrived on.
    pub channel: MidiChannel,
    /// The key.
    pub key: NoteNumber,
    /// What holds it.
    pub phase: Phase,
}
