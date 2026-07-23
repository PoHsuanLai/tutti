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
            | ProfileState::Disabled(id) => push_id(out, id),
        }
    }

    pub(super) fn decode_body(sub_id2: u8, b: &[u8]) -> Option<ProfileState> {
        let read_id = |b: &[u8]| -> Option<ProfileId> {
            Some(ProfileId(b.get(..5)?.try_into().ok()?))
        };
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
            _ => None,
        }
    }
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
    fn set_and_report_round_trip_over_sysex7() {
        for state in [
            ProfileState::SetOn(ProfileId([1, 2, 3, 4, 5])),
            ProfileState::SetOff(ProfileId([1, 2, 3, 4, 5])),
            ProfileState::Enabled(ProfileId([1, 2, 3, 4, 5])),
            ProfileState::Disabled(ProfileId([1, 2, 3, 4, 5])),
        ] {
            let m = msg(state);
            let mut events = Vec::new();
            ci_to_sysex7(0, &m, &mut events);
            assert_eq!(sysex7_to_ci(&events).expect("reassembles"), m);
        }
    }
}
