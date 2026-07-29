//! MIDI-CI Property Exchange (M2-101 §7).
//!
//! Property Exchange moves structured data (JSON, encoded as UTF-8 bytes) between
//! devices: a header describing the resource, then a chunked body. This layer
//! models the **Get Property Data** / **Set Property Data** requests and their
//! replies at the message level — the header and body are kept as opaque byte
//! blobs (the JSON schema is the application's concern, not the codec's).
//!
//! Wire body (M2-101 §7.1.3): `request_id`, a 2-byte header length + header
//! bytes, a 2-byte chunk count, a 2-byte chunk number, then a 2-byte body length
//! + body bytes.

use std::vec::Vec;

/// Sub-ID#2: Get Property Data (inquiry).
pub const SUB_ID2_GET_PROPERTY_DATA: u8 = 0x34;
/// Sub-ID#2: Get Property Data Reply.
pub const SUB_ID2_GET_PROPERTY_DATA_REPLY: u8 = 0x35;
/// Sub-ID#2: Set Property Data (inquiry).
pub const SUB_ID2_SET_PROPERTY_DATA: u8 = 0x36;
/// Sub-ID#2: Set Property Data Reply.
pub const SUB_ID2_SET_PROPERTY_DATA_REPLY: u8 = 0x37;
/// Sub-ID#2: Subscription (M2-101 §8.11).
pub const SUB_ID2_SUBSCRIPTION: u8 = 0x38;
/// Sub-ID#2: Reply to Subscription (M2-101 §8.12).
pub const SUB_ID2_SUBSCRIPTION_REPLY: u8 = 0x39;

/// `true` if `sub_id2` belongs to the Property Exchange family (this subset).
pub(super) fn is_property_sub_id2(sub_id2: u8) -> bool {
    matches!(
        sub_id2,
        SUB_ID2_GET_PROPERTY_DATA
            | SUB_ID2_GET_PROPERTY_DATA_REPLY
            | SUB_ID2_SET_PROPERTY_DATA
            | SUB_ID2_SET_PROPERTY_DATA_REPLY
            | SUB_ID2_SUBSCRIPTION
            | SUB_ID2_SUBSCRIPTION_REPLY
    )
}

/// Which Property Exchange message this is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropertyKind {
    GetData,
    GetDataReply,
    SetData,
    SetDataReply,
    /// Subscription (M2-101 §8.11) — establishes, updates, or ends a
    /// subscription. Which of those it does lives in the header's `command`
    /// property, not in the sub-ID; see [`SubscriptionCommand`].
    Subscription,
    /// Reply to Subscription (§8.12). §11.2 of M2-103 makes this mandatory:
    /// a device "shall reply with a Reply to Subscription message, so the
    /// original sender is aware of the success or failure of a command."
    SubscriptionReply,
}

/// A Property Exchange message body (M2-101 §7.1.3). The `header` and `body` are
/// opaque UTF-8/JSON blobs; `chunk`/`num_chunks` support fragmenting a large body
/// across several messages (both `1` for a single-message exchange).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PropertyData {
    pub kind: PropertyKind,
    /// Request id tying a reply to its request (M2-101 §7.1.1).
    pub request_id: u8,
    /// Property header blob (JSON: resource, status, …).
    pub header: Vec<u8>,
    /// Total number of body chunks in this exchange (≥ 1).
    pub num_chunks: u16,
    /// This message's chunk number (1-based).
    pub chunk: u16,
    /// The body blob for this chunk (JSON payload bytes).
    pub body: Vec<u8>,
}

impl PropertyData {
    /// The sub-ID#2 that carries this message on the wire.
    pub(super) fn sub_id2(&self) -> u8 {
        match self.kind {
            PropertyKind::GetData => SUB_ID2_GET_PROPERTY_DATA,
            PropertyKind::GetDataReply => SUB_ID2_GET_PROPERTY_DATA_REPLY,
            PropertyKind::SetData => SUB_ID2_SET_PROPERTY_DATA,
            PropertyKind::SetDataReply => SUB_ID2_SET_PROPERTY_DATA_REPLY,
            PropertyKind::Subscription => SUB_ID2_SUBSCRIPTION,
            PropertyKind::SubscriptionReply => SUB_ID2_SUBSCRIPTION_REPLY,
        }
    }

    pub(super) fn encode_body(&self, out: &mut Vec<u8>) {
        out.push(self.request_id);
        push_len(out, self.header.len());
        out.extend_from_slice(&self.header);
        push_u14(out, self.num_chunks);
        push_u14(out, self.chunk);
        push_len(out, self.body.len());
        out.extend_from_slice(&self.body);
    }

    pub(super) fn decode_body(sub_id2: u8, b: &[u8]) -> Option<PropertyData> {
        let kind = match sub_id2 {
            SUB_ID2_GET_PROPERTY_DATA => PropertyKind::GetData,
            SUB_ID2_GET_PROPERTY_DATA_REPLY => PropertyKind::GetDataReply,
            SUB_ID2_SET_PROPERTY_DATA => PropertyKind::SetData,
            SUB_ID2_SET_PROPERTY_DATA_REPLY => PropertyKind::SetDataReply,
            SUB_ID2_SUBSCRIPTION => PropertyKind::Subscription,
            SUB_ID2_SUBSCRIPTION_REPLY => PropertyKind::SubscriptionReply,
            _ => return None,
        };
        let mut cur = b;
        let request_id = take(&mut cur, 1)?[0];
        let header = take_len_prefixed(&mut cur)?;
        let num_chunks = take_u14(&mut cur)?;
        let chunk = take_u14(&mut cur)?;
        let body = take_len_prefixed(&mut cur)?;
        Some(PropertyData {
            kind,
            request_id,
            header,
            num_chunks,
            chunk,
            body,
        })
    }
}

/// The `command` property carried in a Subscription message's header
/// (M2-103 §11.1, Table 39).
///
/// The sub-ID says only "this is a Subscription"; the command says what it
/// *does*, and each is restricted to one direction. Modelling that here rather
/// than leaving callers to compare strings is what makes
/// [`is_valid_from`](Self::is_valid_from) checkable — a Responder that sends
/// `Start`, or an Initiator that sends `Full`, is speaking out of turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubscriptionCommand {
    /// `"start"` — Initiator only. Creates a subscription. §11.1: "The header
    /// is identical to that used by an Inquiry: Get Property Data message. The
    /// Response does not return any Property Data."
    Start,
    /// `"partial"` — Responder only. An update to a *subset* of the subscribed
    /// data, formatted like a partial Set Property Data.
    Partial,
    /// `"full"` — Responder only. A complete set of the subscribed data,
    /// formatted like a Reply to Get Property.
    Full,
    /// `"notify"` — Responder only. Asks the Initiator to refresh by issuing
    /// its own Get Property Data. §11.1: "There is no body in this message."
    Notify,
    /// `"end"` — either side. Ends the subscription.
    End,
}

impl SubscriptionCommand {
    /// The wire string for this command (the header JSON's `command` value).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Partial => "partial",
            Self::Full => "full",
            Self::Notify => "notify",
            Self::End => "end",
        }
    }

    /// Parse a `command` value, or `None` if it names no known command.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "start" => Self::Start,
            "partial" => Self::Partial,
            "full" => Self::Full,
            "notify" => Self::Notify,
            "end" => Self::End,
            _ => return None,
        })
    }

    /// Whether this command may be sent by an Initiator (`true`) or a Responder
    /// (`false`), per the per-command direction rules in §11.1.
    ///
    /// `End` is the only command either side may send — §11.5 lets the
    /// subscription be ended "by either the Initiator or the Responder".
    pub fn is_valid_from(&self, is_initiator: bool) -> bool {
        match self {
            Self::Start => is_initiator,
            Self::Partial | Self::Full | Self::Notify => !is_initiator,
            Self::End => true,
        }
    }
}

/// Push a 14-bit value as two little-endian 7-bit bytes.
fn push_u14(out: &mut Vec<u8>, v: u16) {
    out.push((v & 0x7F) as u8);
    out.push(((v >> 7) & 0x7F) as u8);
}

/// Push a length as a 14-bit little-endian pair (same encoding as [`push_u14`]).
fn push_len(out: &mut Vec<u8>, len: usize) {
    push_u14(out, len as u16);
}

/// Split `cur.len() >= n` bytes off the front of `cur`, advancing it.
fn take<'a>(cur: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
    if cur.len() < n {
        return None;
    }
    let (head, tail) = cur.split_at(n);
    *cur = tail;
    Some(head)
}

/// Read a 14-bit little-endian value, advancing `cur`.
fn take_u14(cur: &mut &[u8]) -> Option<u16> {
    let b = take(cur, 2)?;
    Some((b[0] as u16 & 0x7F) | ((b[1] as u16 & 0x7F) << 7))
}

/// Read a 14-bit-length-prefixed byte blob, advancing `cur`.
fn take_len_prefixed(cur: &mut &[u8]) -> Option<Vec<u8>> {
    let len = take_u14(cur)? as usize;
    Some(take(cur, len)?.to_vec())
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;

    fn header() -> CiHeader {
        CiHeader {
            device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
            ci_version: CI_VERSION,
            source: Muid(0x10),
            destination: Muid(0x20),
        }
    }

    #[test]
    fn get_and_set_round_trip_over_sysex7() {
        for kind in [
            PropertyKind::GetData,
            PropertyKind::GetDataReply,
            PropertyKind::SetData,
            PropertyKind::SetDataReply,
        ] {
            let data = PropertyData {
                kind,
                request_id: 7,
                header: br#"{"resource":"DeviceInfo"}"#.to_vec(),
                num_chunks: 1,
                chunk: 1,
                body: br#"{"name":"Tutti"}"#.to_vec(),
            };
            let m = CiMessage::Property {
                header: header(),
                data: data.clone(),
            };
            let mut events = Vec::new();
            ci_to_sysex7(0, &m, &mut events);
            let back = sysex7_to_ci(&events).expect("reassembles");
            assert_eq!(back, m);
        }
    }

    #[test]
    fn subscription_round_trips_over_sysex7() {
        for kind in [PropertyKind::Subscription, PropertyKind::SubscriptionReply] {
            let data = PropertyData {
                kind,
                request_id: 3,
                header: br#"{"resource":"ChannelList","command":"start"}"#.to_vec(),
                num_chunks: 1,
                chunk: 1,
                body: Vec::new(),
            };
            let m = CiMessage::Property {
                header: header(),
                data: data.clone(),
            };
            let mut events = Vec::new();
            ci_to_sysex7(0, &m, &mut events);
            assert_eq!(sysex7_to_ci(&events).expect("reassembles"), m);
        }
    }

    #[test]
    fn subscription_uses_the_spec_sub_ids() {
        // §8.11 / §8.12 fix these at 0x38 and 0x39. The wire body is identical
        // to Get/Set Property Data, so the sub-ID is the *only* thing that
        // distinguishes a subscription from an ordinary property exchange —
        // assert the byte rather than trusting a round-trip through our own
        // encoder, which would agree with itself either way.
        let sub = PropertyData {
            kind: PropertyKind::Subscription,
            request_id: 0,
            header: Vec::new(),
            num_chunks: 1,
            chunk: 1,
            body: Vec::new(),
        };
        assert_eq!(sub.sub_id2(), 0x38);
        assert_eq!(
            PropertyData {
                kind: PropertyKind::SubscriptionReply,
                ..sub
            }
            .sub_id2(),
            0x39
        );
    }

    #[test]
    fn subscription_commands_round_trip_through_their_wire_strings() {
        for cmd in [
            SubscriptionCommand::Start,
            SubscriptionCommand::Partial,
            SubscriptionCommand::Full,
            SubscriptionCommand::Notify,
            SubscriptionCommand::End,
        ] {
            assert_eq!(SubscriptionCommand::parse(cmd.as_str()), Some(cmd));
        }
        assert_eq!(SubscriptionCommand::parse("subscribe"), None);
        // The strings are the wire contract from M2-103 Table 39, so pin them.
        assert_eq!(SubscriptionCommand::Start.as_str(), "start");
        assert_eq!(SubscriptionCommand::Partial.as_str(), "partial");
        assert_eq!(SubscriptionCommand::Full.as_str(), "full");
        assert_eq!(SubscriptionCommand::Notify.as_str(), "notify");
        assert_eq!(SubscriptionCommand::End.as_str(), "end");
    }

    #[test]
    fn each_command_is_restricted_to_its_direction() {
        // M2-103 §11.1 marks start "Initiator only" and partial/full/notify
        // "Responder only"; §11.5 lets either side send end. A device that
        // ignores this talks out of turn — a Responder cannot start its own
        // subscription, and an Initiator cannot push updates (the note under
        // §8.11: an Initiator "shall not send updates … but shall send updates
        // … using an Inquiry: Set Property Data message instead").
        let initiator = true;
        let responder = false;
        assert!(SubscriptionCommand::Start.is_valid_from(initiator));
        assert!(!SubscriptionCommand::Start.is_valid_from(responder));
        for cmd in [
            SubscriptionCommand::Partial,
            SubscriptionCommand::Full,
            SubscriptionCommand::Notify,
        ] {
            assert!(cmd.is_valid_from(responder), "{cmd:?} is responder-only");
            assert!(!cmd.is_valid_from(initiator), "{cmd:?} is responder-only");
        }
        assert!(SubscriptionCommand::End.is_valid_from(initiator));
        assert!(SubscriptionCommand::End.is_valid_from(responder));
    }

    #[test]
    fn empty_header_and_body_round_trip() {
        let data = PropertyData {
            kind: PropertyKind::GetData,
            request_id: 0,
            header: Vec::new(),
            num_chunks: 1,
            chunk: 1,
            body: Vec::new(),
        };
        let m = CiMessage::Property {
            header: header(),
            data,
        };
        assert_eq!(CiMessage::decode(&m.encode()).unwrap(), m);
    }
}
