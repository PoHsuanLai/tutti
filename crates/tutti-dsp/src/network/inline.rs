//! [`Inline`]: a stack array with a length.

use std::ops::Deref;

/// A fixed-capacity list on the stack: `N` slots of `T` and a length.
///
/// It exists so a [`Network`](crate::Network) can be built on the audio thread
/// without allocating. The slots past the length hold `T::default()` and are
/// never read; equality compares only the live ones.
#[derive(Clone, Copy, Debug)]
pub struct Inline<T, const N: usize> {
    items: [T; N],
    len: usize,
}

/// A push onto a full [`Inline`]: it carries the refused value back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Full<T>(pub T);

impl<T: Default, const N: usize> Inline<T, N> {
    /// The capacity.
    pub const CAPACITY: usize = N;

    /// An empty list.
    #[must_use]
    pub fn new() -> Self {
        Self {
            items: std::array::from_fn(|_| T::default()),
            len: 0,
        }
    }

    /// Appends `value`.
    ///
    /// # Errors
    ///
    /// [`Full`] with the value, when all `N` slots are live.
    pub fn push(&mut self, value: T) -> Result<(), Full<T>> {
        if self.len == N {
            return Err(Full(value));
        }
        self.items[self.len] = value;
        self.len += 1;
        Ok(())
    }

    /// A list of `items`, or `None` when they do not fit.
    #[must_use]
    pub fn from_slice(items: &[T]) -> Option<Self>
    where
        T: Clone,
    {
        if items.len() > N {
            return None;
        }
        let mut list = Self::new();
        for (slot, item) in list.items.iter_mut().zip(items) {
            slot.clone_from(item);
        }
        list.len = items.len();
        Some(list)
    }

    /// `len` copies of `value`, or `None` when they do not fit.
    #[must_use]
    pub fn filled(value: T, len: usize) -> Option<Self>
    where
        T: Clone,
    {
        if len > N {
            return None;
        }
        let mut list = Self::new();
        list.items[..len].fill(value);
        list.len = len;
        Some(list)
    }

    /// The live items.
    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        &self.items[..self.len]
    }

    /// The live items, mutably.
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.items[..self.len]
    }
}

impl<T: Default, const N: usize> Default for Inline<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> Deref for Inline<T, N> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.items[..self.len]
    }
}

impl<T: PartialEq, const N: usize> PartialEq for Inline<T, N> {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A push fills the slots in order, refuses the one past the capacity and
    /// hands the value back; equality ignores what is in the dead slots.
    ///
    /// Mutation (run): `>` for `==` in the `push` guard writes out of bounds
    /// (panics); skipping `self.len += 1` fails the length.
    #[test]
    fn a_push_past_the_capacity_is_refused_with_the_value() {
        let mut list: Inline<u8, 2> = Inline::new();
        assert_eq!(list.push(1), Ok(()));
        assert_eq!(list.push(2), Ok(()));
        assert_eq!(list.push(3), Err(Full(3)));
        assert_eq!(&*list, &[1, 2]);
    }

    /// `from_slice` and `filled` refuse what does not fit, and equality compares
    /// the live items only.
    ///
    /// Mutation (run): `<` for `>` in the `from_slice` guard accepts 3
    /// into 2 and fails; comparing `items` instead of the slices fails the last
    /// assert.
    #[test]
    fn construction_refuses_what_does_not_fit_and_equality_reads_live_items() {
        assert!(Inline::<u8, 2>::from_slice(&[1, 2, 3]).is_none());
        assert!(Inline::<u8, 2>::filled(7, 3).is_none());
        let a = Inline::<u8, 3>::from_slice(&[1, 2]).unwrap();
        let mut b = Inline::<u8, 3>::from_slice(&[1, 2, 9]).unwrap();
        assert_ne!(a, b);
        b = Inline::<u8, 3>::from_slice(&[1, 2]).unwrap();
        assert_eq!(a, b);
    }
}
