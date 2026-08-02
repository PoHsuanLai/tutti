//! MIDI-CI Profile Configuration (M2-101 §6).
//!
//! Profiles are named, standardized behaviors (e.g. "General MIDI 2", "MPE") a
//! device can expose and a peer can enable. The message family is: **Profile
//! Inquiry** → **Profile Inquiry Reply** (the enabled/disabled profile lists),
//! then **Set Profile On/Off** and their **Enabled/Disabled Report**
//! notifications.

use std::vec::Vec;

/// Sub-ID#2: Profile Inquiry.
pub const SUB_ID2_PROFILE_INQUIRY: u8 = 0x20;
/// Sub-ID#2: Profile Inquiry Reply.
pub const SUB_ID2_PROFILE_INQUIRY_REPLY: u8 = 0x21;
/// Sub-ID#2: Set Profile On.
pub const SUB_ID2_SET_PROFILE_ON: u8 = 0x22;
/// Sub-ID#2: Set Profile Off.
pub const SUB_ID2_SET_PROFILE_OFF: u8 = 0x23;
/// Sub-ID#2: Profile Enabled Report.
pub const SUB_ID2_PROFILE_ENABLED: u8 = 0x24;
/// Sub-ID#2: Profile Disabled Report.
pub const SUB_ID2_PROFILE_DISABLED: u8 = 0x25;
/// Sub-ID#2: Profile Added Report (M2-101 §7.4).
pub const SUB_ID2_PROFILE_ADDED: u8 = 0x26;
/// Sub-ID#2: Profile Removed Report (M2-101 §7.5).
pub const SUB_ID2_PROFILE_REMOVED: u8 = 0x27;
/// Sub-ID#2: Profile Details Inquiry (M2-101 §7.6).
pub const SUB_ID2_PROFILE_DETAILS_INQUIRY: u8 = 0x28;
/// Sub-ID#2: Reply to Profile Details Inquiry (M2-101 §7.7).
pub const SUB_ID2_PROFILE_DETAILS_REPLY: u8 = 0x29;
/// Sub-ID#2: Profile Specific Data (M2-101 §7.12).
pub const SUB_ID2_PROFILE_SPECIFIC_DATA: u8 = 0x2F;

/// `true` if `sub_id2` belongs to the Profile Configuration family.
pub(super) fn is_profile_sub_id2(sub_id2: u8) -> bool {
    matches!(
        sub_id2,
        SUB_ID2_PROFILE_INQUIRY
            | SUB_ID2_PROFILE_INQUIRY_REPLY
            | SUB_ID2_SET_PROFILE_ON
            | SUB_ID2_SET_PROFILE_OFF
            | SUB_ID2_PROFILE_ENABLED
            | SUB_ID2_PROFILE_DISABLED
            | SUB_ID2_PROFILE_ADDED
            | SUB_ID2_PROFILE_REMOVED
            | SUB_ID2_PROFILE_DETAILS_INQUIRY
            | SUB_ID2_PROFILE_DETAILS_REPLY
            | SUB_ID2_PROFILE_SPECIFIC_DATA
    )
}

/// A 5-byte Profile Id (M2-101 §6.5): a standardized or manufacturer-specific
/// profile identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProfileId(pub [u8; 5]);

/// A Profile Configuration message body. `Inquiry` has no body; the reply lists
/// the enabled and disabled profiles; the set/report variants each carry one
/// profile id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProfileState {
    /// Ask a device which profiles it exposes (M2-101 §6.2). No body.
    Inquiry,
    /// The device's profile lists (M2-101 §6.3).
    InquiryReply {
        enabled: Vec<ProfileId>,
        disabled: Vec<ProfileId>,
    },
    /// Request a profile be turned on (M2-101 §6.6).
    SetOn(ProfileId),
    /// Request a profile be turned off (M2-101 §6.7).
    SetOff(ProfileId),
    /// Notification that a profile is now enabled (M2-101 §6.8).
    Enabled(ProfileId),
    /// Notification that a profile is now disabled (M2-101 §6.9).
    Disabled(ProfileId),
    /// A profile this device newly supports (M2-101 §7.4).
    ///
    /// §7.4 makes this a broadcast: "The Profile Added Report message shall
    /// have the Destination MUID set to Broadcast (0x7F 7F 7F 7F)" — see
    /// [`Muid::BROADCAST`](super::Muid::BROADCAST). It also requires that if
    /// the added profile is already enabled, a Profile Enabled Report follow
    /// "immediately following the Profile Added Report".
    Added(ProfileId),
    /// A profile this device no longer supports (M2-101 §7.5). Broadcast, like
    /// [`Added`](Self::Added).
    Removed(ProfileId),
    /// Ask what a responder's implementation of a profile supports — channel
    /// counts, optional features (M2-101 §7.6).
    DetailsInquiry {
        profile: ProfileId,
        /// What class of detail is being asked for. §7.6.1 splits the range:
        /// `0x00..=0x3F` is Registered Target Data with one format across all
        /// profiles, `0x40..=0x7F` is Profile Specific Target Data whose
        /// meaning each profile specification defines for itself.
        target: u8,
    },
    /// The answer to a [`DetailsInquiry`](Self::DetailsInquiry) (M2-101 §7.7).
    ///
    /// §7.7.1: the target "shall be the same as in the Profile Details Inquiry
    /// message which was received", so it is echoed rather than re-chosen.
    DetailsReply {
        profile: ProfileId,
        target: u8,
        /// Opaque reply data; its format is defined by the profile spec or by
        /// M2-102 for registered targets, not by this codec.
        data: Vec<u8>,
    },
    /// Profile-defined data for an already-negotiated profile (M2-101 §7.12).
    /// Either side may send it.
    SpecificData { profile: ProfileId, data: Vec<u8> },
}

impl ProfileState {
    /// The sub-ID#2 that carries this state on the wire.
    pub(super) fn sub_id2(&self) -> u8 {
        match self {
            ProfileState::Inquiry => SUB_ID2_PROFILE_INQUIRY,
            ProfileState::InquiryReply { .. } => SUB_ID2_PROFILE_INQUIRY_REPLY,
            ProfileState::SetOn(_) => SUB_ID2_SET_PROFILE_ON,
            ProfileState::SetOff(_) => SUB_ID2_SET_PROFILE_OFF,
            ProfileState::Enabled(_) => SUB_ID2_PROFILE_ENABLED,
            ProfileState::Disabled(_) => SUB_ID2_PROFILE_DISABLED,
            ProfileState::Added(_) => SUB_ID2_PROFILE_ADDED,
            ProfileState::Removed(_) => SUB_ID2_PROFILE_REMOVED,
            ProfileState::DetailsInquiry { .. } => SUB_ID2_PROFILE_DETAILS_INQUIRY,
            ProfileState::DetailsReply { .. } => SUB_ID2_PROFILE_DETAILS_REPLY,
            ProfileState::SpecificData { .. } => SUB_ID2_PROFILE_SPECIFIC_DATA,
        }
    }

    pub(super) fn encode_body(&self, out: &mut Vec<u8>) {
        let push_id = |out: &mut Vec<u8>, id: &ProfileId| out.extend_from_slice(&id.0);
        match self {
            ProfileState::Inquiry => {}
            ProfileState::InquiryReply { enabled, disabled } => {
                push_count(out, enabled.len());
                for id in enabled {
                    push_id(out, id);
                }
                push_count(out, disabled.len());
                for id in disabled {
                    push_id(out, id);
                }
            }
            ProfileState::SetOn(id)
            | ProfileState::SetOff(id)
            | ProfileState::Enabled(id)
            | ProfileState::Disabled(id)
            | ProfileState::Added(id)
            | ProfileState::Removed(id) => push_id(out, id),
            ProfileState::DetailsInquiry { profile, target } => {
                push_id(out, profile);
                out.push(target & 0x7F);
            }
            ProfileState::DetailsReply {
                profile,
                target,
                data,
            } => {
                push_id(out, profile);
                out.push(target & 0x7F);
                // §7.7.2: "two bytes, LSB first".
                push_count(out, data.len());
                out.extend_from_slice(data);
            }
            ProfileState::SpecificData { profile, data } => {
                push_id(out, profile);
                // §7.12 Table 28 gives this length **four** bytes, not the two
                // every other length in this family uses. Reusing push_count
                // here would encode a body no conformant reader can parse.
                push_len32(out, data.len());
                out.extend_from_slice(data);
            }
        }
    }

    pub(super) fn decode_body(sub_id2: u8, b: &[u8]) -> Option<ProfileState> {
        let read_id =
            |b: &[u8]| -> Option<ProfileId> { Some(ProfileId(b.get(..5)?.try_into().ok()?)) };
        match sub_id2 {
            SUB_ID2_PROFILE_INQUIRY => Some(ProfileState::Inquiry),
            SUB_ID2_PROFILE_INQUIRY_REPLY => {
                let (enabled, rest) = read_profile_list(b)?;
                let (disabled, _) = read_profile_list(rest)?;
                Some(ProfileState::InquiryReply { enabled, disabled })
            }
            SUB_ID2_SET_PROFILE_ON => Some(ProfileState::SetOn(read_id(b)?)),
            SUB_ID2_SET_PROFILE_OFF => Some(ProfileState::SetOff(read_id(b)?)),
            SUB_ID2_PROFILE_ENABLED => Some(ProfileState::Enabled(read_id(b)?)),
            SUB_ID2_PROFILE_DISABLED => Some(ProfileState::Disabled(read_id(b)?)),
            SUB_ID2_PROFILE_ADDED => Some(ProfileState::Added(read_id(b)?)),
            SUB_ID2_PROFILE_REMOVED => Some(ProfileState::Removed(read_id(b)?)),
            SUB_ID2_PROFILE_DETAILS_INQUIRY => Some(ProfileState::DetailsInquiry {
                profile: read_id(b)?,
                target: *b.get(5)?,
            }),
            SUB_ID2_PROFILE_DETAILS_REPLY => {
                let profile = read_id(b)?;
                let target = *b.get(5)?;
                let len = read_count(b.get(6..8)?)?;
                Some(ProfileState::DetailsReply {
                    profile,
                    target,
                    data: b.get(8..8 + len)?.to_vec(),
                })
            }
            SUB_ID2_PROFILE_SPECIFIC_DATA => {
                let profile = read_id(b)?;
                let len = read_len32(b.get(5..9)?)?;
                Some(ProfileState::SpecificData {
                    profile,
                    data: b.get(9..9 + len)?.to_vec(),
                })
            }
            _ => None,
        }
    }
}

/// Push a length as four little-endian 7-bit bytes (M2-101 §7.12, Table 28).
fn push_len32(out: &mut Vec<u8>, n: usize) {
    for shift in [0, 7, 14, 21] {
        out.push(((n >> shift) & 0x7F) as u8);
    }
}

/// Read a 4-byte little-endian 7-bit length.
fn read_len32(b: &[u8]) -> Option<usize> {
    let b: [u8; 4] = b.try_into().ok()?;
    Some(
        b.iter()
            .enumerate()
            .map(|(i, v)| ((*v as usize) & 0x7F) << (7 * i))
            .sum(),
    )
}

/// Read a 2-byte little-endian 7-bit count (the inverse of [`push_count`]).
fn read_count(b: &[u8]) -> Option<usize> {
    Some((*b.first()? as usize & 0x7F) | ((*b.get(1)? as usize & 0x7F) << 7))
}

/// Push a 14-bit profile count as two little-endian 7-bit bytes (M2-101 §6.3).
fn push_count(out: &mut Vec<u8>, n: usize) {
    out.push((n & 0x7F) as u8);
    out.push(((n >> 7) & 0x7F) as u8);
}

/// Read a length-prefixed profile-id list, returning it and the remaining bytes.
fn read_profile_list(b: &[u8]) -> Option<(Vec<ProfileId>, &[u8])> {
    if b.len() < 2 {
        return None;
    }
    let count = (b[0] as usize & 0x7F) | ((b[1] as usize & 0x7F) << 7);
    let mut rest = &b[2..];
    let mut ids = Vec::with_capacity(count);
    for _ in 0..count {
        let id = ProfileId(rest.get(..5)?.try_into().ok()?);
        ids.push(id);
        rest = &rest[5..];
    }
    Some((ids, rest))
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;
    use tutti_types::MidiGroup;

    fn header() -> CiHeader {
        CiHeader {
            device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
            ci_version: CI_VERSION,
            source: Muid(0x10),
            destination: Muid(0x20),
        }
    }

    fn msg(state: ProfileState) -> CiMessage {
        CiMessage::Profile {
            header: header(),
            state,
        }
    }

    #[test]
    fn inquiry_round_trips() {
        let m = msg(ProfileState::Inquiry);
        assert_eq!(CiMessage::decode(&m.encode()).unwrap(), m);
    }

    #[test]
    fn inquiry_reply_lists_round_trip() {
        let m = msg(ProfileState::InquiryReply {
            enabled: vec![ProfileId([0x7E, 1, 2, 3, 4]), ProfileId([0x7E, 5, 6, 7, 8])],
            disabled: vec![ProfileId([0x00, 9, 10, 11, 12])],
        });
        let back = CiMessage::decode(&m.encode()).expect("decodes");
        assert_eq!(back, m);
    }

    #[test]
    fn added_and_removed_reports_round_trip() {
        for state in [
            ProfileState::Added(ProfileId([0x7E, 1, 2, 3, 4])),
            ProfileState::Removed(ProfileId([0x7E, 1, 2, 3, 4])),
        ] {
            let m = msg(state);
            assert_eq!(CiMessage::decode(&m.encode()).unwrap(), m);
        }
    }

    #[test]
    fn added_and_removed_use_the_spec_sub_ids() {
        // Their bodies are a bare profile id, identical to Enabled/Disabled —
        // so the sub-ID is the only thing distinguishing "I gained this
        // profile" from "I turned it on", two very different claims.
        let id = ProfileId([1, 2, 3, 4, 5]);
        let sub_id = |s: ProfileState| msg(s).encode()[3];
        assert_eq!(sub_id(ProfileState::Added(id)), 0x26);
        assert_eq!(sub_id(ProfileState::Removed(id)), 0x27);
        assert_eq!(sub_id(ProfileState::Enabled(id)), 0x24);
        assert_eq!(sub_id(ProfileState::Disabled(id)), 0x25);
    }

    #[test]
    fn details_inquiry_and_reply_round_trip() {
        let profile = ProfileId([0x7E, 9, 8, 7, 6]);
        let inquiry = msg(ProfileState::DetailsInquiry {
            profile,
            target: 0x01,
        });
        assert_eq!(CiMessage::decode(&inquiry.encode()).unwrap(), inquiry);

        let reply = msg(ProfileState::DetailsReply {
            profile,
            target: 0x01,
            data: vec![4, 0, 16],
        });
        assert_eq!(CiMessage::decode(&reply.encode()).unwrap(), reply);
    }

    #[test]
    fn details_target_survives_both_halves_of_its_range() {
        // §7.6.1 splits the target byte: 0x00-0x3F is Registered Target Data
        // (one format across all profiles), 0x40-0x7F is Profile Specific. The
        // codec must carry either untouched — masking to 6 bits, say, would
        // silently turn a profile-specific target into a registered one.
        for target in [0x00, 0x3F, 0x40, 0x7F] {
            let m = msg(ProfileState::DetailsInquiry {
                profile: ProfileId([1, 2, 3, 4, 5]),
                target,
            });
            match CiMessage::decode(&m.encode()).unwrap() {
                CiMessage::Profile {
                    state: ProfileState::DetailsInquiry { target: got, .. },
                    ..
                } => assert_eq!(got, target),
                other => panic!("expected DetailsInquiry, got {other:?}"),
            }
        }
    }

    #[test]
    fn specific_data_uses_a_four_byte_length() {
        // §7.12 Table 28 gives this length four bytes, unlike the two every
        // other length in this family uses. Assert the byte positions: a
        // 2-byte length here would round-trip through our own codec happily
        // and be unparseable by anything else.
        let m = msg(ProfileState::SpecificData {
            profile: ProfileId([1, 2, 3, 4, 5]),
            data: vec![0xA, 0xB, 0xC],
        });
        let bytes = m.encode();
        let body_at = bytes.len() - 5 - 4 - 3; // profile + len32 + data
        assert_eq!(&bytes[body_at..body_at + 5], &[1, 2, 3, 4, 5]);
        assert_eq!(
            &bytes[body_at + 5..body_at + 9],
            &[3, 0, 0, 0],
            "length is four LSB-first 7-bit bytes"
        );
        assert_eq!(CiMessage::decode(&bytes).unwrap(), m);
    }

    #[test]
    fn specific_data_round_trips_a_payload_past_the_7bit_boundary() {
        // 200 bytes needs the second length byte, which a 1-byte length or a
        // careless mask would drop.
        let m = msg(ProfileState::SpecificData {
            profile: ProfileId([1, 2, 3, 4, 5]),
            data: (0..200u8).map(|b| b & 0x7F).collect(),
        });
        assert_eq!(CiMessage::decode(&m.encode()).unwrap(), m);
    }

    #[test]
    fn set_and_report_round_trip_over_sysex7() {
        for state in [
            ProfileState::SetOn(ProfileId([1, 2, 3, 4, 5])),
            ProfileState::SetOff(ProfileId([1, 2, 3, 4, 5])),
            ProfileState::Enabled(ProfileId([1, 2, 3, 4, 5])),
            ProfileState::Disabled(ProfileId([1, 2, 3, 4, 5])),
        ] {
            let m = msg(state);
            let mut events = Vec::new();
            ci_to_sysex7(MidiGroup::FIRST, &m, &mut events);
            assert_eq!(sysex7_to_ci(&events).expect("reassembles"), m);
        }
    }
}
