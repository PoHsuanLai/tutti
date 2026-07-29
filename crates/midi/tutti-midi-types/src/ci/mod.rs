//! MIDI Capability Inquiry (MIDI-CI, M2-101).
//!
//! MIDI-CI lets two MIDI devices negotiate shared capabilities — discover each
//! other, enable *Profiles*, and exchange *Properties* — over Universal System
//! Exclusive messages (sub-ID#1 `0x0D`). It is **not** a UMP message type: a CI
//! message is a byte payload carried inside a SysEx, so tutti transports it over
//! the existing SysEx7 fragmenter ([`crate::MidiEvent::sysex7_fragments`]).
//!
//! `midi2` v0.11 does not help here — its `ci` module is a WIP stub with no
//! usable message builders — so this layer is hand-rolled from the M2-101 byte
//! format:
//!
//! ```text
//!   7E <device_id> 0D <sub_id2> <ci_ver> <src_muid:4> <dst_muid:4> <body…>
//! ```
//!
//! Layering mirrors the UMP-Stream endpoint work: this module owns the *message
//! codec* (encode/decode CI bytes), while the *negotiator* state machine lives in
//! `tutti-midi-runtime`. The families split into submodules:
//! [`discovery`] (the mandatory core), [`profile`], [`property`].

use std::vec::Vec;

use crate::MidiEvent;

pub mod discovery;
pub mod profile;
pub mod property;

pub use discovery::{CiCategories, DiscoveryData, Nak};
pub use profile::{ProfileId, ProfileState};
pub use property::{PropertyCapabilities, PropertyData, PropertyKind, SubscriptionCommand};

/// Universal SysEx real-time/non-real-time id for MIDI-CI: `0x7E`
/// (non-real-time). Every CI message begins with it.
pub const CI_UNIVERSAL_SYSEX: u8 = 0x7E;

/// Sub-ID#1 identifying a MIDI-CI message (M2-101 §5.1).
pub const CI_SUB_ID_1: u8 = 0x0D;

/// The MIDI-CI Message Format Version this implementation speaks: **0x02**
/// (MIDI-CI v1.2).
///
/// M2-101 §5.2: "In this version 1.2 of the MIDI-CI Specification, the version
/// number is 0x02." §5.3 requires a device "always use its own Message Format
/// Version", so this may only be raised alongside the message bodies — which now
/// carry every Version-2 field: Discovery's Output Path ID (Table 6), Reply's
/// Output Path Instance ID + Function Block (Table 8), and NAK's details,
/// message length and text (Table 15).
///
/// Older peers still interoperate: §5.4 requires a receiver to "process the
/// fields, values, and bits defined in the received version" when it is lower
/// than its own, and our decoders default each absent Version-2 field rather
/// than rejecting the message.
///
/// Changing this value at runtime is not a free edit — §5.3: "If a Device wishes
/// to change to sending a different Message Format Version, the Device shall
/// invalidate its MUID and initiate a new Discovery Transaction."
pub const CI_VERSION: u8 = 0x02;

/// The "whole device" destination for the device-id byte and for broadcast
/// MUIDs (M2-101 §5.1).
pub const CI_DEVICE_ID_FUNCTION_BLOCK: u8 = 0x7F;

/// A 28-bit **MIDI Unique Identifier** (M2-101 §5.1) — the random address a CI
/// device assigns itself for the session. Serialized little-endian as four 7-bit
/// bytes on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Muid(pub u32);

impl Muid {
    /// The broadcast MUID `0x0FFF_FFFF` — addresses every device (M2-101 §5.1).
    pub const BROADCAST: Muid = Muid(0x0FFF_FFFF);

    /// Derive a MUID from a caller-supplied 32-bit seed, masked to 28 bits. The
    /// engine forbids `rand`/clock access, so randomness is the caller's to
    /// provide (e.g. hash a device name + a session nonce). The broadcast value
    /// is nudged aside so a seed never collides with it.
    pub fn from_seed(seed: u32) -> Self {
        let v = seed & 0x0FFF_FFFF;
        if v == Self::BROADCAST.0 {
            Muid(v ^ 0x1)
        } else {
            Muid(v)
        }
    }

    /// The four little-endian 7-bit wire bytes.
    #[inline]
    pub fn to_bytes(self) -> [u8; 4] {
        [
            (self.0 & 0x7F) as u8,
            ((self.0 >> 7) & 0x7F) as u8,
            ((self.0 >> 14) & 0x7F) as u8,
            ((self.0 >> 21) & 0x7F) as u8,
        ]
    }

    /// Reassemble from four little-endian 7-bit wire bytes.
    #[inline]
    pub fn from_bytes(b: [u8; 4]) -> Self {
        Muid(
            (b[0] as u32 & 0x7F)
                | ((b[1] as u32 & 0x7F) << 7)
                | ((b[2] as u32 & 0x7F) << 14)
                | ((b[3] as u32 & 0x7F) << 21),
        )
    }
}

/// The fixed MIDI-CI preamble shared by every message (M2-101 §5.1): the source
/// and destination MUIDs, the CI version, and the device-id byte that scopes the
/// message. The sub-ID#2 (message type) is **not** stored here — it is fully
/// determined by the [`CiMessage`] variant, so it's derived on encode and used to
/// dispatch on decode rather than duplicated in the header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CiHeader {
    /// Device-id byte: a channel `0x00..=0x0F`, or [`CI_DEVICE_ID_FUNCTION_BLOCK`].
    pub device_id: u8,
    /// CI message version (normally [`CI_VERSION`]).
    pub ci_version: u8,
    pub source: Muid,
    pub destination: Muid,
}

impl CiHeader {
    /// Length of the encoded preamble in bytes: `7E id 0D sub2 ver` + 2×4 MUID.
    pub const LEN: usize = 5 + 4 + 4;

    /// Append the preamble bytes to `out`, stamping the given `sub_id2`.
    fn encode(&self, sub_id2: u8, out: &mut Vec<u8>) {
        out.push(CI_UNIVERSAL_SYSEX);
        out.push(self.device_id);
        out.push(CI_SUB_ID_1);
        out.push(sub_id2);
        out.push(self.ci_version);
        out.extend_from_slice(&self.source.to_bytes());
        out.extend_from_slice(&self.destination.to_bytes());
    }

    /// Parse the preamble from the front of `bytes`, returning the header, its
    /// sub-ID#2, and the offset where the body begins — or `None` if `bytes`
    /// isn't a CI message.
    fn decode(bytes: &[u8]) -> Option<(CiHeader, u8, usize)> {
        if bytes.len() < Self::LEN {
            return None;
        }
        if bytes[0] != CI_UNIVERSAL_SYSEX || bytes[2] != CI_SUB_ID_1 {
            return None;
        }
        let header = CiHeader {
            device_id: bytes[1],
            ci_version: bytes[4],
            source: Muid::from_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]),
            destination: Muid::from_bytes([bytes[9], bytes[10], bytes[11], bytes[12]]),
        };
        Some((header, bytes[3], Self::LEN))
    }
}

/// A decoded MIDI-CI message: its [`CiHeader`] plus a typed body. Non-exhaustive
/// so new families (added under [`profile`] / [`property`]) don't break matches.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CiMessage {
    /// Discovery / Discovery Reply — the initial handshake advertising identity
    /// and capabilities (M2-101 §5.2). `is_reply` distinguishes the two.
    Discovery {
        header: CiHeader,
        is_reply: bool,
        data: DiscoveryData,
    },
    /// Invalidate MUID — tells a device its MUID collided and it must pick a new
    /// one (M2-101 §5.5). Carries the target MUID.
    InvalidateMuid { header: CiHeader, target: Muid },
    /// NAK — a device rejects a CI message it can't handle (M2-101 §5.6).
    Nak { header: CiHeader, nak: Nak },
    /// Profile Configuration message (inquiry/reply/enable/disable) — see [`profile`].
    Profile {
        header: CiHeader,
        state: ProfileState,
    },
    /// Property Exchange message (get/set data) — see [`property`].
    Property {
        header: CiHeader,
        data: PropertyData,
    },
    /// Property Exchange **Capabilities** inquiry or reply (M2-101 §8.4/§8.6).
    ///
    /// A separate variant rather than another [`PropertyKind`]: it shares the
    /// 0x30-0x3F category but not the chunked body, carrying three scalars
    /// instead. `is_reply` distinguishes 0x30 from 0x31.
    PropertyCapabilities {
        header: CiHeader,
        is_reply: bool,
        data: property::PropertyCapabilities,
    },
}

impl CiMessage {
    /// The message's preamble.
    pub fn header(&self) -> &CiHeader {
        match self {
            CiMessage::Discovery { header, .. }
            | CiMessage::InvalidateMuid { header, .. }
            | CiMessage::Nak { header, .. }
            | CiMessage::Profile { header, .. }
            | CiMessage::Property { header, .. }
            | CiMessage::PropertyCapabilities { header, .. } => header,
        }
    }

    /// Encode the full CI byte payload (the bytes carried *inside* a SysEx,
    /// starting at `0x7E`, without the 0xF0/0xF7 delimiters).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            CiMessage::Discovery {
                header,
                is_reply,
                data,
            } => {
                let sub_id2 = if *is_reply {
                    discovery::SUB_ID2_DISCOVERY_REPLY
                } else {
                    discovery::SUB_ID2_DISCOVERY
                };
                header.encode(sub_id2, &mut out);
                data.encode_body(*is_reply, &mut out);
            }
            CiMessage::InvalidateMuid { header, target } => {
                header.encode(discovery::SUB_ID2_INVALIDATE_MUID, &mut out);
                out.extend_from_slice(&target.to_bytes());
            }
            CiMessage::Nak { header, nak } => {
                header.encode(discovery::SUB_ID2_NAK, &mut out);
                nak.encode_body(&mut out);
            }
            CiMessage::Profile { header, state } => {
                header.encode(state.sub_id2(), &mut out);
                state.encode_body(&mut out);
            }
            CiMessage::Property { header, data } => {
                header.encode(data.sub_id2(), &mut out);
                data.encode_body(&mut out);
            }
            CiMessage::PropertyCapabilities {
                header,
                is_reply,
                data,
            } => {
                let sub_id2 = if *is_reply {
                    property::SUB_ID2_PE_CAPABILITIES_REPLY
                } else {
                    property::SUB_ID2_PE_CAPABILITIES
                };
                header.encode(sub_id2, &mut out);
                data.encode_body(&mut out);
            }
        }
        out
    }

    /// Decode a CI byte payload (as produced by [`encode`](Self::encode) or
    /// reassembled by [`sysex7_to_ci`]) into a typed message, or `None` if the
    /// bytes aren't a CI message this layer models.
    pub fn decode(bytes: &[u8]) -> Option<CiMessage> {
        let (header, sub_id2, body_at) = CiHeader::decode(bytes)?;
        let body = &bytes[body_at..];
        match sub_id2 {
            discovery::SUB_ID2_DISCOVERY => Some(CiMessage::Discovery {
                header,
                is_reply: false,
                data: DiscoveryData::decode_body(false, body)?,
            }),
            discovery::SUB_ID2_DISCOVERY_REPLY => Some(CiMessage::Discovery {
                header,
                is_reply: true,
                data: DiscoveryData::decode_body(true, body)?,
            }),
            discovery::SUB_ID2_INVALIDATE_MUID => {
                let b: [u8; 4] = body.get(..4)?.try_into().ok()?;
                Some(CiMessage::InvalidateMuid {
                    header,
                    target: Muid::from_bytes(b),
                })
            }
            discovery::SUB_ID2_NAK => Some(CiMessage::Nak {
                header,
                nak: Nak::decode_body(body)?,
            }),
            s if profile::is_profile_sub_id2(s) => Some(CiMessage::Profile {
                header,
                state: ProfileState::decode_body(s, body)?,
            }),
            property::SUB_ID2_PE_CAPABILITIES => Some(CiMessage::PropertyCapabilities {
                header,
                is_reply: false,
                data: property::PropertyCapabilities::decode_body(body)?,
            }),
            property::SUB_ID2_PE_CAPABILITIES_REPLY => Some(CiMessage::PropertyCapabilities {
                header,
                is_reply: true,
                data: property::PropertyCapabilities::decode_body(body)?,
            }),
            s if property::is_property_sub_id2(s) => Some(CiMessage::Property {
                header,
                data: PropertyData::decode_body(s, body)?,
            }),
            _ => None,
        }
    }
}

/// Encode `message` and fragment it onto `out` as SysEx7 packets on `group`
/// (the CI transport). Thin wrapper over [`MidiEvent::sysex7_fragments`].
pub fn ci_to_sysex7(group: u8, message: &CiMessage, out: &mut Vec<MidiEvent>) {
    let body = message.encode();
    MidiEvent::sysex7_fragments(group, &body, out);
}

/// Reassemble a run of SysEx7 packets (as emitted by [`ci_to_sysex7`]) back into
/// a [`CiMessage`], or `None` if they don't form one. Concatenates each packet's
/// payload via [`MidiEvent::sysex7_payload`] then decodes.
pub fn sysex7_to_ci(events: &[MidiEvent]) -> Option<CiMessage> {
    let mut body = Vec::new();
    for ev in events {
        let (_status, bytes, n) = ev.sysex7_payload()?;
        body.extend_from_slice(&bytes[..n]);
    }
    CiMessage::decode(&body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn muid_round_trips_through_wire_bytes() {
        for raw in [0u32, 1, 0x0123_4567, 0x0FFF_FFFE] {
            let m = Muid(raw & 0x0FFF_FFFF);
            assert_eq!(Muid::from_bytes(m.to_bytes()), m);
        }
    }

    #[test]
    fn from_seed_masks_to_28_bits_and_dodges_broadcast() {
        assert_eq!(Muid::from_seed(0xFFFF_FFFF).0 & !0x0FFF_FFFF, 0);
        // A seed that would land on broadcast is nudged aside.
        assert_ne!(Muid::from_seed(0x0FFF_FFFF), Muid::BROADCAST);
    }

    #[test]
    fn header_round_trips() {
        let h = CiHeader {
            device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
            ci_version: CI_VERSION,
            source: Muid(0x0123_4567),
            destination: Muid::BROADCAST,
        };
        let mut bytes = Vec::new();
        h.encode(discovery::SUB_ID2_DISCOVERY, &mut bytes);
        let (back, sub_id2, at) = CiHeader::decode(&bytes).expect("decodes");
        assert_eq!(at, CiHeader::LEN);
        assert_eq!(sub_id2, discovery::SUB_ID2_DISCOVERY);
        assert_eq!(back, h);
    }

    #[test]
    fn invalidate_muid_round_trips_over_sysex7() {
        let msg = CiMessage::InvalidateMuid {
            header: CiHeader {
                device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
                ci_version: CI_VERSION,
                source: Muid(0x1),
                destination: Muid::BROADCAST,
            },
            target: Muid(0x0ABC_DEF0 & 0x0FFF_FFFF),
        };
        let mut events = Vec::new();
        ci_to_sysex7(0, &msg, &mut events);
        let back = sysex7_to_ci(&events).expect("reassembles");
        assert_eq!(back, msg);
    }

    #[test]
    fn non_ci_sysex_is_rejected() {
        // A random SysEx7 (not starting 0x7E … 0x0D) is not a CI message.
        let mut events = Vec::new();
        MidiEvent::sysex7_fragments(0, &[0x01, 0x02, 0x03], &mut events);
        assert!(sysex7_to_ci(&events).is_none());
    }
}
