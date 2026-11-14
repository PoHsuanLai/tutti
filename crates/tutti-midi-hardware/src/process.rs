//! The Process Inquiry of a MIDI-CI peer, as the session drives it: the peers
//! that can be asked for a MIDI Message Report ([`ReportPeer`]), the request
//! ([`ReportAsk`]) and what happened ([`ProcessChange`]).
//!
//! The session is the Initiator (M2-101 §9): it asks every peer that declared
//! Process Inquiry for its capabilities when it is found, and lists those whose
//! reply says they support MIDI Message Report. A request is made only from such
//! a handle, so an app cannot ask a peer that never declared the category or
//! answered that it lacks the feature. The handle is a snapshot: the peer may be
//! lost before the request is sent, and that is refused ([`ReportError::NoSuchPeer`]).
//!
//! A report is a Reply, the MIDI messages themselves, and an End. The session
//! tells of the first and the last ([`ProcessChange::ReportBegan`],
//! [`ProcessChange::ReportEnded`]); the messages between them arrive on the
//! input as the MIDI they are, in order (§9.8.2), and reach the graph like any
//! other message of the device.

use std::sync::Arc;

use tutti_midi_device::{
    DeviceId, Muid, Nak, ProcessError, ProcessFeatures, ReportRequest, ReportScope,
};
use tutti_ump::MidiGroup;

/// A peer that declared Process Inquiry and answered that it supports MIDI
/// Message Report: the handle a [`ReportAsk`] is made from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportPeer {
    pub(crate) device: DeviceId,
    pub(crate) group: MidiGroup,
    pub(crate) muid: Muid,
}

impl ReportPeer {
    /// The input device the peer was found on.
    #[must_use]
    pub const fn device(&self) -> &DeviceId {
        &self.device
    }

    /// The Group of the connection the peer answered on.
    #[must_use]
    pub const fn group(&self) -> MidiGroup {
        self.group
    }

    /// The peer's MUID.
    #[must_use]
    pub const fn muid(&self) -> Muid {
        self.muid
    }

    /// A request that the peer report what `request` asks
    /// ([`MidiSession::report`](crate::MidiSession::report)).
    #[must_use]
    pub fn ask(&self, request: ReportRequest) -> ReportAsk {
        ReportAsk {
            peer: self.clone(),
            request,
        }
    }
}

/// A request for a MIDI Message Report from one peer. Made only from a
/// [`ReportPeer`], and it names the input device that handle was read from, so it
/// cannot be sent to another.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportAsk {
    pub(crate) peer: ReportPeer,
    pub(crate) request: ReportRequest,
}

/// The peers of one input that can be asked for a report.
pub(crate) type Snapshot = Arc<[ReportPeer]>;

/// A change in the Process Inquiry of a peer, once each from
/// [`MidiSession::poll`](crate::MidiSession::poll). Every variant names the peer
/// by the Group it was found on and its MUID.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProcessChange {
    /// The peer's Reply to Process Inquiry Capabilities arrived. If it
    /// supports MIDI Message Report it is now in
    /// [`MidiSession::report_peers`](crate::MidiSession::report_peers).
    Capabilities {
        /// The Group.
        group: MidiGroup,
        /// The peer.
        muid: Muid,
        /// What it supports.
        features: ProcessFeatures,
    },
    /// The peer did not reply to the capabilities inquiry in time.
    CapabilitiesTimedOut {
        /// The Group.
        group: MidiGroup,
        /// The peer.
        muid: Muid,
    },
    /// The peer NAKed the capabilities inquiry: it has no Process Inquiry.
    CapabilitiesRefused {
        /// The Group.
        group: MidiGroup,
        /// The peer.
        muid: Muid,
    },
    /// The peer's Reply to MIDI Message Report arrived: the messages of the
    /// report follow on the input.
    ReportBegan {
        /// The Group.
        group: MidiGroup,
        /// The peer.
        muid: Muid,
        /// The kinds of message it will send, of those asked.
        declared: ReportScope,
    },
    /// The peer's End of MIDI Message Report arrived: the report is complete.
    ReportEnded {
        /// The Group.
        group: MidiGroup,
        /// The peer.
        muid: Muid,
    },
    /// The peer NAKed the report (§9.8.1), which ends it, complete or not.
    ReportRefused {
        /// The Group.
        group: MidiGroup,
        /// The peer.
        muid: Muid,
        /// Its NAK, whose status says whether to retry.
        nak: Nak,
    },
    /// The peer did not finish the report in time.
    ReportTimedOut {
        /// The Group.
        group: MidiGroup,
        /// The peer.
        muid: Muid,
    },
    /// A [`ReportAsk`] was not sent: one was already waiting, or the peer has since
    /// said it does not support MIDI Message Report.
    NotSent {
        /// The Group.
        group: MidiGroup,
        /// The peer.
        muid: Muid,
        /// Why.
        error: ProcessError,
    },
    /// The peer is gone (lost, or MIDI-CI ended): its Initiator went with it.
    Dropped {
        /// The Group.
        group: MidiGroup,
        /// The peer.
        muid: Muid,
    },
}

/// A [`ReportAsk`] that cannot be made.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum ReportError {
    /// The device the request was made from is no longer an open input.
    #[error("the device is not an open input")]
    NotOpen,
    /// The peer is not one found on that input that supports MIDI Message Report
    /// (it was lost, or has since answered that it does not).
    #[error("no such MIDI-CI peer supporting MIDI Message Report on that input")]
    NoSuchPeer,
}
