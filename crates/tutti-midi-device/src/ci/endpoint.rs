//! Inquiry: Endpoint and Reply to Endpoint (M2-101 §5.7 and §5.8, Tables 9 to 11):
//! a device asks the UMP Endpoint behind a Function Block for one piece of
//! information, selected by a Status.
//!
//! In v1.2 the only Status is `0x00`, the Endpoint's Product Instance Id; `0x01` to
//! `0x7F` are reserved (Table 10). The reply's Information Data depends on the
//! Status it echoes (§5.8.1), so the two are one value, [`EndpointInfo`]: a reply
//! cannot carry data that disagrees with its status.

use std::fmt;

use tutti_ump::U7;

use crate::name::ProductInstanceId;

/// What an Inquiry: Endpoint asks for (§5.7.1, Table 10).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EndpointStatus {
    /// `0x00`: "Product Instance ID of the UMP Endpoint to which the Responder
    /// belongs".
    ProductInstanceId,
}

impl EndpointStatus {
    /// The wire value of [`ProductInstanceId`](Self::ProductInstanceId).
    pub const PRODUCT_INSTANCE_ID: u8 = 0x00;

    /// The status, or `None` for a reserved value (`0x01..=0x7F`) or one with the
    /// top bit set.
    #[must_use]
    pub const fn new(raw: u8) -> Option<Self> {
        match raw {
            Self::PRODUCT_INSTANCE_ID => Some(Self::ProductInstanceId),
            _ => None,
        }
    }

    /// The Status byte.
    #[must_use]
    pub const fn get(self) -> U7 {
        match self {
            Self::ProductInstanceId => U7::of(Self::PRODUCT_INSTANCE_ID),
        }
    }
}

/// A Product Instance Id as MIDI-CI carries it: at most 16 bytes.
///
/// M2-101 §5.8.3.1: "Product Instance Id shall be ASCII Text in the ordinal range
/// 32-126. The Product Instance Id shall not be any longer than 16 bytes in size".
/// M2-104's UMP Stream notification of the same Id allows 42 bytes, which is
/// [`ProductInstanceId`]'s own limit; the CI message is the narrower, and the
/// Id it carries "shall match" the Stream's, so only an Id that fits both is one
/// MIDI-CI can carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CiInstanceId(ProductInstanceId);

impl CiInstanceId {
    /// The most bytes MIDI-CI carries (§5.8.3.1).
    pub const MAX_LEN: usize = 16;

    /// The Id, or `None` when it is longer than 16 bytes.
    #[must_use]
    pub fn new(id: ProductInstanceId) -> Option<Self> {
        (id.as_str().len() <= Self::MAX_LEN).then_some(Self(id))
    }

    /// The Id.
    #[must_use]
    pub fn get(&self) -> &ProductInstanceId {
        &self.0
    }

    pub(crate) fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > Self::MAX_LEN {
            return None;
        }
        Self::new(ProductInstanceId::from_bytes(bytes)?)
    }
}

impl fmt::Display for CiInstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl TryFrom<ProductInstanceId> for CiInstanceId {
    type Error = ProductInstanceId;

    fn try_from(id: ProductInstanceId) -> Result<Self, ProductInstanceId> {
        Self::new(id).ok_or(id)
    }
}

/// The Status and Information Data of a Reply to Endpoint (§5.8): the data is
/// the kind the status names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EndpointInfo {
    /// Status `0x00` with the Endpoint Product Instance Id (§5.8.3.1).
    ProductInstanceId(CiInstanceId),
}

impl EndpointInfo {
    /// The status this information answers.
    #[must_use]
    pub fn status(&self) -> EndpointStatus {
        match self {
            Self::ProductInstanceId(_) => EndpointStatus::ProductInstanceId,
        }
    }

    /// Appends Status, Length of Following Information Data (14 bits, LSB
    /// first) and the data.
    pub(crate) fn encode_body(&self, out: &mut impl Extend<u8>) {
        out.extend([self.status().get().get()]);
        match self {
            Self::ProductInstanceId(id) => {
                let text = id.get().as_str().as_bytes();
                out.extend([(text.len() & 0x7F) as u8, ((text.len() >> 7) & 0x7F) as u8]);
                out.extend(text.iter().copied());
            }
        }
    }
}

#[cfg(test)]
mod tests;
