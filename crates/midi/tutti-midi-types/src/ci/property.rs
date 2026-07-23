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

/// `true` if `sub_id2` belongs to the Property Exchange family (this subset).
pub(super) fn is_property_sub_id2(sub_id2: u8) -> bool {
    matches!(
        sub_id2,
        SUB_ID2_GET_PROPERTY_DATA
            | SUB_ID2_GET_PROPERTY_DATA_REPLY
            | SUB_ID2_SET_PROPERTY_DATA
            | SUB_ID2_SET_PROPERTY_DATA_REPLY
    )
}

/// Which Property Exchange message this is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropertyKind {
    GetData,
    GetDataReply,
    SetData,
    SetDataReply,
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
