//! A Partial Set and a `partial` Subscription update (M2-103 §10, §11.1): a
//! change to a subset of a Resource's Property Data, as JSON Pointers (RFC 6901)
//! each with a new scalar value.

use std::fmt;

use super::tree::{PeJson, PeObject};

/// A JSON Pointer (RFC 6901): the empty text, or `/` and a reference token, any
/// number of times. In a token `~0` stands for `~` and `~1` for `/`, and a `~`
/// is followed by one of those two.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct JsonPointer(String);

impl JsonPointer {
    /// The pointer `text` spells, or `None` for text that is not one.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        let valid = text.is_empty()
            || (text.starts_with('/')
                && text.char_indices().all(|(at, c)| {
                    c != '~' || matches!(text.as_bytes().get(at + 1), Some(b'0' | b'1'))
                }));
        valid.then(|| Self(text.to_owned()))
    }

    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The reference tokens, unescaped, in order.
    pub fn tokens(&self) -> impl Iterator<Item = String> + '_ {
        self.0
            .split('/')
            .skip(1)
            .map(|token| token.replace("~1", "/").replace("~0", "~"))
    }
}

impl fmt::Display for JsonPointer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A change that cannot be made to the Property Data.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PartialError {
    /// The pointer names a place the Property Data does not have: a container on
    /// its way that is missing or holds a scalar, or an array index past its
    /// end.
    #[error("{0} names no place in the Property Data")]
    NoSuchPlace(JsonPointer),
    /// The Property Data of a Partial Set is not an object of pointers and
    /// scalar values.
    #[error("the Property Data of a partial change is not an object of pointers and scalars")]
    NotAChange,
}

/// The changes of a Partial Set or a `partial` update: each pointer once, its
/// value a string, number or boolean and never an object or an array (§10).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PartialUpdate(Vec<(JsonPointer, PeJson)>);

impl PartialUpdate {
    /// No change.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a change, replacing the value of one with the same pointer.
    ///
    /// # Errors
    ///
    /// [`PartialError::NotAChange`] unless the value is a string, a number or a
    /// boolean.
    pub fn with(mut self, pointer: JsonPointer, value: PeJson) -> Result<Self, PartialError> {
        if !value.is_scalar() {
            return Err(PartialError::NotAChange);
        }
        match self.0.iter_mut().find(|(p, _)| *p == pointer) {
            Some((_, old)) => *old = value,
            None => self.0.push((pointer, value)),
        }
        Ok(self)
    }

    /// The changes, in order.
    pub fn iter(&self) -> impl Iterator<Item = (&JsonPointer, &PeJson)> {
        self.0.iter().map(|(p, v)| (p, v))
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The Property Data that carries the changes: an object whose names are
    /// the pointers.
    #[must_use]
    pub fn to_json(&self) -> PeJson {
        let mut object = PeObject::new();
        for (pointer, value) in &self.0 {
            object.insert(pointer.as_str(), value.clone());
        }
        PeJson::Object(object)
    }

    /// Reads the changes of Property Data.
    ///
    /// # Errors
    ///
    /// [`PartialError::NotAChange`] unless it is an object whose names are
    /// pointers and whose values are strings, numbers or booleans.
    pub fn from_json(json: &PeJson) -> Result<Self, PartialError> {
        let PeJson::Object(object) = json else {
            return Err(PartialError::NotAChange);
        };
        let mut update = Self::new();
        for (name, value) in object.iter() {
            let pointer = JsonPointer::new(name).ok_or(PartialError::NotAChange)?;
            update = update.with(pointer, value.clone())?;
        }
        Ok(update)
    }

    /// The Property Data with the changes made, or an error and nothing changed.
    ///
    /// Each change replaces the value at its pointer: an object's property, which
    /// is added when the object has none of that name, or an element of an array,
    /// which must exist. The empty pointer replaces the whole value.
    ///
    /// # Errors
    ///
    /// [`PartialError::NoSuchPlace`] for the first pointer that cannot be followed.
    pub fn apply(&self, data: &PeJson) -> Result<PeJson, PartialError> {
        let mut out = data.clone();
        for (pointer, value) in &self.0 {
            let tokens: Vec<String> = pointer.tokens().collect();
            let place = place(&mut out, &tokens)
                .ok_or_else(|| PartialError::NoSuchPlace(pointer.clone()))?;
            *place = value.clone();
        }
        Ok(out)
    }
}

/// The value at `tokens` below `node`. The last token of an object that has no
/// such property adds it, as a null to be replaced.
fn place<'a>(node: &'a mut PeJson, tokens: &[String]) -> Option<&'a mut PeJson> {
    let Some((first, rest)) = tokens.split_first() else {
        return Some(node);
    };
    match node {
        PeJson::Object(object) => {
            if object.get(first).is_none() {
                if !rest.is_empty() {
                    return None;
                }
                object.insert(first.as_str(), PeJson::Null);
            }
            place(object.get_mut(first)?, rest)
        }
        PeJson::Array(items) => {
            let index = array_index(first)?;
            place(items.get_mut(index)?, rest)
        }
        _ => None,
    }
}

/// An array index token: digits with no leading zero (RFC 6901 §4).
fn array_index(token: &str) -> Option<usize> {
    let canonical = token == "0" || (!token.starts_with('0') && !token.is_empty());
    if canonical && token.bytes().all(|b| b.is_ascii_digit()) {
        token.parse().ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests;
