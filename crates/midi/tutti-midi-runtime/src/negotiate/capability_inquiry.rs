//! MIDI-CI negotiation state machines (M2-101).
//!
//! The message codec lives in [`tutti_midi_types::ci`]; this module is the
//! *behavior* around it, mirroring the two halves of the UMP-Stream endpoint
//! work in [`super::endpoint`]:
//!
//! - [`CiResponder`] — the answering side. Pure `config-in → messages-out`:
//!   given an inbound [`CiMessage`], it returns the replies (Discovery Reply,
//!   Profile lists, Property data, or a NAK for anything unsupported).
//! - [`CiInitiator`] — the inquiring side. Builds a Discovery, ingests replies
//!   into a [`DiscoveredCiDevice`], and — the one bit of real protocol state —
//!   detects a MUID collision (a reply bearing this initiator's own MUID) and
//!   emits an Invalidate MUID.
//!
//! Both are pure and loopback-testable: a `CiInitiator`'s Discovery fed to a
//! `CiResponder`, whose reply is fed back, reconstructs the responder's identity.
//! No transport, no inbound routing — that wiring is the ECS layer's job.

use tutti_midi_types::ci::{
    CI_DEVICE_ID_FUNCTION_BLOCK, CI_VERSION, CiCategories, CiHeader, CiMessage, DiscoveryData,
    Muid, Nak, ProfileId, ProfileState, PropertyCapabilities, PropertyData, PropertyKind,
    SubscriptionCommand,
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
    #[must_use]
    pub fn new(muid: Muid, identity: DiscoveryData) -> Self {
        Self {
            muid,
            identity,
            profiles: Vec::new(),
            properties: Vec::new(),
            pe_capabilities: PropertyCapabilities {
                // One in-flight request. Honest for a responder with no request
                // queue: claiming more invites a peer to pipeline inquiries that
                // would then have to be dropped.
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
    #[must_use]
    pub fn with_pe_capabilities(mut self, caps: PropertyCapabilities) -> Self {
        self.pe_capabilities = caps;
        self
    }

    /// Declare the profiles this device exposes as `(id, enabled)` pairs.
    #[must_use]
    pub fn with_profiles(mut self, profiles: Vec<(ProfileId, bool)>) -> Self {
        self.profiles = profiles;
        self
    }

    /// Declare the properties this device answers Get requests for.
    #[must_use]
    pub fn with_properties(mut self, properties: Vec<CiProperty>) -> Self {
        self.properties = properties;
        self
    }

    /// This device's MUID.
    pub fn muid(&self) -> Muid {
        self.muid
    }

    /// A reply header addressed back to `dest`, sourced from this responder's
    /// MUID.
    fn reply_header(&self, dest: Muid) -> CiHeader {
        CiHeader {
            device_id: CI_DEVICE_ID_FUNCTION_BLOCK,
            ci_version: CI_VERSION,
            source: self.muid,
            destination: dest,
        }
    }

    /// The reply stream for one inbound CI message.
    ///
    /// Empty when the message needs no reply — a report or a reply this
    /// responder merely observes. A single NAK for a message family this layer
    /// does not model: M2-101 §5.11 lists that as the first intended use of a
    /// NAK, and silence would leave the initiator waiting out its §5.5.5
    /// timeout.
    pub fn respond_to(&self, inbound: &CiMessage) -> Vec<CiMessage> {
        let src = inbound.header().source;
        match inbound {
            // A Discovery inquiry → this device's Discovery Reply. Inbound
            // replies are ignored: this is the responder half.
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
                    // Message." It identifies the *initiator's* MIDI Out
                    // connection, so substituting this device's own would name
                    // the wrong path.
                    output_path_id: inquiry.output_path_id,
                    ..self.identity
                },
            }],

            // Profile Inquiry → this device's enabled/disabled profile lists.
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

            // Set Profile On/Off → the matching Enabled/Disabled report, but
            // only for a profile this device exposes; otherwise NAK.
            CiMessage::Profile {
                state: ProfileState::SetOn(id),
                ..
            } => self.profile_report(src, *id, true),
            CiMessage::Profile {
                state: ProfileState::SetOff(id),
                ..
            } => self.profile_report(src, *id, false),

            // A property Get this device can satisfy → its reply; else NAK.
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

            // PE Capabilities inquiry → the declared capabilities. §8.4
            // recommends a peer asks this once before any other Property
            // Exchange inquiry, so a NAK here stalls the whole family before it
            // starts.
            CiMessage::PropertyCapabilities {
                is_reply: false, ..
            } => vec![CiMessage::PropertyCapabilities {
                header: self.reply_header(src),
                is_reply: true,
                data: self.pe_capabilities,
            }],

            // Profile Details Inquiry. The detail formats are defined per
            // profile (§7.6.1) or by M2-102, neither of which this layer
            // models, so answer only for a profile this device exposes, and do
            // so with empty target data rather than inventing values.
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

            // Reports and replies that are merely observed — no response.
            //
            // Notify (§8.13) is here deliberately. It is deprecated in favour of
            // ACK/NAK, and the spec's requirement is to "continue to honor the
            // rules receiving a Notify message" — receiving, not answering.
            // Replying would invent traffic the spec asks a device not to send.
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

            // A CI family this layer does not model. M2-101 §5.11 lists "Reply
            // to a MIDI-CI message the Device does not support" as the first
            // intended use of a NAK, and Table 16 has the code for it. Silence
            // instead leaves the initiator waiting out its §5.5.5 timeout.
            _ => self.nak_with(src, 0, Nak::STATUS_MESSAGE_NOT_SUPPORTED),
        }
    }

    /// The Enabled/Disabled report for a profile this device exposes, or a NAK
    /// for one it does not.
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
            // The property is not held here — the *resource*, not the message,
            // is what is unsupported, so retrying will not change the answer.
            // That is why this is a Do-Not-Retry status.
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
    /// A `start` naming a resource this device does not hold is the one case
    /// that NAKs: the resource, not the message, is unsupported, and no retry
    /// will change that.
    fn subscription_reply(&self, dest: Muid, request_id: u8, header: &[u8]) -> Vec<CiMessage> {
        let command: Option<SubscriptionCommand> =
            header_str_field(header, "command").and_then(|s| s.parse().ok());

        // §11.1 makes partial/full/notify Responder-only. Arriving here they are
        // inbound *to* the responder, so an initiator sent one out of turn.
        if let Some(cmd) = command
            && !cmd.is_valid_from(true)
        {
            return self.nak_with(
                dest,
                tutti_midi_types::ci::property::SUB_ID2_SUBSCRIPTION,
                Nak::STATUS_MESSAGE_NOT_SUPPORTED,
            );
        }

        // A `start` names a resource, and a peer can only be subscribed to a
        // resource this device actually holds.
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
    /// The peer's MUID, taken from the *source* of its Discovery Reply. This is
    /// the address subsequent inquiries are sent to.
    pub muid: Muid,
    /// The peer's Discovery identity: manufacturer, family, model, version and
    /// its maximum SysEx size.
    pub identity: DiscoveryData,
    /// Which CI families the peer declared support for — profiles, property
    /// exchange, process inquiry. Asking outside this set earns a NAK.
    pub categories: CiCategories,
    /// Enabled + disabled profiles, concatenated. Empty until a Profile Inquiry
    /// Reply arrives; the two lists are not distinguishable here.
    pub profiles: Vec<ProfileId>,
}

/// The **inquiring** half of MIDI-CI. Build [`discovery`](Self::discovery), send
/// it, feed replies to [`ingest`](Self::ingest); [`discovered`](Self::discovered)
/// yields the peer once its Discovery Reply arrives. `ingest` also returns any
/// message that must be sent back — specifically an Invalidate MUID when a
/// reply carries this initiator's own MUID (a collision).
#[derive(Clone, Debug)]
pub struct CiInitiator {
    muid: Muid,
    identity: DiscoveryData,
    discovered: Option<DiscoveredCiDevice>,
}

impl CiInitiator {
    /// An initiator with the given MUID and Discovery identity, having
    /// discovered nothing yet.
    #[must_use]
    pub fn new(muid: Muid, identity: DiscoveryData) -> Self {
        Self {
            muid,
            identity,
            discovered: None,
        }
    }

    /// This initiator's own MUID — the `source` of every message it builds, and
    /// the value a reply must *not* carry.
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

    /// Feeds one inbound message. Records a Discovery Reply / Profile list, and
    /// returns any message that must be emitted in response — an Invalidate MUID
    /// on a MUID collision, otherwise nothing.
    pub fn ingest(&mut self, inbound: &CiMessage) -> Vec<CiMessage> {
        match inbound {
            CiMessage::Discovery {
                is_reply: true,
                header,
                data,
            } => {
                // MUID collision: a peer replied claiming this initiator's own
                // MUID.
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
                if let Some(dev) = self.discovered.as_mut()
                    && dev.muid == header.source
                {
                    dev.profiles = enabled.iter().chain(disabled).copied().collect();
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
mod tests;
