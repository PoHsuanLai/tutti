//! The bounded text of a UMP Stream: Endpoint Name, Product Instance Id and
//! Function Block Name, held without allocating, split into packets and
//! reassembled from them.

use std::fmt;
use std::str::FromStr;

use tutti_ump::Form;
use unicode_normalization::{UnicodeNormalization, is_nfc};

/// Why a text is not a name ([`Name::parse`], [`ProductInstanceId`] and
/// [`MessageText`](crate::MessageText)'s `TryFrom<&str>`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NameError {
    /// The text is longer than `max` bytes (for a name, composed to NFC; for a
    /// [`MessageText`](crate::MessageText), escaped as it goes on the wire).
    TooLong {
        /// The most bytes the type holds.
        max: usize,
    },
    /// A name holds a NUL, which ends one on the wire.
    Nul,
    /// A name starts with a Byte Order Mark (M2-104 §1.8).
    Bom,
    /// A [`ProductInstanceId`] holds a byte outside ASCII `32..=126` (other
    /// than the NUL that [`Nul`](Self::Nul) names).
    NotPrintableAscii,
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong { max } => write!(f, "the text is longer than {max} bytes"),
            Self::Nul => f.write_str("the text holds a NUL, which ends a name on the wire"),
            Self::Bom => f.write_str("the text starts with a Byte Order Mark"),
            Self::NotPrintableAscii => {
                f.write_str("the text holds a byte outside printable ASCII (32 to 126)")
            }
        }
    }
}

impl std::error::Error for NameError {}

/// A UTF-8 name of at most `N` bytes with no NUL, held inline.
///
/// A NUL ends a name on the wire (M2-104 §7.1.4: unused bytes of the last
/// packet are `0x00`), so a name cannot contain one. [`EndpointName`] and
/// [`BlockName`] are the two sizes.
///
/// M2-104 §1.8, "Unicode in Message Fields": "MIDI messages only use normalized
/// UTF-8 encoded Unicode characters, following the NFC (Normalization Form C)
/// standard" and "Unicode text in MIDI messages shall not include a Byte Order
/// Mark." So a name is always held in NFC and never starts with U+FEFF:
/// [`new`](Self::new) composes the text it is given and refuses a BOM, and a
/// name a peer sent is read the same way (a BOM is refused, text that is not
/// NFC is composed: §1.8 states a rule for what is sent and none for a
/// receiver, so a receiver that composes keeps the name it displays and compares
/// one thing, where one that refuses would lose a name a sender spelled
/// loosely). The length bound is on the composed bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Name<const N: usize> {
    bytes: [u8; N],
    len: u8,
}

/// An Endpoint Name: at most 98 bytes, a Start, five Continue and an End
/// packet of 14 bytes (M2-104 §7.1.4).
pub type EndpointName = Name<98>;

/// A Function Block Name: at most 91 bytes.
///
/// M2-104 §7.1.9: "The name shall not be any longer than 91 bytes in size,
/// either in a Complete message (if less than 14 bytes) ... or comprised of a
/// Start, up to five optional Continue messages and an End message." A Function
/// Block Name packet carries 13 bytes, and 7 x 13 = 91, one less than an
/// Endpoint Name's 98 (7 x 14).
pub type BlockName = Name<91>;

impl<const N: usize> Name<N> {
    /// The name, composed to NFC, or `None` when the composed text is longer
    /// than `N` bytes, holds a NUL or starts with a Byte Order Mark (§1.8).
    /// [`parse`](Self::parse) says which.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        Self::parse(text).ok()
    }

    /// The name, composed to NFC, or why the text is not one.
    ///
    /// # Errors
    ///
    /// [`NameError::Bom`] for a leading Byte Order Mark, [`NameError::Nul`] for
    /// a NUL, [`NameError::TooLong`] when the composed text is longer than `N`
    /// bytes.
    pub fn parse(text: &str) -> Result<Self, NameError> {
        if text.starts_with('\u{FEFF}') {
            return Err(NameError::Bom);
        }
        // Composed only when it is not NFC already, so the common name does not
        // allocate.
        let composed;
        let bytes = if is_nfc(text) {
            text.as_bytes()
        } else {
            composed = text.nfc().collect::<String>();
            composed.as_bytes()
        };
        if bytes.contains(&0) {
            return Err(NameError::Nul);
        }
        let too_long = NameError::TooLong { max: N };
        if bytes.len() > N {
            return Err(too_long);
        }
        let mut held = [0; N];
        held[..bytes.len()].copy_from_slice(bytes);
        Ok(Self {
            bytes: held,
            len: u8::try_from(bytes.len()).map_err(|_| too_long)?,
        })
    }

    /// A name read off the wire: UTF-8 bytes, read as [`parse`](Self::parse)
    /// reads a text.
    pub(crate) fn from_bytes(bytes: &[u8]) -> Option<Self> {
        Self::parse(std::str::from_utf8(bytes).ok()?).ok()
    }

    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // Checked UTF-8 in `from_bytes`, the only constructor.
        std::str::from_utf8(self.as_bytes()).unwrap_or("")
    }

    /// The text's bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    /// Whether the name is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<const N: usize> fmt::Debug for Name<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl<const N: usize> fmt::Display for Name<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<const N: usize> AsRef<str> for Name<N> {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl<const N: usize> TryFrom<&str> for Name<N> {
    type Error = NameError;

    fn try_from(text: &str) -> Result<Self, NameError> {
        Self::parse(text)
    }
}

impl<const N: usize> FromStr for Name<N> {
    type Err = NameError;

    fn from_str(text: &str) -> Result<Self, NameError> {
        Self::parse(text)
    }
}

/// A Product Instance Id: ASCII `32..=126`, at most 42 bytes, a Start, one
/// Continue and an End packet of 14 bytes (M2-104 §7.1.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProductInstanceId(Name<42>);

impl ProductInstanceId {
    /// The Id, or `None` when it is longer than 42 bytes or holds a byte
    /// outside `32..=126`. [`parse`](Self::parse) says which.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        Self::parse(text).ok()
    }

    /// The Id, or why the text is not one.
    ///
    /// # Errors
    ///
    /// [`NameError::Nul`] for a NUL, [`NameError::NotPrintableAscii`] for any
    /// other byte outside `32..=126`, [`NameError::TooLong`] past 42 bytes.
    pub fn parse(text: &str) -> Result<Self, NameError> {
        if text.bytes().any(|b| b == 0) {
            return Err(NameError::Nul);
        }
        if !text.bytes().all(|b| (32..=126).contains(&b)) {
            return Err(NameError::NotPrintableAscii);
        }
        Name::parse(text).map(Self)
    }

    pub(crate) fn from_bytes(bytes: &[u8]) -> Option<Self> {
        Self::parse(std::str::from_utf8(bytes).ok()?).ok()
    }

    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for ProductInstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for ProductInstanceId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl TryFrom<&str> for ProductInstanceId {
    type Error = NameError;

    fn try_from(text: &str) -> Result<Self, NameError> {
        Self::parse(text)
    }
}

impl FromStr for ProductInstanceId {
    type Err = NameError;

    fn from_str(text: &str) -> Result<Self, NameError> {
        Self::parse(text)
    }
}

/// Cuts `bytes` into packets of `W` bytes and hands each, with its form, to
/// `emit`: one `Complete` packet when it fits, else `Start`, `Continue`s and
/// `End`, the last padded with `0x00`. An empty text sends nothing.
pub(crate) fn packets<const W: usize>(bytes: &[u8], mut emit: impl FnMut(Form, [u8; W])) {
    let count = bytes.len().div_ceil(W);
    for (i, chunk) in bytes.chunks(W).enumerate() {
        let form = match (i, count) {
            (_, 1) => Form::Complete,
            (0, _) => Form::Start,
            (i, n) if i == n - 1 => Form::End,
            _ => Form::Continue,
        };
        let mut packet = [0; W];
        packet[..chunk.len()].copy_from_slice(chunk);
        emit(form, packet);
    }
}

/// Reassembles a text from its packets (M2-104 §7.1.4). It holds at most `N`
/// bytes: a longer message is dropped whole, and so is a `Continue` or `End`
/// with no `Start` before it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Assembler<const N: usize> {
    buf: [u8; N],
    len: usize,
    open: bool,
}

impl<const N: usize> Assembler<N> {
    pub(crate) const fn new() -> Self {
        Self {
            buf: [0; N],
            len: 0,
            open: false,
        }
    }

    /// Takes one packet's bytes; a message it finishes comes back, cut at the
    /// first `0x00`.
    pub(crate) fn feed(&mut self, form: Form, bytes: &[u8]) -> Option<&[u8]> {
        match form {
            Form::Complete | Form::Start => {
                self.len = 0;
                self.open = true;
            }
            Form::Continue | Form::End => {}
        }
        if !self.open {
            return None;
        }
        let end = self.len + bytes.len();
        if end > N {
            self.open = false;
            return None;
        }
        self.buf[self.len..end].copy_from_slice(bytes);
        self.len = end;
        if matches!(form, Form::Complete | Form::End) {
            self.open = false;
            let text = &self.buf[..self.len];
            let cut = text.iter().position(|&b| b == 0).unwrap_or(text.len());
            Some(&text[..cut])
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests;
