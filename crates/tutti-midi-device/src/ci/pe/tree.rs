//! The Property Data of a JSON Resource as a value: [`PeJson`], and the reader
//! and writer of its text (M2-103 §6.1).

use super::json;
use super::write;

/// The deepest nesting of arrays and objects the reader accepts. Property Data
/// arrives from a peer, so the depth the reader recurses to is bounded.
pub const PE_JSON_MAX_DEPTH: usize = 32;

/// A finite non-integer number, as JSON writes one: no NaN, no infinity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PeDecimal(f64);

impl PeDecimal {
    /// The number, or `None` for NaN and infinity, which JSON cannot write.
    #[must_use]
    pub fn new(raw: f64) -> Option<Self> {
        raw.is_finite().then_some(Self(raw))
    }

    /// The number.
    #[must_use]
    pub fn get(self) -> f64 {
        self.0
    }
}

/// A JSON object: its properties in the order written, each name once. Two
/// objects are equal when they have the same properties, in whatever order.
#[derive(Clone, Debug, Default)]
pub struct PeObject(Vec<(String, PeJson)>);

impl PartialEq for PeObject {
    fn eq(&self, other: &Self) -> bool {
        self.0.len() == other.0.len()
            && self
                .0
                .iter()
                .all(|(name, value)| other.get(name) == Some(value))
    }
}

impl PeObject {
    /// An object with no property.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets a property, replacing the value of one with the same name.
    pub fn insert(&mut self, name: impl Into<String>, value: PeJson) {
        let name = name.into();
        match self.0.iter_mut().find(|(n, _)| *n == name) {
            Some((_, old)) => *old = value,
            None => self.0.push((name, value)),
        }
    }

    /// The value of the property `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&PeJson> {
        self.0.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    /// The value of the property `name`, to change in place.
    pub fn get_mut(&mut self, name: &str) -> Option<&mut PeJson> {
        self.0.iter_mut().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    /// Takes the property `name` out.
    pub fn remove(&mut self, name: &str) -> Option<PeJson> {
        let at = self.0.iter().position(|(n, _)| n == name)?;
        Some(self.0.remove(at).1)
    }

    /// The properties, in order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &PeJson)> {
        self.0.iter().map(|(n, v)| (n.as_str(), v))
    }

    /// How many properties there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The object of `members`, or `None` when a name is there twice.
    pub(super) fn from_members(members: Vec<(String, PeJson)>) -> Option<Self> {
        let mut object = Self::new();
        for (name, value) in members {
            if object.get(&name).is_some() {
                return None;
            }
            object.0.push((name, value));
        }
        Some(object)
    }
}

impl IntoIterator for PeObject {
    type Item = (String, PeJson);
    type IntoIter = std::vec::IntoIter<(String, PeJson)>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

/// A JSON value: the Property Data of a Resource or a part of it, and the value
/// of a Header Data property, which is a scalar: [`is_scalar`](PeJson::is_scalar)
/// (M2-103 §7.1.1).
#[derive(Clone, Debug, PartialEq)]
pub enum PeJson {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// A number written without a point or an exponent.
    Integer(i64),
    /// A number written with a point or an exponent.
    Decimal(PeDecimal),
    /// A string, unescaped.
    String(String),
    /// An array.
    Array(Vec<PeJson>),
    /// An object.
    Object(PeObject),
}

/// Text that is not the JSON of Property Data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "the Property Data is not JSON of the Property Exchange form: it stops being so at byte {offset}"
)]
pub struct PeJsonError {
    /// The offset of the byte where the text stopped being the grammar. A
    /// nesting deeper than [`PE_JSON_MAX_DEPTH`] and a name written twice in an
    /// object stop it at the end of the text.
    pub offset: usize,
}

impl PeJson {
    /// Reads the JSON text `bytes`: any whitespace between tokens, strings
    /// of 7-bit ASCII with `\u` escapes (§6.1.1), each name once in an object
    /// and a nesting of at most [`PE_JSON_MAX_DEPTH`].
    ///
    /// # Errors
    ///
    /// [`PeJsonError`] with the offset where the text stopped being the grammar.
    pub fn parse(bytes: &[u8]) -> Result<Self, PeJsonError> {
        json::document(bytes).map_err(|offset| PeJsonError { offset })
    }

    /// The compact text of the value: no whitespace, every character beyond
    /// printable ASCII escaped as UTF-16 (§6.1.1).
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = String::new();
        write::json(&mut out, self);
        out.into_bytes()
    }

    /// Whether this is a number, a boolean or a string: what a header property
    /// and a change of a Partial Set may be (M2-103 §7.1.1, §10).
    #[must_use]
    pub fn is_scalar(&self) -> bool {
        matches!(
            self,
            Self::Bool(_) | Self::Integer(_) | Self::Decimal(_) | Self::String(_)
        )
    }

    /// The string, if this is one.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(text) => Some(text),
            _ => None,
        }
    }

    /// The object, if this is one.
    #[must_use]
    pub fn as_object(&self) -> Option<&PeObject> {
        match self {
            Self::Object(object) => Some(object),
            _ => None,
        }
    }

    /// The array, if this is one.
    #[must_use]
    pub fn as_array(&self) -> Option<&[PeJson]> {
        match self {
            Self::Array(items) => Some(items),
            _ => None,
        }
    }
}

impl From<bool> for PeJson {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<i64> for PeJson {
    fn from(value: i64) -> Self {
        Self::Integer(value)
    }
}

impl From<&str> for PeJson {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}

impl From<String> for PeJson {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

#[cfg(test)]
mod tests;
