//! The text types of a Header Data property, each holding the limit M2-103 sets
//! on it (§6.1, §7.1.1 to §7.5, §8.4, §8.5, §11.1).

use std::fmt;

use super::write::escaped_len;

/// A string that is not a valid value of the named type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("not a valid {what}")]
pub struct NotValid {
    /// The type the string was meant to be.
    pub what: &'static str,
}

/// A number outside the range of the type it was meant to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{raw} is not a valid {what}")]
pub struct NotInRange<R: fmt::Display + fmt::Debug> {
    /// The type the number was meant to be.
    pub what: &'static str,
    /// The number.
    pub raw: R,
}

/// Defines a string type that holds only text its validator accepts.
macro_rules! text_type {
    ($(#[$meta:meta])* $name:ident, $what:literal, $valid:expr) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash)]
        pub struct $name(String);

        impl $name {
            /// The value, or `None` when `text` breaks the limit.
            #[must_use]
            pub fn new(text: &str) -> Option<Self> {
                let valid: fn(&str) -> bool = $valid;
                valid(text).then(|| Self(text.to_owned()))
            }

            /// The text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<&str> for $name {
            type Error = NotValid;

            fn try_from(text: &str) -> Result<Self, NotValid> {
                Self::new(text).ok_or(NotValid { what: $what })
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> String {
                value.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

pub(super) use text_type;

fn alphanumeric(text: &str) -> bool {
    text.bytes().all(|b| b.is_ascii_alphanumeric())
}

text_type! {
    /// The name of a Resource: 1 to 36 ASCII alphanumeric characters (M2-103
    /// Table 13), or the `X-` prefix of a manufacturer-specific Resource and 1 to
    /// 33 alphanumeric characters (§8.4). The prefix is the one hyphen
    /// the name may hold: Table 13 lists alphanumerics only, and §8.4 requires a
    /// custom Resource to start `X-`, so the prefix is the reading that holds both.
    ResourceName, "resource name",
    |t| {
        let body = t.strip_prefix("X-").unwrap_or(t);
        t.len() <= 36 && !body.is_empty() && alphanumeric(body)
    }
}

text_type! {
    /// A Resource ID (§8.5): 1 to 36 ASCII alphanumeric or `_` characters
    /// (Table 13).
    ResourceId, "resource id",
    |t| {
        (1..=36).contains(&t.len())
            && t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    }
}

text_type! {
    /// A Subscription Id (§11.1, Table 39): 1 to 8 characters of `a-z`, `0-9` and `_`.
    SubscribeId, "subscription id",
    |t| {
        (1..=8).contains(&t.len())
            && t.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_'))
    }
}

text_type! {
    /// The name of a header property: 1 to 20 ASCII alphanumeric characters, not
    /// all digits (§6.1, §7.1.1), or the `x-` prefix of a manufacturer-specific
    /// property (§6.1.4) and alphanumerics.
    PropertyName, "property name",
    |t| {
        let body = t.strip_prefix("x-").unwrap_or(t);
        t.len() <= 20
            && !body.is_empty()
            && alphanumeric(body)
            && !body.bytes().all(|b| b.is_ascii_digit())
    }
}

text_type! {
    /// A Media Type, as `mediaType` carries it (§7.5, Table 16): 1 to 75 printable
    /// ASCII characters.
    MediaType, "media type",
    |t| (1..=75).contains(&t.len()) && t.bytes().all(|b| matches!(b, b' '..=b'~'))
}

/// The explanatory text of a Reply (§7.3, Table 14), held as the text it says:
/// at most 512 bytes once escaped, which is how it travels.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PeMessage(String);

impl PeMessage {
    /// The most escaped bytes a message may take, 512.
    pub const MAX_ESCAPED_LEN: usize = 512;

    /// The message, or `None` when its escaped form is longer than
    /// [`MAX_ESCAPED_LEN`](Self::MAX_ESCAPED_LEN) bytes.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        (escaped_len(text) <= Self::MAX_ESCAPED_LEN).then(|| Self(text.to_owned()))
    }

    /// The text, unescaped.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for PeMessage {
    type Error = NotValid;

    fn try_from(text: &str) -> Result<Self, NotValid> {
        Self::new(text).ok_or(NotValid { what: "message" })
    }
}

impl From<PeMessage> for String {
    fn from(message: PeMessage) -> String {
        message.0
    }
}

impl fmt::Display for PeMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
