//! The plugin's state blob, read and patched. The layout is the one
//! documented in `tutti-shared-plugin`'s `core` module.
//!
//! The plugin is configured and observed through its state, the one channel
//! every format and every process boundary has: nothing here depends on an
//! environment variable or a symbol of the plugin image.

use tutti_plugin_types::Control;

use crate::spec::Mode;

const MAGIC: [u8; 4] = *b"TSP1";

/// What kind of event the host sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A parameter value.
    Param,
    /// A note on.
    NoteOn,
    /// A note off.
    NoteOff,
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

/// Where the plugin's blob starts inside whatever the host saved: a host may
/// wrap the plugin's bytes in a container of its own.
fn blob_at(bytes: &[u8]) -> usize {
    bytes
        .windows(MAGIC.len())
        .position(|w| w == MAGIC)
        .expect("the state holds the plugin's blob")
}

impl Transcript {
    /// Parses a saved state.
    ///
    /// # Panics
    ///
    /// When the state holds no complete blob of the plugin's.
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Self {
        let mut c = Cursor(&bytes[blob_at(bytes)..]);
        c.take::<4>();
        let mode = c.u32();
        c.take::<16>(); // level, aux
        c.take::<16>(); // process calls, frames
        let prepares = c.u32();
        let rate = c.f64();
        let max_block = c.u32();
        let blocks = (0..c.u32()).map(|_| c.u32()).collect();
        let events = (0..c.u32())
            .map(|_| {
                let head = c.u64();
                let kind = match head >> 32 {
                    1 => Kind::Param,
                    2 => Kind::NoteOn,
                    3 => Kind::NoteOff,
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
            blocks,
            prepares,
            rate,
            max_block,
            events,
        }
    }

    /// The events of one kind, in arrival order.
    pub fn of(&self, kind: Kind) -> impl Iterator<Item = &Logged> {
        self.events.iter().filter(move |e| e.kind == kind)
    }
}

/// The transcript of what `c`'s plugin has been sent.
pub fn transcript<C: Control + ?Sized>(c: &mut C) -> Transcript {
    Transcript::parse(&c.save_state().expect("the plugin saves"))
}

/// Selects the plugin's mode by saving its state, patching the mode word in
/// place and loading it back. A mode parameter would not do: a write to a VST3
/// separate controller reaches the component only inside a block, after setup
/// has read the mode.
pub fn set_mode<C: Control + ?Sized>(c: &mut C, mode: Mode) {
    let mut state = c.save_state().expect("the plugin saves");
    patch_mode(&mut state, mode);
    c.load_state(&state)
        .expect("the plugin takes its configuration");
}

/// Sets the mode word of the plugin's blob inside `state`, in place.
///
/// # Panics
///
/// When `state` holds no blob of the plugin's.
pub fn patch_mode(state: &mut [u8], mode: Mode) {
    let at = blob_at(state) + MAGIC.len();
    state[at..at + 4].copy_from_slice(&mode.index().to_le_bytes());
}
