//! The value of a header property the spec does not name, and the bag that
//! keeps those properties.

use super::limits::PropertyName;

/// A finite non-integer number, as a header property carries one (M2-103 §7.1.1:
/// a property is a number, a boolean or a string).
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

/// The value of a header property: a number, a boolean or a string (M2-103
/// §7.1.1), never an object, an array or `null`.
#[derive(Clone, Debug, PartialEq)]
pub enum PeValue {
    /// `true` or `false`.
    Bool(bool),
    /// A number written without a point or an exponent.
    Integer(i64),
    /// A number written with a point or an exponent.
    Decimal(PeDecimal),
    /// A string, unescaped.
    String(String),
}

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

/// A property name that a header type carries as a field, so it is not an extra
/// property of that header.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is a property of the header types, not an extra property")]
pub struct PeReservedProperty(pub String);

/// The properties of a header that the spec does not name here: those of a later
/// version (M2-103 §7.6) and manufacturer-specific `x-` properties (§6.1.4), in
/// the order they were read or added. A name holds one value.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PeExtra(Vec<(PropertyName, PeValue)>);

impl PeExtra {
    /// Adds a property, replacing the value of one with the same name.
    ///
    /// # Errors
    ///
    /// [`PeReservedProperty`] for a name the header types carry as a field.
    pub fn insert(&mut self, name: PropertyName, value: PeValue) -> Result<(), PeReservedProperty> {
        if RESERVED.contains(&name.as_str()) {
            return Err(PeReservedProperty(name.into()));
        }
        match self.0.iter_mut().find(|(n, _)| *n == name) {
            Some((_, old)) => *old = value,
            None => self.0.push((name, value)),
        }
        Ok(())
    }

    /// The value of the property `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&PeValue> {
        self.0
            .iter()
            .find(|(n, _)| n.as_str() == name)
            .map(|(_, v)| v)
    }

    /// The properties, in order.
    pub fn iter(&self) -> impl Iterator<Item = (&PropertyName, &PeValue)> {
        self.0.iter().map(|(n, v)| (n, v))
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}
