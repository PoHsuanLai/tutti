//! The shared plugin's state blob, read: the format of its transcript, owned
//! here with the plugin that writes it (`core`, "State"), so every reader (the
//! host contract, the session's instrument contract) parses one definition.
//!
//! The plugin is configured and observed through its state, the one channel
//! every format and every process boundary has: nothing here depends on an
//! environment variable or a symbol of the plugin image.

/// The blob's first four bytes.
pub const MAGIC: [u8; 4] = *b"TSP1";

/// What kind of event the host sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A parameter value.
    Param,
    /// A note on.
    NoteOn,
    /// A note off.
    NoteOff,
    /// A control change: its controller number is the `id`, its value the
    /// `value`. Recorded, never acted on.
    Control,
}

/// An event as the plugin received it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Logged {
    /// What it was.
    pub kind: Kind,
    /// Its sample offset in the block.
    pub offset: u32,
    /// The parameter id or the key.
    pub id: u32,
    /// The parameter value (0 for a note).
    pub value: f64,
}

/// What the plugin recorded of what the host sent, read from its state.
#[derive(Clone, Debug, PartialEq)]
pub struct Transcript {
    /// The mode index.
    pub mode: u32,
    /// The `level` and `aux` parameters, as the plugin holds them.
    pub level: f64,
    /// See `level`.
    pub aux: f64,
    /// Blocks processed since the last activation.
    pub calls: u64,
    /// Frames processed since the last activation.
    pub frames: u64,
    /// Frame counts of the last blocks, oldest first.
    pub blocks: Vec<u32>,
    /// Activations.
    pub prepares: u32,
    /// The rate it was last activated at.
    pub rate: f64,
    /// The block size it was last activated with.
    pub max_block: u32,
    /// Events in arrival order.
    pub events: Vec<Logged>,
    /// The host sent more than the plugin keeps, so the lists above are
    /// incomplete.
    pub overflow: bool,
}

/// A reader over the blob.
struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    fn take<const N: usize>(&mut self) -> [u8; N] {
        let (head, rest) = self
            .0
            .split_first_chunk::<N>()
            .expect("the blob is complete");
        self.0 = rest;
        *head
    }
    fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.take())
    }
    fn u64(&mut self) -> u64 {
        u64::from_le_bytes(self.take())
    }
    fn f64(&mut self) -> f64 {
        f64::from_bits(self.u64())
    }
}

/// Where the plugin's bytes start inside a state the host saved verbatim.
fn blob_at(state: &[u8]) -> usize {
    state
        .windows(MAGIC.len())
        .position(|w| w == MAGIC)
        .expect("the state holds the plugin's blob")
}

/// The plugin's blob, found in a state the host stores verbatim: what
/// [`Format::extract_blob`] does by default.
#[must_use]
pub fn find_blob(state: &[u8]) -> Vec<u8> {
    state[blob_at(state)..].to_vec()
}

/// `state` with the plugin's blob replaced by `blob` (the same length, found
/// as [`find_blob`] does): the default of
/// [`Format::embed_blob`].
#[must_use]
pub fn replace_blob(state: &[u8], blob: &[u8]) -> Vec<u8> {
    let at = blob_at(state);
    let mut out = state.to_vec();
    out[at..at + blob.len()].copy_from_slice(blob);
    out
}

impl Transcript {
    /// Parses the plugin's blob (from its magic).
    ///
    /// # Panics
    ///
    /// When `blob` is not a complete blob of the plugin's.
    #[must_use]
    pub fn parse(blob: &[u8]) -> Self {
        let mut c = Cursor(blob);
        assert_eq!(
            c.take::<4>(),
            MAGIC,
            "the blob starts with the plugin's magic"
        );
        let mode = c.u32();
        let (level, aux) = (c.f64(), c.f64());
        let (calls, frames) = (c.u64(), c.u64());
        let prepares = c.u32();
        let rate = c.f64();
        let max_block = c.u32();
        let overflow = c.u32() != 0;
        let blocks = (0..c.u32()).map(|_| c.u32()).collect();
        let events = (0..c.u32())
            .map(|_| {
                let head = c.u64();
                let kind = match head >> 32 {
                    1 => Kind::Param,
                    2 => Kind::NoteOn,
                    3 => Kind::NoteOff,
                    4 => Kind::Control,
                    other => panic!("the transcript holds event kind {other}"),
                };
                let id = c.u64() as u32;
                Logged {
                    kind,
                    offset: head as u32,
                    id,
                    value: c.f64(),
                }
            })
            .collect();
        Self {
            mode,
            level,
            aux,
            calls,
            frames,
            blocks,
            prepares,
            rate,
            max_block,
            events,
            overflow,
        }
    }

    /// The transcript, refused when it is incomplete: a case that iterates a
    /// list the plugin truncated would otherwise pass on what is left.
    ///
    /// # Panics
    ///
    /// When the plugin's log overflowed.
    #[must_use]
    pub fn complete(self) -> Self {
        assert!(
            !self.overflow,
            "the plugin's log overflowed: the host sent more than it keeps"
        );
        self
    }

    /// The events of one kind, in arrival order.
    pub fn of(&self, kind: Kind) -> impl Iterator<Item = &Logged> {
        self.events.iter().filter(move |e| e.kind == kind)
    }
}
