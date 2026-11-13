//! Holds what a device says about itself once it is open, and the check of a
//! saved device against it.
//!
//! The OS's listing ([`DeviceInfo`]) is what a device is *called*. What it
//! *is* is learned after it is opened: its Device Identity (manufacturer,
//! family, model, revision: M2-104 §7.1.3, the MIDI 1.0 Identity Reply, MIDI-CI
//! Discovery) and its Product Instance Id (M2-104 §7.1.5; M2-101 §5.8.3.1:
//! "should be the same as the Serial Number", unique per
//! Manufacturer/Family/Model, persistent across power cycles).
//!
//! A Device Identity never names a unit: two keyboards of one model share
//! it, so it can refute a saved device (a different model answers) but never
//! confirm one. Only a Product Instance Id can, so [`Learned::compare`] says
//! [`Match::SameUnit`] only when both sides have one and they are equal.
//! [`DeviceTable::resolve`](crate::DeviceTable::resolve) is unchanged: it works
//! over the listing, before anything is opened, and this check runs after.

use tutti_ump::DeviceIdentity;

use crate::id::DeviceInfo;
use crate::name::ProductInstanceId;

/// Holds what a device has said about itself so far: either field is `None`
/// until a message carries it.
///
/// Sources are not merged by guessing: [`or`](Self::or) fills a field only
/// where this value has nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Learned {
    identity: Option<DeviceIdentity>,
    instance_id: Option<ProductInstanceId>,
}

impl Learned {
    /// Returns a value with nothing known.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            identity: None,
            instance_id: None,
        }
    }

    /// Returns `self` with the Device Identity the device declared.
    #[must_use]
    pub const fn with_identity(mut self, identity: DeviceIdentity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Returns `self` with the Product Instance Id the device declared.
    #[must_use]
    pub const fn with_instance_id(mut self, id: ProductInstanceId) -> Self {
        self.instance_id = Some(id);
        self
    }

    /// Returns the Device Identity, if the device said one.
    #[must_use]
    pub const fn identity(&self) -> Option<&DeviceIdentity> {
        self.identity.as_ref()
    }

    /// Returns the Product Instance Id, if the device said one.
    #[must_use]
    pub const fn instance_id(&self) -> Option<&ProductInstanceId> {
        self.instance_id.as_ref()
    }

    /// Returns whether nothing is known.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.identity.is_none() && self.instance_id.is_none()
    }

    /// Returns `self` with each field `other` has filled in where `self` has
    /// none. The source that declares a whole Endpoint goes first
    /// (`endpoint.or(peer)`); a MIDI-CI peer, which speaks for one Function
    /// Block, fills what it left.
    #[must_use]
    pub const fn or(self, other: Self) -> Self {
        Self {
            identity: if self.identity.is_some() {
                self.identity
            } else {
                other.identity
            },
            instance_id: if self.instance_id.is_some() {
                self.instance_id
            } else {
                other.instance_id
            },
        }
    }

    /// Compares `self`, what was saved, with what the device says `now`.
    ///
    /// - A field known on both sides and different is [`Match::Different`],
    ///   whichever field it is.
    /// - Else, equal Product Instance Ids on both sides is
    ///   [`Match::SameUnit`] (the identities agree, or one side has none).
    /// - Else equal Device Identities on both sides is [`Match::SameModel`]:
    ///   another unit of the model answers alike.
    /// - Else nothing was comparable: [`Match::Unknown`].
    #[must_use]
    pub fn compare(&self, now: &Self) -> Match {
        let instance = both(self.instance_id, now.instance_id);
        let identity = both(self.identity, now.identity);
        match (instance, identity) {
            (Some(false), _) | (_, Some(false)) => Match::Different,
            (Some(true), _) => Match::SameUnit,
            (None, Some(true)) => Match::SameModel,
            (None, None) => Match::Unknown,
        }
    }
}

/// Returns whether two values known on both sides are equal; `None` when either side
/// is unknown.
fn both<T: PartialEq>(a: Option<T>, b: Option<T>) -> Option<bool> {
    Some(a? == b?)
}

/// Names what [`Learned::compare`] concluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Match {
    /// Both sides have a Product Instance Id, and it is the same: the saved
    /// unit.
    SameUnit,
    /// The Device Identity is the same and no Product Instance Id is on both
    /// sides: the saved model, which may be another unit of it.
    SameModel,
    /// A field known on both sides differs: not the saved device.
    Different,
    /// Nothing comparable is known on both sides yet.
    Unknown,
}

/// Holds a device an app keeps: the listing's row and what the device said of
/// itself when it was last open. Restore it with
/// [`DeviceTable::resolve`](crate::DeviceTable::resolve) on [`info`](Self::info),
/// then, once the device is open and has spoken, [`confirm`](Self::confirm).
///
/// The serde form is the plain derive: both parts are valid on their own, and
/// `learned` defaults to empty, so a file that kept only a [`DeviceInfo`]
/// reads as a saved device that knows nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SavedDevice {
    info: DeviceInfo,
    #[cfg_attr(feature = "serde", serde(default))]
    learned: Learned,
}

impl SavedDevice {
    /// Returns a saved device with what it said.
    #[must_use]
    pub const fn new(info: DeviceInfo, learned: Learned) -> Self {
        Self { info, learned }
    }

    /// Returns the listing's row, for [`DeviceTable::resolve`](crate::DeviceTable::resolve).
    #[must_use]
    pub const fn info(&self) -> &DeviceInfo {
        &self.info
    }

    /// Returns what it said of itself.
    #[must_use]
    pub const fn learned(&self) -> &Learned {
        &self.learned
    }

    /// Compares what the device now says with what was saved, as [`Learned::compare`] does.
    #[must_use]
    pub fn confirm(&self, now: &Learned) -> Match {
        self.learned.compare(now)
    }
}

#[cfg(test)]
mod tests;
