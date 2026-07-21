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
//!
//! **Not answered:** the optional Endpoint Name and Product Instance Id text
//! notifications. Those are variable-length UMP-Stream text messages that need a
//! growable message buffer, which `midi2` only provides under its `std` feature;
//! tutti builds `midi2` `no_std`, so they are omitted. A discovery that requests
//! *only* those receives no reply — spec-legal, since every text notification is
//! optional.

use tutti_midi_types::midi2::ump_stream::UmpStream;
use tutti_midi_types::midi2::UmpMessage;
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
    function_blocks: Vec<FunctionBlock>,
}

impl EndpointNegotiator {
    /// A negotiator for an endpoint speaking UMP 1.1 with the given identity and
    /// Function Blocks, advertising MIDI-2 + MIDI-1 protocol support (no JR
    /// timestamps). Adjust with the `with_*` setters.
    pub fn new(identity: DeviceIdentity, function_blocks: Vec<FunctionBlock>) -> Self {
        Self {
            ump_version: UmpVersion::V1_1,
            capabilities: EndpointCapabilities::MIDI2_PROTOCOL
                | EndpointCapabilities::MIDI1_PROTOCOL,
            protocol: Protocol::Midi2,
            jr: JrTimestamps::empty(),
            identity,
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
    }

    #[test]
    fn responds_to_full_discovery_with_all_notifications() {
        let n = negotiator();
        let replies = n.respond_to(&EndpointNegotiator::discovery_request());
        // info + identity + stream-config + one function block.
        assert_eq!(replies.len(), 4);
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
            UmpMessage::UmpStream(UmpStream::StreamConfigurationNotification(_))
        ));
        assert!(matches!(
            UmpMessage::try_from(replies[3].data_words()).unwrap(),
            UmpMessage::UmpStream(UmpStream::FunctionBlockInfo(_))
        ));
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
