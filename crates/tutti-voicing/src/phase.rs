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
