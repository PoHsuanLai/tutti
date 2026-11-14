//! The ACK and NAK messages Property Exchange uses around chunks: the Timeout
//! Wait, the Flow Control acknowledgement and the resend request, and the
//! notices that end a transaction (M2-101 §5.10, §5.11; M2-103 §12 and §15).
//!
//! A [`PeSignal`] is the value of one of them. It is built by the reassembler
//! and turned into an [`Ack`] or a [`Nak`] with [`PeSignal::answer`]; nothing
//! here sends.

use std::time::Duration;
use tutti_ump::U7;

use super::chunk::{ChunkNo, PeRequestId, unwire14, wire14};
use crate::{Ack, Nak, TimeoutWait};

/// The time that may pass between the chunks of a message, and before a reply to
/// an inquiry: under 3 seconds (M2-103 §12.2).
pub const PE_TIMEOUT: Duration = Duration::from_secs(3);

/// A Property Exchange transaction, by the sub-ID#2 of its inquiry (M2-101 §8).
///
/// An ACK or a NAK names its transaction by that sub-ID#2, whichever side of the
/// transaction sends it (M2-103 §12.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PeTransaction {
    /// Get Property Data, `0x34`.
    Get,
    /// Set Property Data, `0x36`.
    Set,
    /// Subscription, `0x38`.
    Subscription,
}

impl PeTransaction {
    /// The sub-ID#2 of the transaction's inquiry.
    #[must_use]
    pub const fn inquiry_sub_id2(self) -> u8 {
        match self {
            Self::Get => 0x34,
            Self::Set => 0x36,
            Self::Subscription => 0x38,
        }
    }

    /// The transaction an inquiry's sub-ID#2 names.
    #[must_use]
    pub const fn from_inquiry_sub_id2(sub_id2: u8) -> Option<Self> {
        match sub_id2 {
            0x34 => Some(Self::Get),
            0x36 => Some(Self::Set),
            0x38 => Some(Self::Subscription),
            _ => None,
        }
    }
}

/// A message that carries Header Data and Property Data in chunks: an inquiry
/// of an Initiator or the reply of a Responder, of one [`PeTransaction`].
///
/// A Capabilities message has no header and a Notify message has no Property
/// Data, so neither is chunked (M2-101 §8.3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PeMessageKind {
    /// The transaction it belongs to.
    pub transaction: PeTransaction,
    /// Whether it is the Responder's reply and not the Initiator's inquiry.
    pub reply: bool,
}

impl PeMessageKind {
    /// Inquiry: Get Property Data, `0x34`.
    pub const GET_INQUIRY: Self = Self::new(PeTransaction::Get, false);
    /// Reply to Get Property Data, `0x35`.
    pub const GET_REPLY: Self = Self::new(PeTransaction::Get, true);
    /// Inquiry: Set Property Data, `0x36`.
    pub const SET_INQUIRY: Self = Self::new(PeTransaction::Set, false);
    /// Reply to Set Property Data, `0x37`.
    pub const SET_REPLY: Self = Self::new(PeTransaction::Set, true);
    /// Subscription, `0x38`.
    pub const SUBSCRIPTION_INQUIRY: Self = Self::new(PeTransaction::Subscription, false);
    /// Reply to Subscription, `0x39`.
    pub const SUBSCRIPTION_REPLY: Self = Self::new(PeTransaction::Subscription, true);

    const fn new(transaction: PeTransaction, reply: bool) -> Self {
        Self { transaction, reply }
    }

    /// The kind a sub-ID#2 names, or `None` for any other.
    #[must_use]
    pub const fn from_sub_id2(sub_id2: u8) -> Option<Self> {
        match sub_id2 {
            0x34 => Some(Self::GET_INQUIRY),
            0x35 => Some(Self::GET_REPLY),
            0x36 => Some(Self::SET_INQUIRY),
            0x37 => Some(Self::SET_REPLY),
            0x38 => Some(Self::SUBSCRIPTION_INQUIRY),
            0x39 => Some(Self::SUBSCRIPTION_REPLY),
            _ => None,
        }
    }

    /// The sub-ID#2.
    #[must_use]
    pub const fn sub_id2(self) -> u8 {
        self.transaction.inquiry_sub_id2() + self.reply as u8
    }
}

/// Which device ends a transaction with a Terminate NAK (M2-103 §12.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PeTerminator {
    /// The Responder: status data 0 (§12.1.1).
    Responder,
    /// The Initiator: status data 1 (§12.1.2).
    Initiator,
}

/// What a device says about a chunked transaction in an ACK or a NAK.
///
/// Every signal names the Request ID of the transaction and, in its details,
/// a chunk number; [`answer`](Self::answer) lays them out as M2-101 §5.10.3 has
/// the Property Exchange details (the stream id, the 14-bit chunk number, two
/// reserved bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PeSignal {
    /// ACK `0x10`: the sender cannot produce a reply or the next chunk in time,
    /// and asks to keep the transaction open for `wait` longer (M2-103 §12.2.1).
    TimeoutWait {
        /// The transaction.
        request_id: PeRequestId,
        /// The extra time.
        wait: TimeoutWait,
        /// The chunk number field, or `None` for the 0 of a transaction that has
        /// no chunk yet.
        chunk: Option<ChunkNo>,
    },
    /// ACK `0x11`, Flow Control: chunk `chunk` is received and processed, send the
    /// next (M2-103 §15.1).
    SendNext {
        /// The transaction.
        request_id: PeRequestId,
        /// The chunk acknowledged.
        chunk: ChunkNo,
    },
    /// NAK `0x12`, Flow Control: send the most recent chunk again; `last_good`
    /// is the last chunk received intact, `None` for none (M2-103 §15.4).
    Resend {
        /// The transaction.
        request_id: PeRequestId,
        /// The last successful chunk.
        last_good: Option<ChunkNo>,
    },
    /// NAK `0x20`: the transaction is ended and both devices consider it
    /// concluded (M2-103 §12.1).
    Terminate {
        /// The transaction.
        request_id: PeRequestId,
        /// The device that ends it.
        by: PeTerminator,
    },
    /// NAK `0x21`: the chunks are out of sequence and cannot be recovered.
    OutOfSequence {
        /// The transaction.
        request_id: PeRequestId,
    },
    /// NAK `0x41`: the message is malformed.
    Malformed {
        /// The transaction.
        request_id: PeRequestId,
    },
    /// NAK `0x42`: a timeout has occurred (M2-103 §12.2.3).
    TimedOut {
        /// The transaction.
        request_id: PeRequestId,
    },
}

/// The message a [`PeSignal`] is sent as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeAnswer {
    /// An ACK.
    Ack(Ack),
    /// A NAK.
    Nak(Nak),
}

fn u7(raw: u8) -> U7 {
    U7::new(raw).unwrap_or(U7::of(0))
}

impl PeSignal {
    /// The Request ID of the transaction it is about.
    #[must_use]
    pub const fn request_id(self) -> PeRequestId {
        match self {
            Self::TimeoutWait { request_id, .. }
            | Self::SendNext { request_id, .. }
            | Self::Resend { request_id, .. }
            | Self::Terminate { request_id, .. }
            | Self::OutOfSequence { request_id }
            | Self::Malformed { request_id }
            | Self::TimedOut { request_id } => request_id,
        }
    }

    /// The ACK or NAK that says it, for a message of `transaction`.
    #[must_use]
    pub fn answer(self, transaction: PeTransaction) -> PeAnswer {
        let sub_id2 = u7(transaction.inquiry_sub_id2());
        let detail = |chunk: Option<ChunkNo>| {
            let [lo, hi] = wire14(chunk.map_or(0, ChunkNo::get));
            [self.request_id().get(), lo, hi, 0, 0].map(u7)
        };
        let ack = |code: U7, data: u8, chunk| {
            PeAnswer::Ack(
                Ack::new(sub_id2, code)
                    .with_status_data(u7(data))
                    .with_details(detail(chunk)),
            )
        };
        let nak = |code: U7, data: u8, chunk| {
            PeAnswer::Nak(
                Nak::new(sub_id2, code)
                    .with_status_data(u7(data))
                    .with_details(detail(chunk)),
            )
        };
        match self {
            Self::TimeoutWait { wait, chunk, .. } => {
                ack(Ack::STATUS_TIMEOUT_WAIT, wait.tenths(), chunk)
            }
            Self::SendNext { chunk, .. } => {
                ack(Ack::STATUS_FLOW_CONTROL_NEXT_CHUNKS, 0, Some(chunk))
            }
            Self::Resend { last_good, .. } => nak(Nak::STATUS_RESEND_LAST_CHUNK, 0, last_good),
            Self::Terminate { by, .. } => nak(
                Nak::STATUS_TERMINATE,
                u8::from(by == PeTerminator::Initiator),
                None,
            ),
            Self::OutOfSequence { .. } => nak(Nak::STATUS_CHUNKS_OUT_OF_SEQUENCE, 0, None),
            Self::Malformed { .. } => nak(Nak::STATUS_MALFORMED, 0, None),
            Self::TimedOut { .. } => nak(Nak::STATUS_TIMEOUT, 0, None),
        }
    }

    /// The signal an ACK carries and the transaction it names, or `None` for an
    /// ACK that is not one of Property Exchange's.
    #[must_use]
    pub fn from_ack(ack: &Ack) -> Option<(PeTransaction, Self)> {
        let transaction = PeTransaction::from_inquiry_sub_id2(ack.ack_sub_id2.get())?;
        let (request_id, chunk) = details(&ack.details)?;
        let signal = match ack.status_code {
            Ack::STATUS_TIMEOUT_WAIT => Self::TimeoutWait {
                request_id,
                wait: TimeoutWait::from_ack(ack)?,
                chunk,
            },
            Ack::STATUS_FLOW_CONTROL_NEXT_CHUNKS => Self::SendNext {
                request_id,
                chunk: chunk?,
            },
            _ => return None,
        };
        Some((transaction, signal))
    }

    /// The signal a NAK carries and the transaction it names, or `None` for a
    /// NAK that is not one of Property Exchange's.
    #[must_use]
    pub fn from_nak(nak: &Nak) -> Option<(PeTransaction, Self)> {
        let transaction = PeTransaction::from_inquiry_sub_id2(nak.nak_sub_id2.get())?;
        let (request_id, chunk) = details(&nak.details)?;
        let signal = match nak.status_code {
            Nak::STATUS_RESEND_LAST_CHUNK => Self::Resend {
                request_id,
                last_good: chunk,
            },
            Nak::STATUS_TERMINATE => Self::Terminate {
                request_id,
                by: match nak.status_data.get() {
                    0 => PeTerminator::Responder,
                    1 => PeTerminator::Initiator,
                    _ => return None,
                },
            },
            Nak::STATUS_CHUNKS_OUT_OF_SEQUENCE => Self::OutOfSequence { request_id },
            Nak::STATUS_MALFORMED => Self::Malformed { request_id },
            Nak::STATUS_TIMEOUT => Self::TimedOut { request_id },
            _ => return None,
        };
        Some((transaction, signal))
    }
}

/// The Request ID and chunk number of the 5 details bytes.
fn details(bytes: &[U7; 5]) -> Option<(PeRequestId, Option<ChunkNo>)> {
    let request_id = PeRequestId::new(bytes[0].get())?;
    let chunk = unwire14(bytes[1].get(), bytes[2].get());
    Some((request_id, ChunkNo::new(chunk)))
}

#[cfg(test)]
mod tests;
