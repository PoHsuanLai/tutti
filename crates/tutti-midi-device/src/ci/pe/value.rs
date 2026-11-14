//! The value of a header property the spec does not name, and the bag that
//! keeps those properties.

use super::limits::PropertyName;
use super::tree::PeJson;

/// The property names the header types carry themselves.
const RESERVED: &[&str] = &[
    "resource",
    "resId",
    "mutualEncoding",
    "flowControl",
    "offset",
    "limit",
    "setPartial",
    "mediaType",
    "status",
    "message",
    "cacheTime",
    "totalCount",
    "subscribeId",
    "command",
    "endedBy",
];

/// A property that cannot be an extra property of a header.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PeExtraError {
    /// The name is one the header types carry as a field.
    #[error("{0:?} is a property of the header types, not an extra property")]
    Reserved(String),
    /// A header property is a number, a boolean or a string (M2-103 §7.1.1).
    #[error("a header property is a scalar, not an object, an array or null")]
    NotScalar,
}

/// The properties of a header that the spec does not name here: those of a later
/// version (M2-103 §7.6) and manufacturer-specific `x-` properties (§6.1.4), in
/// the order they were read or added. A name holds one value.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PeExtra(Vec<(PropertyName, PeJson)>);

impl PeExtra {
    /// Adds a property, replacing the value of one with the same name.
    ///
    /// # Errors
    ///
    /// [`PeExtraError::Reserved`] for a name the header types carry as a field,
    /// and [`PeExtraError::NotScalar`] for a value that is not a number, a
    /// boolean or a string.
    pub fn insert(&mut self, name: PropertyName, value: PeJson) -> Result<(), PeExtraError> {
        if RESERVED.contains(&name.as_str()) {
            return Err(PeExtraError::Reserved(name.into()));
        }
        if !value.is_scalar() {
            return Err(PeExtraError::NotScalar);
        }
        match self.0.iter_mut().find(|(n, _)| *n == name) {
            Some((_, old)) => *old = value,
            None => self.0.push((name, value)),
        }
        Ok(())
    }

    /// The value of the property `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&PeJson> {
        self.0
            .iter()
            .find(|(n, _)| n.as_str() == name)
            .map(|(_, v)| v)
    }

    /// The properties, in order.
    pub fn iter(&self) -> impl Iterator<Item = (&PropertyName, &PeJson)> {
        self.0.iter().map(|(n, v)| (n, v))
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}
