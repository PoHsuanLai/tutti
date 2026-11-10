//! The bounded text of a UMP Stream: Endpoint Name, Product Instance Id and
//! Function Block Name, held without allocating, split into packets and
//! reassembled from them.

use std::fmt;

use tutti_ump::Form;

/// A UTF-8 name of at most `N` bytes with no NUL, held inline.
///
/// A NUL ends a name on the wire (M2-104 §7.1.4: unused bytes of the last
/// packet are `0x00`), so a name cannot contain one. [`EndpointName`] and
/// [`BlockName`] are the two sizes.
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
/// M2-104 §7.1.9 says 98, as for an Endpoint Name, but a Function Block Name
/// packet carries 13 bytes, and a Start, five Continue and an End packet hold
/// 7 x 13 = 91. 91 is what fits the message the spec describes.
pub type BlockName = Name<91>;

impl<const N: usize> Name<N> {
    /// The name, or `None` when it is longer than `N` bytes or holds a NUL.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        Self::from_bytes(text.as_bytes())
    }

    pub(crate) fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > N || bytes.contains(&0) {
            return None;
        }
        std::str::from_utf8(bytes).ok()?;
        let mut held = [0; N];
        held[..bytes.len()].copy_from_slice(bytes);
        Some(Self {
            bytes: held,
            len: u8::try_from(bytes.len()).ok()?,
        })
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

/// A Product Instance Id: ASCII `32..=126`, at most 42 bytes, a Start, one
/// Continue and an End packet of 14 bytes (M2-104 §7.1.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProductInstanceId(Name<42>);

impl ProductInstanceId {
    /// The Id, or `None` when it is longer than 42 bytes or holds a byte
    /// outside `32..=126`.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        Self::from_bytes(text.as_bytes())
    }

    pub(crate) fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if !bytes.iter().all(|b| (32..=126).contains(b)) {
            return None;
        }
        Name::from_bytes(bytes).map(Self)
    }

    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
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
