//! The UDP payload (M2-124 section 5): a signature and Command Packets.

use crate::{Command, Header, ParseError};

/// The word every UDP payload starts with: `MIDI` in ASCII.
pub const SIGNATURE: u32 = 0x4D49_4449;

/// The size past which a UDP payload risks fragmentation (M2-124 section
/// 5.1.1).
pub const MAX_DATAGRAM_BYTES: usize = 1400;

/// Why a UDP payload is not read at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum SignatureError {
    /// The payload is shorter than the signature.
    #[error("the payload is shorter than the 4-byte signature")]
    TooShort,
    /// The payload starts with another word, so the rest is ignored.
    #[error("the payload starts with {found:#010x}, not the signature")]
    Mismatch {
        /// The first word.
        found: u32,
    },
}

/// A command that is refused: its header, and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Refused {
    /// The header of the refused command (the words that were present, for a
    /// command cut short by the end of the datagram).
    pub header: Header,
    /// What is wrong with it.
    pub error: ParseError,
}

/// The Command Packets of one UDP payload, in order, after its signature is
/// checked ([`parse`]).
///
/// A command that is refused ends only itself, since its length is known; a
/// command cut short by the end of the payload ends the iteration.
#[derive(Clone, Debug)]
pub struct Commands<'a> {
    rest: &'a [u8],
}

/// Checks a UDP payload's signature and returns its commands.
///
/// # Errors
///
/// [`SignatureError`] when the payload does not start with the signature: the
/// whole payload is to be ignored.
pub fn parse(datagram: &[u8]) -> Result<Commands<'_>, SignatureError> {
    let (word, rest) = datagram
        .split_first_chunk::<4>()
        .ok_or(SignatureError::TooShort)?;
    let found = u32::from_be_bytes(*word);
    if found != SIGNATURE {
        return Err(SignatureError::Mismatch { found });
    }
    Ok(Commands { rest })
}

impl Iterator for Commands<'_> {
    type Item = Result<Command, Refused>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        let Some((word, after)) = self.rest.split_first_chunk::<4>() else {
            let partial = self.rest.iter().fold(0u32, |w, b| w << 8 | u32::from(*b));
            self.rest = &[];
            return Some(Err(Refused {
                header: Header::from_word(partial),
                error: ParseError::Truncated,
            }));
        };
        let header = Header::from_word(u32::from_be_bytes(*word));
        let Some((payload, after)) =
            after.split_at_checked(usize::from(header.payload_words()) * 4)
        else {
            self.rest = &[];
            return Some(Err(Refused {
                header,
                error: ParseError::Truncated,
            }));
        };
        self.rest = after;
        Some(Command::parse(header, payload).map_err(|error| Refused { header, error }))
    }
}

/// A UDP payload under construction: the signature, then commands.
#[derive(Clone, Debug)]
pub struct DatagramBuilder {
    bytes: Vec<u8>,
}

impl Default for DatagramBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl DatagramBuilder {
    /// A payload holding the signature alone.
    #[must_use]
    pub fn new() -> Self {
        Self {
            bytes: SIGNATURE.to_be_bytes().to_vec(),
        }
    }

    /// Appends a command.
    pub fn push(&mut self, command: &Command) {
        command.write(&mut self.bytes);
    }

    /// The payload's size so far, in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether no command has been appended.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.len() == 4
    }

    /// The payload.
    #[must_use]
    pub fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

/// Packs `commands` into UDP payloads of at most [`MAX_DATAGRAM_BYTES`], in
/// order: a command goes in the payload being filled, or in a new one when it
/// does not fit. A command larger than the limit has a payload to itself.
pub(crate) fn pack(commands: &[Command]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut current = DatagramBuilder::new();
    let mut scratch = Vec::new();
    for command in commands {
        scratch.clear();
        command.write(&mut scratch);
        if !current.is_empty() && current.len() + scratch.len() > MAX_DATAGRAM_BYTES {
            out.push(std::mem::take(&mut current).finish());
        }
        current.bytes.extend_from_slice(&scratch);
    }
    if !current.is_empty() {
        out.push(current.finish());
    }
    out
}
