//! [`CiDevice`]: one MIDI-CI Device on one Group of one connection, and the
//! Management messages it runs (M2-101 §3.3, §4, §5.5 to §5.9, Appendix E).
//!
//! It is a pure state machine in the shape of [`Connection`](crate::Connection):
//! every step returns the [`CiAction`]s to carry out, time comes in as an
//! [`Instant`] argument, and nothing here reads a clock, sends a byte, draws from
//! an OS random source or runs a thread. The caller routes to it the CI messages
//! of **one Group** of the connection (a [`Decoded`] from
//! [`CiMessage::decode`]), and sends every [`CiAction::Send`].
//!
//! tutti has no Function Block, so the device is one that "is not associated with a
//! Function Block" (§5.6.2): it speaks with Device ID `0x7F` and answers Discovery
//! with Function Block `0x7F`. A device with Function Blocks would owe a Function Block
//! Info Change Alert (UMP) whenever its MUID changes (§3.3.1); this one has none to
//! alert, and [`CiAction::MuidChanged`] is how the caller learns of the change.
//!
//! # The rules
//!
//! - **Discovery as Initiator** (§4.1, §5.5): [`CiDevice::new`] and every new MUID start a
//!   round: a Discovery from the device's MUID to the Broadcast MUID, Output Path ID 0
//!   (§5.5.4: one MIDI Out). The round's window is [`DISCOVERY_WINDOW`] (§5.5.5);
//!   [`CiAction::WindowClosed`] is signalled once when it ends.
//! - **Replies** (§5.6): a Reply to Discovery addressed to the device records its peer
//!   ([`CiAction::Found`]). Replies for another MUID are not the device's.
//! - **Collision among replies** (§5.9.2): a second reply with the same source MUID in a
//!   round means two Responders share a MUID; the device sends an Invalidate MUID for it
//!   and forgets it ([`CiAction::Lost`] if it had been found). Replies are counted for
//!   the whole round, not only inside the window.
//! - **Discovery as Responder** (§4.1, §5.6): a Discovery from another MUID is answered
//!   to it, echoing its Output Path ID (§5.6.1), with Function Block `0x7F`.
//! - **Collision on the device's MUID** (§5.9.1): a Discovery whose Source MUID is the
//!   device's own and whose body is not the one it sent, or a Reply from the device's
//!   MUID addressed to it (an answer to its Discovery from a device that drew the same
//!   MUID). Option A needs a MUID not yet used in any Transaction, and the
//!   device's is (its own Discovery), so it always takes **Option B**: change the MUID, send
//!   an Invalidate MUID targeting the duplicated one, and start a new round. The Invalidate
//!   goes *from the new MUID*, because a source equal to the target would not tell the
//!   other device from this one.
//! - **Echo** (Appendix A.1.1): a MIDI Thru or merge loops the device's own messages back.
//!   Its Discovery returns with its MUID and its own body, and is ignored; its Reply to
//!   another device returns addressed to that device, and is ignored. A device of the
//!   same model that drew the same MUID sends the same Discovery body and is not seen
//!   there, but it answers this device's Discovery from that MUID to it, which is.
//!   Treating an echo as a collision would change the MUID on every round, for ever.
//! - **Invalidate MUID** (§5.9): its target the device's MUID ends every Transaction and
//!   starts over under a new MUID; its target a peer's discards that peer
//!   ([`CiAction::Lost`]). It is never answered.
//! - **NAK** (§5.3, §5.11, Appendix E): a message addressed to the device's MUID that it
//!   cannot handle (one the codec could not read, which includes every kind it does
//!   not model, Profile and Property Exchange among them) is answered with a NAK carrying Table 16's status. A message for
//!   another MUID, one to the Broadcast MUID, an ACK and a NAK are never NAK'd.
//! - **Endpoint Inquiry as Initiator** (§5.7): once, for each found peer that declared
//!   a Function Block, status `0x00`. The reply sets the peer's Product Instance Id
//!   ([`CiAction::Updated`]); a NAK from it leaves it `None`. **No retry and no timeout**:
//!   §5.7 and §5.8 give none, the id is a convenience (Appendix A.1.1), and a peer
//!   that never answers is just one with no id.
//! - **Endpoint Inquiry as Responder** (§5.8): tutti has no UMP Endpoint Product
//!   Instance Id to report, so it NAKs ("If the Device is not able to provide the
//!   requested information ... it should reply with a MIDI-CI NAK"), status `0x00`.
//!
//! Every new MUID is drawn from the `entropy` the caller gave, redrawn while it is
//! reserved, the device's own or a found peer's. The closure must not return one
//! value for ever, or the redraw does not end.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};
use std::vec::Vec;

use tutti_ump::{DeviceIdentity, U7};

use super::{
    CI_VERSION, CiCategories, CiDeviceId, CiHeader, CiInstanceId, CiMessage, Decoded, Destination,
    DiscoveryData, EndpointInfo, EndpointStatus, MaxSysexSize, Muid, Nak, SUB_ID2_ENDPOINT_INQUIRY,
};
use tutti_ump::FunctionBlockId;

use crate::name::ProductInstanceId;

/// How long a Discovery waits for its replies: "at least 3 seconds" (§5.5.5).
pub const DISCOVERY_WINDOW: Duration = Duration::from_secs(3);

/// What the driver does next.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CiAction {
    /// Send this message to the peer(s) on the connection.
    Send(CiMessage),
    /// A peer answered the Discovery: [`CiDevice::peer`] has it.
    Found(Muid),
    /// What a peer's entry holds changed (its Product Instance Id arrived).
    Updated(Muid),
    /// A peer is gone: invalidated, in collision, or dropped with the device's
    /// own MUID. Whatever the caller keyed to it ends.
    Lost(Muid),
    /// The device's MUID changed (§5.9, §3.3.1): every Transaction under the old
    /// one has ended and a new round is on its way.
    MuidChanged {
        /// The MUID that is no longer the device's.
        old: Muid,
        /// The device's MUID now.
        new: Muid,
    },
    /// The Discovery round's window ([`DISCOVERY_WINDOW`]) has ended.
    WindowClosed,
}

/// What Discovery told about a peer, and what was learned since.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CiPeer {
    data: DiscoveryData,
    instance_id: Option<CiInstanceId>,
    asked: bool,
}

impl CiPeer {
    /// What its Reply to Discovery declared: identity, categories, receivable
    /// size, and the Output Path ID and Function Block of the reply.
    #[must_use]
    pub fn discovery(&self) -> &DiscoveryData {
        &self.data
    }

    /// The Function Block the peer represents, `None` for none (`0x7F`).
    #[must_use]
    pub fn function_block(&self) -> Option<FunctionBlockId> {
        self.data.function_block
    }

    /// Its Endpoint's Product Instance Id, once an Endpoint Inquiry was answered.
    #[must_use]
    pub fn product_instance_id(&self) -> Option<&ProductInstanceId> {
        self.instance_id.as_ref().map(CiInstanceId::get)
    }
}

#[derive(Clone, Debug)]
struct Round {
    window_end: Instant,
    closed: bool,
    /// Every source of a Reply to Discovery addressed to us this round, found
    /// or in collision.
    replied: BTreeSet<Muid>,
}

/// One MIDI-CI Device on one Group of one connection.
pub struct CiDevice<R> {
    muid: Muid,
    own: DiscoveryData,
    entropy: R,
    peers: BTreeMap<Muid, CiPeer>,
    round: Round,
}

impl<R> std::fmt::Debug for CiDevice<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CiDevice")
            .field("muid", &self.muid)
            .field("peers", &self.peers)
            .field("round", &self.round)
            .finish_non_exhaustive()
    }
}

impl<R: FnMut() -> u32> CiDevice<R> {
    /// A device with a MUID drawn from `entropy`, and the actions that start its
    /// first round at `now`: the Discovery to send.
    ///
    /// `max_sysex_size` is what it can receive (§5.5.3: 128 or more, which the type
    /// holds); `categories` are what it supports, [`CiCategories::empty`] for
    /// Management alone. The Output Path ID and Function Block of its Discovery data
    /// are the device's, not a caller's: 0 and none.
    pub fn new(
        identity: DeviceIdentity,
        categories: CiCategories,
        max_sysex_size: MaxSysexSize,
        mut entropy: R,
        now: Instant,
    ) -> (Self, Vec<CiAction>) {
        let muid = draw(&mut entropy, |_| false);
        let mut device = Self {
            muid,
            own: DiscoveryData::new(identity, categories, max_sysex_size),
            entropy,
            peers: BTreeMap::new(),
            round: Round {
                window_end: now,
                closed: true,
                replied: BTreeSet::new(),
            },
        };
        let mut actions = Vec::new();
        device.start_round(now, &mut actions);
        (device, actions)
    }

    /// The device's MUID.
    #[must_use]
    pub fn muid(&self) -> Muid {
        self.muid
    }

    /// The peers found, in MUID order.
    pub fn peers(&self) -> impl Iterator<Item = (Muid, &CiPeer)> {
        self.peers.iter().map(|(muid, peer)| (*muid, peer))
    }

    /// One found peer.
    #[must_use]
    pub fn peer(&self, muid: Muid) -> Option<&CiPeer> {
        self.peers.get(&muid)
    }

    /// When the device next needs [`on_timeout`](Self::on_timeout): the end of the
    /// round's window, or `None` once it has closed.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        (!self.round.closed).then_some(self.round.window_end)
    }

    /// Time has passed: closes the window once [`DISCOVERY_WINDOW`] is over.
    /// Calling it early is harmless.
    pub fn on_timeout(&mut self, now: Instant) -> Vec<CiAction> {
        if !self.round.closed && now >= self.round.window_end {
            self.round.closed = true;
            return vec![CiAction::WindowClosed];
        }
        Vec::new()
    }

    /// A CI message of this device's Group arrived.
    pub fn on_message(&mut self, decoded: &Decoded, now: Instant) -> Vec<CiAction> {
        let mut actions = Vec::new();
        match decoded {
            Decoded::NotCi => {}
            Decoded::Unread(unread) => {
                if unread.destination == Some(Destination::Muid(self.muid))
                    && let Some(nak) = unread.nak(self.muid)
                {
                    actions.push(CiAction::Send(nak));
                }
            }
            Decoded::Message(message) => self.on_decoded(message, now, &mut actions),
        }
        actions
    }

    fn on_decoded(&mut self, message: &CiMessage, now: Instant, actions: &mut Vec<CiAction>) {
        match message {
            CiMessage::Discovery { header, data } => {
                if header.source == self.muid {
                    // Appendix A.1.1: our own Discovery, looped back by a Thru or a
                    // merge, is the message we sent; anything else under our MUID is
                    // another device that drew it.
                    if *data != self.sent_discovery() {
                        self.collide(now, actions);
                    }
                } else {
                    let mut reply = self.own;
                    reply.output_path_id = data.output_path_id;
                    reply.function_block = None;
                    actions.push(CiAction::Send(CiMessage::DiscoveryReply {
                        header: self.header(),
                        to: header.source,
                        data: reply,
                    }));
                }
            }
            CiMessage::DiscoveryReply { header, to, data } => {
                if header.source == self.muid {
                    // A Reply from our MUID to our MUID answers our Discovery from a
                    // device that drew our MUID (we never answer ourselves); one to
                    // another MUID is our own Reply looped back.
                    if *to == self.muid {
                        self.collide(now, actions);
                    }
                } else if *to == self.muid {
                    self.on_reply(header.source, *data, actions);
                }
            }
            CiMessage::InvalidateMuid { target, .. } => {
                if *target == self.muid {
                    self.renew(None, now, actions);
                } else if self.peers.remove(target).is_some() {
                    actions.push(CiAction::Lost(*target));
                }
            }
            CiMessage::EndpointInquiry { header, to, .. } => {
                if *to == self.muid {
                    actions.push(self.nak(
                        header.source,
                        SUB_ID2_ENDPOINT_INQUIRY,
                        Nak::STATUS_NAK,
                    ));
                }
            }
            CiMessage::EndpointReply { header, to, info } => {
                if *to == self.muid
                    && let Some(peer) = self.peers.get_mut(&header.source)
                    && peer.asked
                {
                    let EndpointInfo::ProductInstanceId(id) = info;
                    peer.instance_id = Some(*id);
                    actions.push(CiAction::Updated(header.source));
                }
            }
            // A NAK of the Endpoint Inquiry ends it: the peer has no id to give.
            CiMessage::Nak { header, to, nak } => {
                if *to == self.muid
                    && nak.nak_sub_id2.get() == SUB_ID2_ENDPOINT_INQUIRY
                    && let Some(peer) = self.peers.get_mut(&header.source)
                {
                    peer.asked = false;
                }
            }
            // Never answered: a pair of devices would trade NAKs for ever.
            CiMessage::Ack { .. } => {}
        }
    }

    fn on_reply(&mut self, source: Muid, data: DiscoveryData, actions: &mut Vec<CiAction>) {
        if !self.round.replied.insert(source) {
            // §5.9.2: a second reply with the same MUID.
            actions.push(CiAction::Send(CiMessage::InvalidateMuid {
                header: self.header(),
                target: source,
            }));
            if self.peers.remove(&source).is_some() {
                actions.push(CiAction::Lost(source));
            }
            return;
        }
        let asks = data.function_block.is_some();
        self.peers.insert(
            source,
            CiPeer {
                data,
                instance_id: None,
                asked: asks,
            },
        );
        actions.push(CiAction::Found(source));
        if asks {
            actions.push(CiAction::Send(CiMessage::EndpointInquiry {
                header: self.header(),
                to: source,
                status: EndpointStatus::ProductInstanceId,
            }));
        }
    }

    /// §5.9.1 Option B: a Discovery or Reply carried our own MUID.
    fn collide(&mut self, now: Instant, actions: &mut Vec<CiAction>) {
        let duplicated = self.muid;
        self.renew(Some(duplicated), now, actions);
    }

    /// Changes the MUID, drops every peer, announces the Invalidate (when one is
    /// owed) and starts a new round.
    fn renew(&mut self, invalidate: Option<Muid>, now: Instant, actions: &mut Vec<CiAction>) {
        let old = self.muid;
        let peers = &self.peers;
        self.muid = draw(&mut self.entropy, |m| m == old || peers.contains_key(&m));
        actions.push(CiAction::MuidChanged {
            old,
            new: self.muid,
        });
        for (peer, _) in std::mem::take(&mut self.peers) {
            actions.push(CiAction::Lost(peer));
        }
        if let Some(target) = invalidate {
            actions.push(CiAction::Send(CiMessage::InvalidateMuid {
                header: self.header(),
                target,
            }));
        }
        self.start_round(now, actions);
    }

    fn start_round(&mut self, now: Instant, actions: &mut Vec<CiAction>) {
        self.round = Round {
            window_end: now + DISCOVERY_WINDOW,
            closed: false,
            replied: BTreeSet::new(),
        };
        actions.push(CiAction::Send(CiMessage::Discovery {
            header: self.header(),
            data: self.sent_discovery(),
        }));
    }

    /// The body of the Discovery this device sends: Output Path 0, no Function Block.
    fn sent_discovery(&self) -> DiscoveryData {
        let mut data = self.own;
        data.output_path_id = U7::default();
        data.function_block = None;
        data
    }

    fn header(&self) -> CiHeader {
        CiHeader {
            device_id: CiDeviceId::FunctionBlock,
            ci_version: CI_VERSION,
            source: self.muid,
        }
    }

    fn nak(&self, to: Muid, original: u8, status: U7) -> CiAction {
        CiAction::Send(CiMessage::Nak {
            header: self.header(),
            to,
            // A sub-ID#2 is seven bits wherever it was read from.
            nak: Nak::new(U7::new(original).unwrap_or_default(), status),
        })
    }
}

/// A MUID from `entropy` that `taken` does not refuse.
fn draw(entropy: &mut impl FnMut() -> u32, taken: impl Fn(Muid) -> bool) -> Muid {
    loop {
        if let Some(muid) = Muid::from_entropy(entropy())
            && !taken(muid)
        {
            return muid;
        }
    }
}

#[cfg(test)]
mod tests;
