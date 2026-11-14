//! The UTF-8 strings a command carries outside the endpoint names:
//! [`Text`] (a Bye or NAK message) and [`Username`].

use std::fmt;

/// Why a text is not a [`BoundedText`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum TextError {
    /// The text is shorter than `min` bytes.
    #[error("the text is shorter than {min} bytes")]
    TooShort {
        /// The fewest bytes the type holds.
        min: usize,
    },
    /// The text is longer than `max` bytes.
    #[error("the text is longer than {max} bytes")]
    TooLong {
        /// The most bytes the type holds.
        max: usize,
    },
    /// The text holds a NUL, which pads a string on the wire.
    #[error("the text holds a NUL, which ends a string on the wire")]
    Nul,
}

/// A UTF-8 string of `MIN` to `MAX` bytes with no NUL.
///
/// A string ends at the first NUL or at the end of the command (M2-124
/// section 5.3), so a NUL cannot be part of one. The bounds are the most a
/// command's payload length field can describe.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct BoundedText<const MIN: usize, const MAX: usize>(String);

/// A Bye or NAK message: up to 1016 bytes, the most a NAK carries.
pub type Text = BoundedText<0, 1016>;

/// A user name for an Invitation with User Authentication: 1 to 988 bytes, the
/// payload that is left beside the 32-byte digest.
pub type Username = BoundedText<1, 988>;

impl<const MIN: usize, const MAX: usize> BoundedText<MIN, MAX> {
    /// The string, or why `text` is not one.
    ///
    /// # Errors
    ///
    /// [`TextError::Nul`] for a NUL, [`TextError::TooShort`] under `MIN`
    /// bytes, [`TextError::TooLong`] over `MAX` bytes.
    pub fn new(text: &str) -> Result<Self, TextError> {
        if text.contains('\0') {
            return Err(TextError::Nul);
        }
        if text.len() < MIN {
            return Err(TextError::TooShort { min: MIN });
        }
        if text.len() > MAX {
            return Err(TextError::TooLong { max: MAX });
        }
        Ok(Self(text.to_owned()))
    }

    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<const MIN: usize, const MAX: usize> fmt::Debug for BoundedText<MIN, MAX> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl<const MIN: usize, const MAX: usize> fmt::Display for BoundedText<MIN, MAX> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<const MIN: usize, const MAX: usize> TryFrom<&str> for BoundedText<MIN, MAX> {
    type Error = TextError;

    fn try_from(text: &str) -> Result<Self, TextError> {
        Self::new(text)
    }
}

impl<const MAX: usize> Default for BoundedText<0, MAX> {
    fn default() -> Self {
        Self(String::new())
    }
}
