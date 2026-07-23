//! MIDI-CI negotiation state machines (M2-101).
//!
//! The message codec lives in [`tutti_midi_types::ci`]; this module is the
//! *behavior* around it, mirroring the two halves of the UMP-Stream endpoint
//! work in [`crate::endpoint`]:
//!
//! - [`CiResponder`] — the answering side. Pure `config-in → messages-out`:
//!   given an inbound [`CiMessage`], it returns the replies (Discovery Reply,
//!   Profile lists, Property data, or a NAK for anything unsupported).
//! - [`CiInitiator`] — the inquiring side. Builds a Discovery, ingests replies
//!   into a [`DiscoveredCiDevice`], and — the one bit of real protocol state —
//!   detects a MUID collision (a reply bearing *our* MUID) and emits an
//!   Invalidate MUID.
//!
//! Both are pure and loopback-testable: a `CiInitiator`'s Discovery fed to a
//! `CiResponder`, whose reply is fed back, reconstructs the responder's identity.
//! No transport, no inbound routing — that wiring is the ECS layer's job.

use tutti_midi_types::ci::{
    CiCategories, CiHeader, CiMessage, DiscoveryData, Muid, Nak, ProfileId, ProfileState,
    PropertyData, PropertyKind, CI_DEVICE_ID_FUNCTION_BLOCK, CI_VERSION,
};

/// A property this device exposes over Property Exchange: its resource name (the
/// JSON `resource` key, matched against an inbound Get) and the body returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CiProperty {
    /// The property header blob a Get must carry to select this property.
    pub header: Vec<u8>,
    /// The body returned in the Get reply.
    pub body: Vec<u8>,
}

/// The **responder** half of MIDI-CI — this device's declared identity, MUID,
/// supported profiles, and exposed properties. [`respond_to`](Self::respond_to)
/// turns an inbound [`CiMessage`] into the reply stream, exactly as
/// [`crate::EndpointNegotiator::respond_to`] does for UMP-Stream discovery.
#[derive(Clone, Debug)]
pub struct CiResponder {
    muid: Muid,
    identity: DiscoveryData,
    /// Profiles this device exposes, and whether each is currently enabled.
    profiles: Vec<(ProfileId, bool)>,
    /// Properties this device answers Get requests for.
    properties: Vec<CiProperty>,
}

impl CiResponder {
    /// A responder with the given MUID and Discovery identity, no profiles or
    /// properties. Add them with [`with_profiles`](Self::with_profiles) /
    /// [`with_properties`](Self::with_properties).
    pub fn new(muid: Muid, identity: DiscoveryData) -> Self {
        Self {
            muid,
            identity,
            profiles: Vec::new(),
            properties: Vec::new(),
        }
    }

    /// Declare the profiles this device exposes as `(id, enabled)` pairs.
    pub fn with_profiles(mut self, profiles: Vec<(ProfileId, bool)>) -> Self {
        self.profiles = profiles;
        self
    }

    /// Declare the properties this device answers Get requests for.
    pub fn with_properties(mut self, properties: Vec<CiProperty>) -> Self {
        self.properties = properties;
        self
    }

    /// This device's MUID.
    pub fn muid(&self) -> Muid {
        self.muid
    }

    /// A reply header addressed back to `dest`, sourced from our MUID.
    fn reply_header(&self, dest: Muid) -> CiHeader {
        CiHeader {
            device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
            ci_version: CI_VERSION,
            source: self.muid,
            destination: dest,
        }
    }

    /// The reply stream for one inbound CI message. Empty if the message needs no
    /// reply (e.g. an enabled-report we merely observe); a single NAK for a
    /// message type we don't handle.
    pub fn respond_to(&self, inbound: &CiMessage) -> Vec<CiMessage> {
        let src = inbound.header().source;
        match inbound {
            // A Discovery inquiry → our Discovery Reply. (We ignore inbound
            // replies — we're the responder.)
            CiMessage::Discovery {
                is_reply: false, ..
            } => vec![CiMessage::Discovery {
                header: self.reply_header(src),
                is_reply: true,
                data: self.identity,
            }],

            // Profile Inquiry → our enabled/disabled profile lists.
            CiMessage::Profile {
                state: ProfileState::Inquiry,
                ..
            } => {
                let enabled: Vec<ProfileId> = self
                    .profiles
                    .iter()
                    .filter(|(_, on)| *on)
                    .map(|(id, _)| *id)
                    .collect();
                let disabled: Vec<ProfileId> = self
                    .profiles
                    .iter()
                    .filter(|(_, on)| !*on)
                    .map(|(id, _)| *id)
                    .collect();
                vec![CiMessage::Profile {
                    header: self.reply_header(src),
                    state: ProfileState::InquiryReply { enabled, disabled },
                }]
            }

            // Set Profile On/Off → the matching Enabled/Disabled report (only if
            // we actually expose that profile; otherwise NAK).
            CiMessage::Profile {
                state: ProfileState::SetOn(id),
                ..
            } => self.profile_report(src, *id, true),
            CiMessage::Profile {
                state: ProfileState::SetOff(id),
                ..
            } => self.profile_report(src, *id, false),

            // A property Get we can satisfy → its reply; else NAK.
            CiMessage::Property {
                data:
                    PropertyData {
                        kind: PropertyKind::GetData,
                        request_id,
                        header,
                        ..
                    },
                ..
            } => self.property_reply(src, *request_id, header),

            // A property Set → an (empty-body) Set reply acknowledging it.
            CiMessage::Property {
                data:
                    PropertyData {
                        kind: PropertyKind::SetData,
                        request_id,
                        header,
                        ..
                    },
                ..
            } => vec![CiMessage::Property {
                header: self.reply_header(src),
                data: PropertyData {
                    kind: PropertyKind::SetDataReply,
                    request_id: *request_id,
                    header: header.clone(),
                    num_chunks: 1,
                    chunk: 1,
                    body: Vec::new(),
                },
            }],

            // Reports and replies we merely observe — no response.
            CiMessage::Discovery { is_reply: true, .. }
            | CiMessage::Profile {
                state:
                    ProfileState::InquiryReply { .. }
                    | ProfileState::Enabled(_)
                    | ProfileState::Disabled(_),
                ..
            }
            | CiMessage::Property {
                data:
                    PropertyData {
                        kind: PropertyKind::GetDataReply | PropertyKind::SetDataReply,
                        ..
                    },
                ..
            }
            | CiMessage::InvalidateMuid { .. }
            | CiMessage::Nak { .. } => Vec::new(),

            // `CiMessage` is non-exhaustive; a future family we don't model yet
            // gets no reply (a conservative default, never a spurious NAK).
            _ => Vec::new(),
        }
    }

    /// Emit the Enabled/Disabled report for a profile we expose, or a NAK if we
    /// don't expose it.
    fn profile_report(&self, dest: Muid, id: ProfileId, enable: bool) -> Vec<CiMessage> {
        if self.profiles.iter().any(|(p, _)| *p == id) {
            let state = if enable {
                ProfileState::Enabled(id)
            } else {
                ProfileState::Disabled(id)
            };
            vec![CiMessage::Profile {
                header: self.reply_header(dest),
                state,
            }]
        } else {
            self.nak(dest, tutti_midi_types::ci::profile::SUB_ID2_SET_PROFILE_ON)
        }
    }

    /// A Get reply for the property whose header matches, or a NAK if none does.
    fn property_reply(&self, dest: Muid, request_id: u8, header: &[u8]) -> Vec<CiMessage> {
        match self.properties.iter().find(|p| p.header == header) {
            Some(prop) => vec![CiMessage::Property {
                header: self.reply_header(dest),
                data: PropertyData {
                    kind: PropertyKind::GetDataReply,
                    request_id,
                    header: prop.header.clone(),
                    num_chunks: 1,
                    chunk: 1,
                    body: prop.body.clone(),
                },
            }],
            None => self.nak(dest, tutti_midi_types::ci::property::SUB_ID2_GET_PROPERTY_DATA),
        }
    }

    /// A NAK addressed to `dest` for a message of `nak_sub_id2` we couldn't serve.
    fn nak(&self, dest: Muid, nak_sub_id2: u8) -> Vec<CiMessage> {
        vec![CiMessage::Nak {
            header: self.reply_header(dest),
            nak: Nak {
                nak_sub_id2,
                status_code: 0,
                status_data: 0,
            },
        }]
    }
}

/// What a [`CiInitiator`] has learned about a peer from its Discovery Reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredCiDevice {
    pub muid: Muid,
    pub identity: DiscoveryData,
    pub categories: CiCategories,
    /// Enabled + disabled profiles, filled in once a Profile Inquiry Reply arrives.
    pub profiles: Vec<ProfileId>,
}

/// The **inquiring** half of MIDI-CI. Build [`discovery`](Self::discovery), send
/// it, feed replies to [`ingest`](Self::ingest); [`discovered`](Self::discovered)
/// yields the peer once its Discovery Reply arrives. `ingest` also returns any
/// message we must send back — specifically an Invalidate MUID when a reply
/// carries *our* MUID (a collision).
#[derive(Clone, Debug)]
pub struct CiInitiator {
    muid: Muid,
    identity: DiscoveryData,
    discovered: Option<DiscoveredCiDevice>,
}

impl CiInitiator {
    /// An initiator with our MUID and Discovery identity.
    pub fn new(muid: Muid, identity: DiscoveryData) -> Self {
        Self {
            muid,
            identity,
            discovered: None,
        }
    }

    /// Our MUID.
    pub fn muid(&self) -> Muid {
        self.muid
    }

    /// The broadcast Discovery message this initiator sends to probe the bus.
    pub fn discovery(&self) -> CiMessage {
        CiMessage::Discovery {
            header: CiHeader {
                device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
                ci_version: CI_VERSION,
                source: self.muid,
                destination: Muid::BROADCAST,
            },
            is_reply: false,
            data: self.identity,
        }
    }

    /// Feed one inbound message. Records a Discovery Reply / Profile list, and
    /// returns any message we must emit in response — an Invalidate MUID on a
    /// MUID collision, otherwise nothing.
    pub fn ingest(&mut self, inbound: &CiMessage) -> Vec<CiMessage> {
        match inbound {
            CiMessage::Discovery {
                is_reply: true,
                header,
                data,
            } => {
                // MUID collision: a peer replied claiming our own MUID.
                if header.source == self.muid {
                    return vec![CiMessage::InvalidateMuid {
                        header: CiHeader {
                            device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
                            ci_version: CI_VERSION,
                            source: self.muid,
                            destination: Muid::BROADCAST,
                        },
                        target: self.muid,
                    }];
                }
                self.discovered = Some(DiscoveredCiDevice {
                    muid: header.source,
                    identity: *data,
                    categories: data.categories,
                    profiles: Vec::new(),
                });
                Vec::new()
            }
            CiMessage::Profile {
                state: ProfileState::InquiryReply { enabled, disabled },
                header,
            } => {
                if let Some(dev) = self.discovered.as_mut() {
                    if dev.muid == header.source {
                        dev.profiles = enabled.iter().chain(disabled).copied().collect();
                    }
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// The Profile Inquiry to send to a discovered peer (addressed to its MUID),
    /// or `None` if no peer has been discovered yet.
    pub fn profile_inquiry(&self) -> Option<CiMessage> {
        let dev = self.discovered.as_ref()?;
        Some(CiMessage::Profile {
            header: CiHeader {
                device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
                ci_version: CI_VERSION,
                source: self.muid,
                destination: dev.muid,
            },
            state: ProfileState::Inquiry,
        })
    }

    /// The peer discovered so far, or `None` until its Discovery Reply arrives.
    pub fn discovered(&self) -> Option<&DiscoveredCiDevice> {
        self.discovered.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(mfr: [u8; 3]) -> DiscoveryData {
        DiscoveryData {
            manufacturer: mfr,
            family: 0x1234,
            family_model: 0x0055,
            software_revision: [1, 0, 0, 0],
            categories: CiCategories::PROFILE_CONFIGURATION | CiCategories::PROPERTY_EXCHANGE,
            max_sysex_size: 512,
        }
    }

    fn responder() -> CiResponder {
        CiResponder::new(Muid(0x0011_2233), identity([0x00, 0x21, 0x09]))
            .with_profiles(vec![
                (ProfileId([0x7E, 1, 2, 3, 4]), true),
                (ProfileId([0x7E, 5, 6, 7, 8]), false),
            ])
            .with_properties(vec![CiProperty {
                header: br#"{"resource":"DeviceInfo"}"#.to_vec(),
                body: br#"{"name":"Tutti"}"#.to_vec(),
            }])
    }

    #[test]
    fn discovery_loopback_recovers_identity() {
        let resp = responder();
        let mut init = CiInitiator::new(Muid(0x0044_5566), identity([0x11, 0x22, 0x33]));

        // Initiator → responder → initiator.
        let replies = resp.respond_to(&init.discovery());
        assert_eq!(replies.len(), 1);
        let feedback = init.ingest(&replies[0]);
        assert!(feedback.is_empty(), "no collision, no feedback");

        let dev = init.discovered().expect("discovered the responder");
        assert_eq!(dev.muid, Muid(0x0011_2233));
        assert_eq!(dev.identity.manufacturer, [0x00, 0x21, 0x09]);
        assert!(dev.categories.contains(CiCategories::PROFILE_CONFIGURATION));
    }

    #[test]
    fn profile_inquiry_loopback_lists_profiles() {
        let resp = responder();
        let mut init = CiInitiator::new(Muid(0x0044_5566), identity([1, 2, 3]));
        init.ingest(&resp.respond_to(&init.discovery())[0]);

        let inquiry = init.profile_inquiry().expect("peer known");
        let reply = resp.respond_to(&inquiry);
        assert_eq!(reply.len(), 1);
        // The reply lists both profiles (one enabled, one disabled).
        match &reply[0] {
            CiMessage::Profile {
                state: ProfileState::InquiryReply { enabled, disabled },
                ..
            } => {
                assert_eq!(enabled.len(), 1);
                assert_eq!(disabled.len(), 1);
            }
            other => panic!("expected InquiryReply, got {other:?}"),
        }
        init.ingest(&reply[0]);
        assert_eq!(init.discovered().unwrap().profiles.len(), 2);
    }

    #[test]
    fn property_get_loopback_returns_body() {
        let resp = responder();
        let header = br#"{"resource":"DeviceInfo"}"#.to_vec();
        let get = CiMessage::Property {
            header: CiHeader {
                device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
                ci_version: CI_VERSION,
                source: Muid(0x1),
                destination: resp.muid(),
            },
            data: PropertyData {
                kind: PropertyKind::GetData,
                request_id: 3,
                header: header.clone(),
                num_chunks: 1,
                chunk: 1,
                body: Vec::new(),
            },
        };
        let reply = resp.respond_to(&get);
        match &reply[0] {
            CiMessage::Property { data, .. } => {
                assert_eq!(data.kind, PropertyKind::GetDataReply);
                assert_eq!(data.request_id, 3);
                assert_eq!(data.body, br#"{"name":"Tutti"}"#);
            }
            other => panic!("expected property reply, got {other:?}"),
        }
    }

    #[test]
    fn unknown_property_is_nakked() {
        let resp = responder();
        let get = CiMessage::Property {
            header: CiHeader {
                device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
                ci_version: CI_VERSION,
                source: Muid(0x1),
                destination: resp.muid(),
            },
            data: PropertyData {
                kind: PropertyKind::GetData,
                request_id: 0,
                header: br#"{"resource":"Nope"}"#.to_vec(),
                num_chunks: 1,
                chunk: 1,
                body: Vec::new(),
            },
        };
        assert!(matches!(resp.respond_to(&get)[0], CiMessage::Nak { .. }));
    }

    #[test]
    fn muid_collision_emits_invalidate() {
        let mut init = CiInitiator::new(Muid(0x0AAA_AAAA), identity([1, 2, 3]));
        // A reply that (wrongly) carries our own MUID as its source.
        let colliding = CiMessage::Discovery {
            header: CiHeader {
                device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
                ci_version: CI_VERSION,
                source: init.muid(),
                destination: init.muid(),
            },
            is_reply: true,
            data: identity([9, 9, 9]),
        };
        let feedback = init.ingest(&colliding);
        assert!(matches!(
            feedback.as_slice(),
            [CiMessage::InvalidateMuid { target, .. }] if *target == init.muid()
        ));
        assert!(init.discovered().is_none(), "collision is not a discovery");
    }
}
