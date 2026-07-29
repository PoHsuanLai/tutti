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
    PropertyCapabilities, PropertyData, PropertyKind, SubscriptionCommand,
    CI_DEVICE_ID_FUNCTION_BLOCK, CI_VERSION,
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

/// Read a flat `"key": "value"` string field out of a Property Exchange header.
///
/// PE headers are JSON (M2-103 §5), but this crate deliberately keeps them
/// opaque — the codec moves bytes and the schema is the application's business.
/// Subscription is the one place the *protocol* depends on a header field:
/// §11.1 puts `command` and `subscribeId` there, and the direction rules cannot
/// be checked without reading `command`.
///
/// So this is a deliberately narrow scanner, not a JSON parser: top-level string
/// values only, no escapes, no nesting. That covers every field §11.1 defines
/// (`command` is an enum of five ASCII words; `subscribeId` is "max 8 chars,
/// 'a-z', '0-9' or '_' characters only"). Anything richer belongs to a real
/// parser in the application, which is where a JSON dependency would belong too.
fn header_str_field<'a>(header: &'a [u8], key: &str) -> Option<&'a str> {
    let text = std::str::from_utf8(header).ok()?;
    let pat = format!("\"{key}\"");
    let after_key = &text[text.find(&pat)? + pat.len()..];
    let after_colon = &after_key[after_key.find(':')? + 1..];
    let open = after_colon.find('"')?;
    let rest = &after_colon[open + 1..];
    let close = rest.find('"')?;
    Some(&rest[..close])
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
    /// What this device reports for a Property Exchange Capabilities inquiry.
    pe_capabilities: PropertyCapabilities,
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
            pe_capabilities: PropertyCapabilities {
                // One in-flight request. Honest for a responder with no request
                // queue: claiming more would invite a peer to pipeline
                // inquiries we would then have to drop.
                simultaneous_requests: 1,
                // M2-101 §8.5 Table 31: Common Rules for PE 1.0/1.1 is major
                // 0x00, minor 0x00.
                major_version: 0,
                minor_version: 0,
            },
        }
    }

    /// Override the Property Exchange capabilities this device reports
    /// (M2-101 §8.4). Raise `simultaneous_requests` only alongside a request
    /// queue that can actually service that many.
    pub fn with_pe_capabilities(mut self, caps: PropertyCapabilities) -> Self {
        self.pe_capabilities = caps;
        self
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
                is_reply: false,
                data: inquiry,
                ..
            } => vec![CiMessage::Discovery {
                header: self.reply_header(src),
                is_reply: true,
                data: DiscoveryData {
                    // §5.6.1: "The Reply to Discovery shall return the same
                    // Output Path ID provided in the originating Discovery
                    // Message." It identifies the initiator's MIDI Out
                    // connection, so echoing ours would name the wrong path.
                    output_path_id: inquiry.output_path_id,
                    ..self.identity
                },
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

            // A Subscription. M2-103 §11.2: a device receiving one "shall reply
            // with a Reply to Subscription message, so the original sender is
            // aware of the success or failure of a command" — so this arm never
            // stays silent, even for a command it rejects.
            CiMessage::Property {
                data:
                    PropertyData {
                        kind: PropertyKind::Subscription,
                        request_id,
                        header,
                        ..
                    },
                ..
            } => self.subscription_reply(src, *request_id, header),

            // PE Capabilities inquiry → what we support. §8.4 recommends a peer
            // asks this once before any other Property Exchange inquiry, so a
            // NAK here would stall the whole family before it starts.
            CiMessage::PropertyCapabilities {
                is_reply: false, ..
            } => vec![CiMessage::PropertyCapabilities {
                header: self.reply_header(src),
                is_reply: true,
                data: self.pe_capabilities,
            }],

            // Profile Details Inquiry. The detail formats are defined per
            // profile (§7.6.1) or by M2-102, neither of which this layer
            // models, so answer only for a profile we actually expose and say
            // so with an empty target data rather than inventing values.
            CiMessage::Profile {
                state: ProfileState::DetailsInquiry { profile, target },
                ..
            } => {
                if self.profiles.iter().any(|(p, _)| p == profile) {
                    vec![CiMessage::Profile {
                        header: self.reply_header(src),
                        state: ProfileState::DetailsReply {
                            profile: *profile,
                            // §7.7.1: "shall be the same as in the Profile
                            // Details Inquiry message which was received".
                            target: *target,
                            data: Vec::new(),
                        },
                    }]
                } else {
                    self.nak_with(
                        src,
                        tutti_midi_types::ci::profile::SUB_ID2_PROFILE_DETAILS_INQUIRY,
                        Nak::STATUS_PROFILE_NOT_SUPPORTED,
                    )
                }
            }

            // Reports and replies we merely observe — no response.
            //
            // Notify (§8.13) is here deliberately. It is deprecated in favour of
            // ACK/NAK, and the spec's requirement is that we "continue to honor
            // the rules receiving a Notify message" — receiving, not answering.
            // Replying would be inventing traffic the spec asks us not to send.
            CiMessage::Property {
                data:
                    PropertyData {
                        kind: PropertyKind::Notify,
                        ..
                    },
                ..
            }
            | CiMessage::PropertyCapabilities { is_reply: true, .. }
            | CiMessage::Profile {
                state:
                    ProfileState::Added(_)
                    | ProfileState::Removed(_)
                    | ProfileState::DetailsReply { .. }
                    | ProfileState::SpecificData { .. },
                ..
            }
            | CiMessage::Discovery { is_reply: true, .. }
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
                        kind:
                            PropertyKind::GetDataReply
                            | PropertyKind::SetDataReply
                            | PropertyKind::SubscriptionReply,
                        ..
                    },
                ..
            }
            | CiMessage::InvalidateMuid { .. }
            | CiMessage::Nak { .. } => Vec::new(),

            // A CI family we don't model. M2-101 §5.11 lists "Reply to a MIDI-CI
            // message the Device does not support" as the first intended use of
            // a NAK, and Table 16 has the code for it. Staying silent instead
            // leaves the initiator waiting out its §5.5.5 timeout, so answer.
            _ => self.nak_with(src, 0, Nak::STATUS_MESSAGE_NOT_SUPPORTED),
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
            // Table 16 has a code for exactly this: "Profile not supported on
            // the requested Channel, Group, or Function Block".
            self.nak_with(
                dest,
                tutti_midi_types::ci::profile::SUB_ID2_SET_PROFILE_ON,
                Nak::STATUS_PROFILE_NOT_SUPPORTED,
            )
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
            // We don't hold this property — the resource, not the message, is
            // what's unsupported, and retrying won't change that.
            None => self.nak_with(
                dest,
                tutti_midi_types::ci::property::SUB_ID2_GET_PROPERTY_DATA,
                Nak::STATUS_MESSAGE_NOT_SUPPORTED,
            ),
        }
    }

    /// The mandatory reply to an inbound Subscription (M2-103 §11.2).
    ///
    /// The reply echoes the request id and header so the initiator can tie it to
    /// its request. A command this responder cannot honour still gets a reply
    /// rather than a NAK: §11.2 frames the reply as reporting "the success or
    /// failure of a command", and it is the reply — not silence — that lets the
    /// sender "decide to retry or end the Subscription".
    ///
    /// A `start` for a resource we do not hold is the one case that NAKs: the
    /// resource, not the message, is unsupported, and no retry will change that.
    fn subscription_reply(&self, dest: Muid, request_id: u8, header: &[u8]) -> Vec<CiMessage> {
        let command = header_str_field(header, "command").and_then(SubscriptionCommand::parse);

        // §11.1 makes partial/full/notify Responder-only. Arriving here they are
        // inbound *to* the responder, so an initiator sent one out of turn.
        if let Some(cmd) = command {
            if !cmd.is_valid_from(true) {
                return self.nak_with(
                    dest,
                    tutti_midi_types::ci::property::SUB_ID2_SUBSCRIPTION,
                    Nak::STATUS_MESSAGE_NOT_SUPPORTED,
                );
            }
        }

        // A `start` names a resource; we can only subscribe to what we hold.
        if command == Some(SubscriptionCommand::Start) {
            let resource = header_str_field(header, "resource");
            let known = resource.is_some_and(|r| {
                self.properties
                    .iter()
                    .any(|p| header_str_field(&p.header, "resource") == Some(r))
            });
            if !known {
                return self.nak_with(
                    dest,
                    tutti_midi_types::ci::property::SUB_ID2_SUBSCRIPTION,
                    Nak::STATUS_MESSAGE_NOT_SUPPORTED,
                );
            }
        }

        vec![CiMessage::Property {
            header: self.reply_header(dest),
            data: PropertyData {
                kind: PropertyKind::SubscriptionReply,
                request_id,
                header: header.to_vec(),
                num_chunks: 1,
                chunk: 1,
                // §11.1: a Start's "Response does not return any Property Data".
                body: Vec::new(),
            },
        }]
    }

    /// A NAK addressed to `dest` for a message of `nak_sub_id2`, carrying a
    /// specific M2-101 Table 16 status code.
    ///
    /// Table 16 splits "Do Not Retry" (0x00-0x1F) from "Retry is recommended"
    /// (0x40-0x5F), so a precise code is what lets the initiator decide whether
    /// to try again — a blanket 0x00 tells it nothing.
    fn nak_with(&self, dest: Muid, nak_sub_id2: u8, status_code: u8) -> Vec<CiMessage> {
        vec![CiMessage::Nak {
            header: self.reply_header(dest),
            nak: Nak::new(nak_sub_id2, status_code),
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
        DiscoveryData::new(
            mfr,
            0x1234,
            0x0055,
            [1, 0, 0, 0],
            CiCategories::PROFILE_CONFIGURATION | CiCategories::PROPERTY_EXCHANGE,
            512,
        )
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
    fn reply_echoes_the_initiators_output_path_id() {
        // §5.6.1: "The Reply to Discovery shall return the same Output Path ID
        // provided in the originating Discovery Message." It names the
        // *initiator's* MIDI Out connection, so a responder that answered with
        // its own id would point at the wrong path.
        let resp = responder();
        let mut init_identity = identity([0x11, 0x22, 0x33]);
        init_identity.output_path_id = 0x42;
        let init = CiInitiator::new(Muid(0x0044_5566), init_identity);

        let replies = resp.respond_to(&init.discovery());
        match &replies[0] {
            CiMessage::Discovery {
                is_reply: true,
                data,
                ..
            } => {
                assert_eq!(data.output_path_id, 0x42, "echoed, not the responder's");
                // …while the rest of the reply is still the responder's identity.
                assert_eq!(data.manufacturer, [0x00, 0x21, 0x09]);
            }
            other => panic!("expected a Discovery reply, got {other:?}"),
        }
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

    /// A Subscription message carrying `header`, addressed to `resp`.
    fn subscription(resp: &CiResponder, header: &str) -> CiMessage {
        CiMessage::Property {
            header: CiHeader {
                device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
                ci_version: CI_VERSION,
                source: Muid(0x1),
                destination: resp.muid(),
            },
            data: PropertyData {
                kind: PropertyKind::Subscription,
                request_id: 9,
                header: header.as_bytes().to_vec(),
                num_chunks: 1,
                chunk: 1,
                body: Vec::new(),
            },
        }
    }

    #[test]
    fn pe_capabilities_inquiry_is_answered() {
        // §8.4 has a peer ask this once before any other PE inquiry, so a NAK
        // here would stall the whole family before it starts.
        let resp = responder();
        let inquiry = CiMessage::PropertyCapabilities {
            header: CiHeader {
                device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
                ci_version: CI_VERSION,
                source: Muid(0x1),
                destination: resp.muid(),
            },
            is_reply: false,
            data: PropertyCapabilities {
                simultaneous_requests: 0,
                major_version: 0,
                minor_version: 0,
            },
        };
        let reply = resp.respond_to(&inquiry);
        assert_eq!(reply.len(), 1);
        match &reply[0] {
            CiMessage::PropertyCapabilities { is_reply, data, .. } => {
                assert!(*is_reply);
                assert_eq!(data.simultaneous_requests, 1, "the default we declare");
            }
            other => panic!("expected a capabilities reply, got {other:?}"),
        }
    }

    #[test]
    fn a_capabilities_reply_is_observed_without_answering() {
        let resp = responder();
        let inbound = CiMessage::PropertyCapabilities {
            header: CiHeader {
                device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
                ci_version: CI_VERSION,
                source: Muid(0x1),
                destination: resp.muid(),
            },
            is_reply: true,
            data: PropertyCapabilities {
                simultaneous_requests: 4,
                major_version: 0,
                minor_version: 0,
            },
        };
        assert!(resp.respond_to(&inbound).is_empty());
    }

    #[test]
    fn profile_details_inquiry_echoes_its_target() {
        // §7.7.1: the reply's target "shall be the same as in the Profile
        // Details Inquiry message which was received".
        let resp = responder();
        let profile = resp.profiles[0].0;
        let inquiry = CiMessage::Profile {
            header: CiHeader {
                device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
                ci_version: CI_VERSION,
                source: Muid(0x1),
                destination: resp.muid(),
            },
            state: ProfileState::DetailsInquiry {
                profile,
                target: 0x42,
            },
        };
        match &resp.respond_to(&inquiry)[0] {
            CiMessage::Profile {
                state:
                    ProfileState::DetailsReply {
                        target, profile: p, ..
                    },
                ..
            } => {
                assert_eq!(*target, 0x42, "echoed, not re-chosen");
                assert_eq!(*p, profile);
            }
            other => panic!("expected a DetailsReply, got {other:?}"),
        }
    }

    #[test]
    fn details_inquiry_for_an_unexposed_profile_is_nakked() {
        let resp = responder();
        let inquiry = CiMessage::Profile {
            header: CiHeader {
                device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
                ci_version: CI_VERSION,
                source: Muid(0x1),
                destination: resp.muid(),
            },
            state: ProfileState::DetailsInquiry {
                profile: ProfileId([0x63, 0x63, 0x63, 0x63, 0x63]),
                target: 0,
            },
        };
        assert!(matches!(
            &resp.respond_to(&inquiry)[0],
            CiMessage::Nak { .. }
        ));
    }

    #[test]
    fn a_notify_is_honored_by_being_received_not_answered() {
        // §8.13 deprecates Notify in favour of ACK/NAK: devices "should not
        // send a Notify message" but "shall continue to honor the rules
        // receiving" one. Honoring it means accepting it — replying would emit
        // traffic the spec asks us not to produce.
        let resp = responder();
        let inbound = CiMessage::Property {
            header: CiHeader {
                device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
                ci_version: CI_VERSION,
                source: Muid(0x1),
                destination: resp.muid(),
            },
            data: PropertyData {
                kind: PropertyKind::Notify,
                request_id: 1,
                header: br#"{"status":144}"#.to_vec(),
                num_chunks: 1,
                chunk: 1,
                body: Vec::new(),
            },
        };
        assert!(
            resp.respond_to(&inbound).is_empty(),
            "received without a reply, and without a NAK"
        );
    }

    #[test]
    fn added_and_removed_reports_are_observed_without_answering() {
        // These are broadcast notifications (§7.4/§7.5). Answering a broadcast
        // would have every listening device reply at once.
        let resp = responder();
        for state in [
            ProfileState::Added(ProfileId([1, 2, 3, 4, 5])),
            ProfileState::Removed(ProfileId([1, 2, 3, 4, 5])),
        ] {
            let inbound = CiMessage::Profile {
                header: CiHeader {
                    device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
                    ci_version: CI_VERSION,
                    source: Muid(0x1),
                    destination: Muid::BROADCAST,
                },
                state,
            };
            assert!(resp.respond_to(&inbound).is_empty());
        }
    }

    #[test]
    fn a_start_subscription_is_acknowledged_with_no_body() {
        // M2-103 §11.2 makes the reply mandatory; §11.1 says a Start's
        // "Response does not return any Property Data".
        let resp = responder();
        let reply = resp.respond_to(&subscription(
            &resp,
            r#"{"resource":"DeviceInfo","command":"start"}"#,
        ));
        assert_eq!(reply.len(), 1);
        match &reply[0] {
            CiMessage::Property { data, .. } => {
                assert_eq!(data.kind, PropertyKind::SubscriptionReply);
                assert_eq!(data.request_id, 9, "echoed so the initiator can match it");
                assert!(data.body.is_empty(), "a Start reply carries no data");
            }
            other => panic!("expected a SubscriptionReply, got {other:?}"),
        }
    }

    #[test]
    fn an_end_subscription_is_acknowledged_without_naming_a_resource() {
        // §11.5 lets either side end a subscription, and an `end` is keyed on
        // subscribeId rather than a resource — so it must not be run through the
        // resource check that gates `start`.
        let resp = responder();
        let reply = resp.respond_to(&subscription(
            &resp,
            r#"{"subscribeId":"sub1","command":"end"}"#,
        ));
        assert_eq!(reply.len(), 1);
        assert!(matches!(
            &reply[0],
            CiMessage::Property {
                data: PropertyData {
                    kind: PropertyKind::SubscriptionReply,
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn a_start_for_an_unheld_resource_is_nakked() {
        // We cannot subscribe a peer to data we do not have. The resource, not
        // the message, is unsupported — so a NAK, not an empty reply.
        let resp = responder();
        let reply = resp.respond_to(&subscription(
            &resp,
            r#"{"resource":"NoSuch","command":"start"}"#,
        ));
        assert!(matches!(&reply[0], CiMessage::Nak { .. }));
    }

    #[test]
    fn responder_only_commands_from_an_initiator_are_rejected() {
        // §11.1 marks partial/full/notify "Responder only". Inbound *to* the
        // responder they can only have come from an initiator talking out of
        // turn — the note under §8.11 says an Initiator "shall not send updates
        // to the Property Data by this message".
        let resp = responder();
        for cmd in ["partial", "full", "notify"] {
            let header = format!(r#"{{"resource":"DeviceInfo","command":"{cmd}"}}"#);
            let reply = resp.respond_to(&subscription(&resp, &header));
            assert!(
                matches!(&reply[0], CiMessage::Nak { .. }),
                "{cmd} is responder-only and must be rejected"
            );
        }
    }

    #[test]
    fn a_subscription_reply_is_observed_without_answering() {
        // Replying to a reply would loop forever between two devices.
        let resp = responder();
        let inbound = CiMessage::Property {
            header: CiHeader {
                device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
                ci_version: CI_VERSION,
                source: Muid(0x1),
                destination: resp.muid(),
            },
            data: PropertyData {
                kind: PropertyKind::SubscriptionReply,
                request_id: 9,
                header: Vec::new(),
                num_chunks: 1,
                chunk: 1,
                body: Vec::new(),
            },
        };
        assert!(resp.respond_to(&inbound).is_empty());
    }

    #[test]
    fn header_fields_are_read_out_of_realistic_json() {
        // The scanner is narrow by design, so pin what it must handle: key order
        // it does not control, whitespace, and a field that is absent.
        fn cmd(s: &str) -> Option<&str> {
            super::header_str_field(s.as_bytes(), "command")
        }
        assert_eq!(cmd(r#"{"command":"start"}"#), Some("start"));
        assert_eq!(cmd(r#"{ "command" : "end" }"#), Some("end"), "whitespace");
        assert_eq!(
            cmd(r#"{"resource":"X","command":"full"}"#),
            Some("full"),
            "not the first key"
        );
        assert_eq!(cmd(r#"{"resource":"X"}"#), None, "absent field");
        assert_eq!(cmd("not json at all"), None);
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
