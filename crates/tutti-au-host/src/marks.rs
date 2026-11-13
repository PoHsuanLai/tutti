//! [`Marks`]: what the render tells the control side about each parameter it
//! drove, one dirty bit per parameter, with no queue to fill.
//!
//! The render must not call `AUParameterListenerNotify` (it locks and
//! dispatches) nor write a `NonRealTime` parameter, so it leaves both for the
//! control tick. A ring of ids or values would drop a notification when full;
//! a bit per parameter cannot: marking a parameter twice is one notification
//! (the listeners read the unit's own value, so one tell is all a changed
//! parameter needs), and the set has room for every parameter the unit
//! declares. A `NonRealTime` write carries a value, which is the latest value
//! per parameter and nothing more: the render stores it in the parameter's
//! slot, then marks the bit, and the tick reads the slot of each marked
//! parameter. A slot overwritten after the tick took its bit but before it
//! reads is applied once now and once at the next tick, which is the same
//! value.
//!
//! AU parameter ids are sparse `u32`s, so [`Marks`] holds the sorted ids it was
//! sized for and a parameter's [`Slot`] is its position in them. A table made
//! for a parameter list the marks do not cover gets a fresh [`Marks`]
//! ([`Marks::covering`]), and the old one is drained by the controller until the
//! render has let go of it.

#![cfg(target_os = "macos")]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use tutti_rt::{DirtyIndex, DirtySet};

/// A parameter's place in a [`Marks`]: only the marks that made it can.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Slot(DirtyIndex);

/// The parameters one render told the control side of, and the latest value it
/// wrote to each `NonRealTime` one.
#[derive(Debug)]
pub(crate) struct Marks {
    /// Every parameter id the marks are sized for, ascending.
    ids: Box<[u32]>,
    /// The automated values the unit applied, to tell the listeners of.
    told: DirtySet,
    /// The `NonRealTime` values waiting for a write on the control side.
    deferred: DirtySet,
    /// The latest value (`f32` bits) of each parameter, read for a bit of `deferred`.
    values: Box<[AtomicU32]>,
}

impl Default for Marks {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl Marks {
    /// Marks for `ids` (sorted and deduplicated here). Allocates: the control
    /// side's call.
    pub(crate) fn new(mut ids: Vec<u32>) -> Self {
        ids.sort_unstable();
        ids.dedup();
        Self {
            told: DirtySet::new(ids.len()),
            deferred: DirtySet::new(ids.len()),
            values: ids.iter().map(|_| AtomicU32::new(0)).collect(),
            ids: ids.into(),
        }
    }

    /// `previous` when it has a slot for every id in `ids` (nothing to drain
    /// apart), otherwise new marks for `ids`.
    pub(crate) fn covering(previous: Option<&Arc<Self>>, ids: &[u32]) -> Arc<Self> {
        match previous {
            Some(marks) if ids.iter().all(|&id| marks.slot(id).is_some()) => Arc::clone(marks),
            _ => Arc::new(Self::new(ids.to_vec())),
        }
    }

    /// `id`'s slot, or `None` for a parameter these marks were not sized for.
    pub(crate) fn slot(&self, id: u32) -> Option<Slot> {
        let at = self.ids.binary_search(&id).ok()?;
        self.told.index(at).map(Slot)
    }

    /// The render's call: the unit applied an automated value of this parameter.
    pub(crate) fn tell(&self, slot: Slot) {
        self.told.set(slot.0);
    }

    /// The render's call: this `NonRealTime` parameter should become `value`.
    ///
    /// The last writer wins: two writes to one parameter between ticks deliver
    /// only the later value, which is what a parameter means (its state is its
    /// latest value, never a history). The value is stored before the bit is
    /// set (Release), so a tick that takes the bit (Acquire) reads it.
    pub(crate) fn defer(&self, slot: Slot, value: f32) {
        self.values[slot.0.get()].store(value.to_bits(), Ordering::Relaxed);
        self.deferred.set(slot.0);
    }

    /// The parameters to tell the listeners of since the last take.
    pub(crate) fn take_told(&self) -> impl Iterator<Item = u32> + '_ {
        self.told.take().map(|at| self.ids[at.get()])
    }

    /// The `NonRealTime` parameters to write since the last take, each with its
    /// latest value.
    pub(crate) fn take_deferred(&self) -> impl Iterator<Item = (u32, f32)> + '_ {
        self.deferred.take().map(|at| {
            let bits = self.values[at.get()].load(Ordering::Relaxed);
            (self.ids[at.get()], f32::from_bits(bits))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sparse id has the slot of its position, an id the marks were not
    /// sized for has none, and `covering` reuses marks that fit and replaces
    /// marks that do not.
    #[test]
    fn slots_follow_sparse_ids_and_covering_replaces_what_does_not_fit() {
        let marks = Arc::new(Marks::new(vec![900, 3, 70_000]));
        assert!(marks.slot(3).is_some() && marks.slot(70_000).is_some());
        assert!(marks.slot(4).is_none());
        assert!(Arc::ptr_eq(
            &marks,
            &Marks::covering(Some(&marks), &[3, 900])
        ));
        let grown = Marks::covering(Some(&marks), &[3, 4]);
        assert!(!Arc::ptr_eq(&marks, &grown));
        assert!(grown.slot(4).is_some());
    }

    /// A parameter told many times is told once, and a deferred one carries the
    /// latest value written to it.
    #[test]
    fn many_marks_of_one_parameter_are_one_take_with_the_latest_value() {
        let marks = Marks::new(vec![10, 20]);
        let (a, b) = (marks.slot(10).unwrap(), marks.slot(20).unwrap());
        for _ in 0..5000 {
            marks.tell(a);
            marks.tell(b);
        }
        for v in 0..5000 {
            marks.defer(b, v as f32);
        }
        assert_eq!(marks.take_told().collect::<Vec<_>>(), [10, 20]);
        assert_eq!(marks.take_deferred().collect::<Vec<_>>(), [(20, 4999.0)]);
        assert_eq!(marks.take_told().count() + marks.take_deferred().count(), 0);
    }
}
