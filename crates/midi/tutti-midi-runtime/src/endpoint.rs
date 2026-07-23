//! MIDI 2.0 UMP Stream endpoint negotiation (M2-104 §7.1).
//!
//! When tutti exposes a UMP endpoint to a peer, the peer opens with an
//! **Endpoint Discovery** request; the endpoint answers with the notifications
//! the request asked for (Endpoint Info, Device Identity, Stream Configuration,
//! and one Function Block Info per block). [`EndpointNegotiator`] holds this
//! endpoint's declared identity + topology and turns an inbound discovery into
//! the exact reply stream.
//!
//! `respond_to` is the whole inbound half: config in, reply events out. It is
//! pure (no interior mutation), so it is trivially testable and safe to call
//! from any thread.

use tutti_midi_types::midi2::ump_stream::{Direction, UmpStream};
use tutti_midi_types::midi2::UmpMessage;
use tutti_midi_types::ump::{endpoint_name, product_instance_id};
use tutti_midi_types::{
    EndpointCapabilities, EndpointDiscoveryRequest, FunctionBlockDirection, FunctionBlocks,
    JrTimestamps, MidiEvent, Protocol, UmpVersion,
};

/// One Function Block this endpoint exposes (M2-104 §7.1.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FunctionBlock {
    pub block_number: u8,
    pub first_group: u8,
    pub num_groups: u8,
    pub direction: FunctionBlockDirection,
}

/// SysEx-style device identity carried in a Device Identity notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceIdentity {
    pub manufacturer: [u8; 3],
    pub family: u16,
    pub family_model: u16,
    pub software_version: [u8; 4],
}

/// This endpoint's declared identity, capabilities, and Function Block topology
/// — everything needed to answer an Endpoint Discovery.
#[derive(Clone, Debug)]
pub struct EndpointNegotiator {
    ump_version: UmpVersion,
    capabilities: EndpointCapabilities,
    protocol: Protocol,
    jr: JrTimestamps,
    identity: DeviceIdentity,
    name: String,
    product_instance_id: String,
    function_blocks: Vec<FunctionBlock>,
}

impl EndpointNegotiator {
    /// A negotiator for an endpoint speaking UMP 1.1 with the given identity and
    /// Function Blocks, advertising MIDI-2 + MIDI-1 protocol support (no JR
    /// timestamps). The endpoint name and product-instance id default to empty
    /// (their notifications are then omitted). Adjust with the `with_*` setters.
    pub fn new(identity: DeviceIdentity, function_blocks: Vec<FunctionBlock>) -> Self {
        Self {
            ump_version: UmpVersion::V1_1,
            capabilities: EndpointCapabilities::MIDI2_PROTOCOL
                | EndpointCapabilities::MIDI1_PROTOCOL,
            protocol: Protocol::Midi2,
            jr: JrTimestamps::empty(),
            identity,
            name: String::new(),
            product_instance_id: String::new(),
            function_blocks,
        }
    }

    /// Override the advertised capabilities (protocol + JR support).
    pub fn with_capabilities(mut self, capabilities: EndpointCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Override the currently-configured protocol + active JR-timestamp directions.
    pub fn with_stream_config(mut self, protocol: Protocol, jr: JrTimestamps) -> Self {
        self.protocol = protocol;
        self.jr = jr;
        self
    }

    /// Set the human-readable endpoint name and product-instance id reported in
    /// their respective notifications. Empty strings omit those replies.
    pub fn with_names(
        mut self,
        name: impl Into<String>,
        product_instance_id: impl Into<String>,
    ) -> Self {
        self.name = name.into();
        self.product_instance_id = product_instance_id.into();
        self
    }

    /// The Endpoint Info notification describing this endpoint.
    pub fn endpoint_info(&self) -> MidiEvent {
        MidiEvent::endpoint_info(
            self.ump_version,
            FunctionBlocks {
                count: self.function_blocks.len() as u8,
                is_static: true,
            },
            self.capabilities,
        )
    }

    /// The reply stream for an inbound Endpoint Discovery `event`. Returns empty
    /// if `event` is not an Endpoint Discovery. Replies are ordered Endpoint Info
    /// → Device Identity → Stream Configuration → Function Block Info (one per
    /// block), each included only if the discovery requested it.
    pub fn respond_to(&self, event: &MidiEvent) -> Vec<MidiEvent> {
        let Ok(UmpMessage::UmpStream(UmpStream::EndpointDiscovery(d))) =
            UmpMessage::try_from(event.data_words())
        else {
            return Vec::new();
        };

        let mut out = Vec::new();
        if d.request_endpoint_info() {
            out.push(self.endpoint_info());
        }
        if d.request_device_identity() {
            let id = &self.identity;
            out.push(MidiEvent::device_identity(
                id.manufacturer,
                id.family,
                id.family_model,
                id.software_version,
            ));
        }
        if d.request_endpoint_name() && !self.name.is_empty() {
            endpoint_name(&self.name, &mut out);
        }
        if d.request_product_instance_id() && !self.product_instance_id.is_empty() {
            product_instance_id(&self.product_instance_id, &mut out);
        }
        if d.request_stream_configuration() {
            out.push(MidiEvent::stream_configuration_notification(
                self.protocol,
                self.jr,
            ));
        }
        // Function Block Info isn't gated by a discovery request flag — an
        // endpoint that has blocks announces them alongside its info.
        for fb in &self.function_blocks {
            out.push(MidiEvent::function_block_info(
                true,
                fb.block_number,
                fb.first_group,
                fb.num_groups,
                fb.direction,
            ));
        }
        out
    }

    /// Build the outbound Endpoint Discovery request this endpoint would send to
    /// probe a peer, asking for every reply. Use when tutti is the *discoverer*.
    pub fn discovery_request() -> MidiEvent {
        MidiEvent::endpoint_discovery(
            UmpVersion::V1_1.major,
            UmpVersion::V1_1.minor,
            EndpointDiscoveryRequest::all(),
        )
    }
}

/// What an [`EndpointInquiry`] has learned about a peer from its discovery-reply
/// stream. Fields fill in as the matching notifications arrive; [`name`] /
/// [`product_instance_id`] stay empty until their (possibly multi-packet)
/// notifications complete.
///
/// [`name`]: Self::name
/// [`product_instance_id`]: Self::product_instance_id
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiscoveredEndpoint {
    pub ump_version: Option<UmpVersion>,
    pub capabilities: EndpointCapabilities,
    pub protocol: Option<Protocol>,
    pub jr: JrTimestamps,
    pub identity: Option<DeviceIdentity>,
    pub name: String,
    pub product_instance_id: String,
    pub function_blocks: Vec<FunctionBlock>,
}

/// The **discoverer** half of UMP-Stream endpoint negotiation — the counterpart
/// to [`EndpointNegotiator`] (the responder). Build its [`request`](Self::request)
/// (an Endpoint Discovery asking for everything), send it, then feed each reply
/// event through [`ingest`](Self::ingest); [`result`](Self::result) yields the
/// assembled [`DiscoveredEndpoint`] once at least the Endpoint Info notification
/// has arrived.
///
/// Multi-packet text notifications (Endpoint Name, Product Instance Id) are
/// reassembled by accumulating their words until the message decodes — the
/// receive counterpart to `push_ump_stream_packets`.
#[derive(Clone, Debug, Default)]
pub struct EndpointInquiry {
    discovered: DiscoveredEndpoint,
    saw_info: bool,
    /// In-progress Endpoint Name packet words (across multi-packet messages).
    name_words: Vec<u32>,
    /// In-progress Product Instance Id packet words.
    product_words: Vec<u32>,
}

impl EndpointInquiry {
    /// A fresh inquiry with no accumulated state.
    pub fn new() -> Self {
        Self::default()
    }

    /// The outbound Endpoint Discovery this inquiry sends to probe a peer, asking
    /// for every reply. Identical to [`EndpointNegotiator::discovery_request`].
    pub fn request() -> MidiEvent {
        EndpointNegotiator::discovery_request()
    }

    /// Feed one reply event from the peer. Non-UMP-Stream events are ignored.
    /// Returns `true` if the event advanced the discovered state.
    pub fn ingest(&mut self, event: &MidiEvent) -> bool {
        let Ok(UmpMessage::UmpStream(stream)) = UmpMessage::try_from(event.data_words()) else {
            return false;
        };
        match stream {
            UmpStream::EndpointInfo(m) => {
                self.discovered.ump_version = Some(UmpVersion {
                    major: m.ump_version_major(),
                    minor: m.ump_version_minor(),
                });
                let mut caps = EndpointCapabilities::empty();
                caps.set(EndpointCapabilities::MIDI2_PROTOCOL, m.supports_midi2_protocol());
                caps.set(EndpointCapabilities::MIDI1_PROTOCOL, m.supports_midi1_protocol());
                caps.set(
                    EndpointCapabilities::SEND_JR,
                    m.supports_sending_jr_timestamps(),
                );
                caps.set(
                    EndpointCapabilities::RECEIVE_JR,
                    m.supports_receiving_jr_timestamps(),
                );
                self.discovered.capabilities = caps;
                self.saw_info = true;
                true
            }
            UmpStream::DeviceIdentity(m) => {
                self.discovered.identity = Some(DeviceIdentity {
                    manufacturer: m.device_manufacturer().map(u8::from),
                    family: u16::from(m.device_family()),
                    family_model: u16::from(m.device_family_model_number()),
                    software_version: m.software_version().map(u8::from),
                });
                true
            }
            UmpStream::StreamConfigurationNotification(m) => {
                self.discovered.protocol = Some(match m.protocol() {
                    1 => Protocol::Midi1,
                    _ => Protocol::Midi2,
                });
                let mut jr = JrTimestamps::empty();
                jr.set(JrTimestamps::SEND, m.send_jr_timestamps());
                jr.set(JrTimestamps::RECEIVE, m.receive_jr_timestamps());
                self.discovered.jr = jr;
                true
            }
            UmpStream::FunctionBlockInfo(m) => {
                self.discovered.function_blocks.push(FunctionBlock {
                    block_number: u8::from(m.function_block_number()),
                    first_group: u8::from(m.first_group()),
                    num_groups: m.number_of_groups_spanned(),
                    direction: match m.direction() {
                        Direction::Input => FunctionBlockDirection::Input,
                        Direction::Output => FunctionBlockDirection::Output,
                        _ => FunctionBlockDirection::Bidirectional,
                    },
                });
                true
            }
            UmpStream::EndpointName(_) => {
                self.name_words.extend_from_slice(event.data_words());
                if let Some(name) = decode_endpoint_name(&self.name_words) {
                    self.discovered.name = name;
                    self.name_words.clear();
                }
                true
            }
            UmpStream::ProductInstanceId(_) => {
                self.product_words.extend_from_slice(event.data_words());
                if let Some(id) = decode_product_instance_id(&self.product_words) {
                    self.discovered.product_instance_id = id;
                    self.product_words.clear();
                }
                true
            }
            _ => false,
        }
    }

    /// The assembled endpoint, or `None` until the Endpoint Info notification has
    /// been ingested (the minimum for a meaningful result).
    pub fn result(&self) -> Option<&DiscoveredEndpoint> {
        self.saw_info.then_some(&self.discovered)
    }
}

/// Decode a (possibly multi-packet) Endpoint Name from accumulated UMP-Stream
/// words, or `None` if the words don't yet form a complete message.
fn decode_endpoint_name(words: &[u32]) -> Option<String> {
    match UmpMessage::try_from(words).ok()? {
        UmpMessage::UmpStream(UmpStream::EndpointName(m)) => Some(m.name()),
        _ => None,
    }
}

/// Decode a (possibly multi-packet) Product Instance Id from accumulated
/// UMP-Stream words, or `None` if not yet complete.
fn decode_product_instance_id(words: &[u32]) -> Option<String> {
    match UmpMessage::try_from(words).ok()? {
        UmpMessage::UmpStream(UmpStream::ProductInstanceId(m)) => Some(m.id()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_midi_types::midi2::flex_data::FlexData;

    fn negotiator() -> EndpointNegotiator {
        EndpointNegotiator::new(
            DeviceIdentity {
                manufacturer: [0x00, 0x21, 0x09],
                family: 0x1234,
                family_model: 0x0001,
                software_version: [1, 0, 0, 0],
            },
            vec![FunctionBlock {
                block_number: 0,
                first_group: 0,
                num_groups: 1,
                direction: FunctionBlockDirection::Bidirectional,
            }],
        )
        .with_names("Tutti", "tutti-0001")
    }

    #[test]
    fn responds_to_full_discovery_with_all_notifications() {
        let n = negotiator();
        let replies = n.respond_to(&EndpointNegotiator::discovery_request());
        // info + identity + name + product-id + stream-config + one function block.
        assert_eq!(replies.len(), 6);
        assert!(matches!(
            UmpMessage::try_from(replies[0].data_words()).unwrap(),
            UmpMessage::UmpStream(UmpStream::EndpointInfo(_))
        ));
        assert!(matches!(
            UmpMessage::try_from(replies[1].data_words()).unwrap(),
            UmpMessage::UmpStream(UmpStream::DeviceIdentity(_))
        ));
        assert!(matches!(
            UmpMessage::try_from(replies[2].data_words()).unwrap(),
            UmpMessage::UmpStream(UmpStream::EndpointName(_))
        ));
        assert!(matches!(
            UmpMessage::try_from(replies[3].data_words()).unwrap(),
            UmpMessage::UmpStream(UmpStream::ProductInstanceId(_))
        ));
        assert!(matches!(
            UmpMessage::try_from(replies[4].data_words()).unwrap(),
            UmpMessage::UmpStream(UmpStream::StreamConfigurationNotification(_))
        ));
        assert!(matches!(
            UmpMessage::try_from(replies[5].data_words()).unwrap(),
            UmpMessage::UmpStream(UmpStream::FunctionBlockInfo(_))
        ));
    }

    #[test]
    fn empty_name_omits_its_notification() {
        // Default (no with_names) → name/product-id replies are skipped.
        let n = EndpointNegotiator::new(
            DeviceIdentity {
                manufacturer: [0, 0, 0],
                family: 0,
                family_model: 0,
                software_version: [0; 4],
            },
            vec![],
        );
        let replies = n.respond_to(&EndpointNegotiator::discovery_request());
        // info + identity + stream-config, no name/product-id, no blocks.
        assert_eq!(replies.len(), 3);
        assert!(replies.iter().all(|e| !matches!(
            UmpMessage::try_from(e.data_words()).unwrap(),
            UmpMessage::UmpStream(UmpStream::EndpointName(_))
                | UmpMessage::UmpStream(UmpStream::ProductInstanceId(_))
        )));
    }

    #[test]
    fn endpoint_info_reports_block_count_and_capabilities() {
        let n = negotiator();
        let info = n.endpoint_info();
        let UmpMessage::UmpStream(UmpStream::EndpointInfo(m)) =
            UmpMessage::try_from(info.data_words()).unwrap()
        else {
            panic!("expected EndpointInfo");
        };
        assert_eq!(u8::from(m.number_of_function_blocks()), 1);
        assert!(m.supports_midi2_protocol());
        assert!(m.supports_midi1_protocol());
    }

    #[test]
    fn partial_discovery_only_returns_requested_plus_blocks() {
        let n = negotiator();
        // Ask for identity only.
        let req = MidiEvent::endpoint_discovery(
            1,
            1,
            EndpointDiscoveryRequest::DEVICE_IDENTITY,
        );
        let replies = n.respond_to(&req);
        // identity + the (always-announced) function block.
        assert_eq!(replies.len(), 2);
        assert!(matches!(
            UmpMessage::try_from(replies[0].data_words()).unwrap(),
            UmpMessage::UmpStream(UmpStream::DeviceIdentity(_))
        ));
    }

    #[test]
    fn inquiry_reconstructs_responder_from_its_reply_stream() {
        // Full loopback: the responder's replies to a discovery, fed back into
        // the discoverer, reconstruct the responder's declared identity/topology.
        let n = negotiator();
        let replies = n.respond_to(&EndpointInquiry::request());

        let mut inquiry = EndpointInquiry::new();
        for r in &replies {
            inquiry.ingest(r);
        }
        let d = inquiry.result().expect("Endpoint Info was in the reply stream");

        assert_eq!(d.ump_version, Some(UmpVersion::V1_1));
        assert!(d.capabilities.contains(EndpointCapabilities::MIDI2_PROTOCOL));
        assert!(d.capabilities.contains(EndpointCapabilities::MIDI1_PROTOCOL));
        assert_eq!(d.protocol, Some(Protocol::Midi2));
        assert_eq!(
            d.identity,
            Some(DeviceIdentity {
                manufacturer: [0x00, 0x21, 0x09],
                family: 0x1234,
                family_model: 0x0001,
                software_version: [1, 0, 0, 0],
            })
        );
        assert_eq!(d.name, "Tutti");
        assert_eq!(d.product_instance_id, "tutti-0001");
        assert_eq!(d.function_blocks.len(), 1);
        assert_eq!(d.function_blocks[0].block_number, 0);
        assert_eq!(
            d.function_blocks[0].direction,
            FunctionBlockDirection::Bidirectional
        );
    }

    #[test]
    fn inquiry_has_no_result_until_endpoint_info_seen() {
        let mut inquiry = EndpointInquiry::new();
        // A device-identity reply alone isn't enough.
        inquiry.ingest(&MidiEvent::device_identity([1, 2, 3], 4, 5, [6, 7, 8, 9]));
        assert!(inquiry.result().is_none());
        // A non-UMP-Stream event is ignored.
        assert!(!inquiry.ingest(&MidiEvent::note_on(0, 0, 60, 0x8000)));
    }

    #[test]
    fn non_discovery_event_yields_no_reply() {
        let n = negotiator();
        assert!(n.respond_to(&MidiEvent::note_on(0, 0, 60, 0x8000)).is_empty());
        // A Flex message isn't UMP Stream either.
        let ev = MidiEvent::flex_set_tempo(0, 120.0);
        assert!(matches!(
            UmpMessage::try_from(ev.data_words()).unwrap(),
            UmpMessage::FlexData(FlexData::SetTempo(_))
        ));
        assert!(n.respond_to(&ev).is_empty());
    }
}
