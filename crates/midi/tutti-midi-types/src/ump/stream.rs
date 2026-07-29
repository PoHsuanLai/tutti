//! UMP Stream (MT=0xF): Endpoint Discovery + Function Blocks.
//!
//! UMP Stream messages configure the endpoint itself (protocol negotiation,
//! endpoint & Function Block topology) rather than carrying musical data
//! (M2-104 §7.1.1). The JR Timestamp constructor lives in the parent module;
//! Start/End-of-Clip (also UMP Stream) are built by the clip-file codec.

use bitflags::bitflags;
use midi2::prelude::*;
use midi2::Data;

use super::MidiEvent;

bitflags! {
    /// Which Endpoint Discovery replies to request (M2-104 §7.1.1). Each bit
    /// asks the peer for one reply message; OR them together.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct EndpointDiscoveryRequest: u8 {
        const ENDPOINT_INFO         = 1 << 0;
        const DEVICE_IDENTITY       = 1 << 1;
        const ENDPOINT_NAME         = 1 << 2;
        const PRODUCT_INSTANCE_ID    = 1 << 3;
        const STREAM_CONFIGURATION  = 1 << 4;
    }
}

impl MidiEvent {
    /// UMP Stream **Endpoint Discovery** — the protocol-negotiation request an
    /// endpoint sends to learn a peer's capabilities. `ump_major`/`ump_minor`
    /// are the supported UMP version; `request` selects which replies to ask for.
    #[inline]
    pub fn endpoint_discovery(
        ump_major: u8,
        ump_minor: u8,
        request: EndpointDiscoveryRequest,
    ) -> Self {
        use midi2::ump_stream::EndpointDiscovery;
        let mut m = EndpointDiscovery::<[u32; 4]>::new();
        m.set_ump_version_major(ump_major);
        m.set_ump_version_minor(ump_minor);
        m.set_request_endpoint_info(request.contains(EndpointDiscoveryRequest::ENDPOINT_INFO));
        m.set_request_device_identity(request.contains(EndpointDiscoveryRequest::DEVICE_IDENTITY));
        m.set_request_endpoint_name(request.contains(EndpointDiscoveryRequest::ENDPOINT_NAME));
        m.set_request_product_instance_id(
            request.contains(EndpointDiscoveryRequest::PRODUCT_INSTANCE_ID),
        );
        m.set_request_stream_configuration(
            request.contains(EndpointDiscoveryRequest::STREAM_CONFIGURATION),
        );
        Self::from_ump(0, m.data())
    }

    /// UMP Stream **Function Block Info** — declares one Function Block: whether
    /// it is `active`, its `block_number`, the `first_group` it spans and how
    /// many groups (`num_groups`), and its `direction`
    /// (input/output/bidirectional).
    #[inline]
    pub fn function_block_info(
        active: bool,
        block_number: u8,
        first_group: u8,
        num_groups: u8,
        direction: FunctionBlockDirection,
    ) -> Self {
        use midi2::ump_stream::{Direction, FunctionBlockInfo};
        let mut m = FunctionBlockInfo::<[u32; 4]>::new();
        m.set_active(active);
        m.set_function_block_number(u7::new(block_number & 0x7F));
        m.set_first_group(u4::new(first_group & 0x0F));
        m.set_number_of_groups_spanned(num_groups);
        m.set_direction(match direction {
            FunctionBlockDirection::Input => Direction::Input,
            FunctionBlockDirection::Output => Direction::Output,
            FunctionBlockDirection::Bidirectional => Direction::Bidirectional,
        });
        Self::from_ump(0, m.data())
    }

    /// UMP Stream **Endpoint Info Notification** — the reply to an Endpoint
    /// Discovery `request_endpoint_info`. Declares this endpoint's UMP version,
    /// its Function Blocks (count + whether the set is `static`), and which
    /// protocols / JR-timestamp directions it [`supports`](EndpointCapabilities).
    #[inline]
    pub fn endpoint_info(
        ump_version: UmpVersion,
        function_blocks: FunctionBlocks,
        supports: EndpointCapabilities,
    ) -> Self {
        use midi2::ump_stream::EndpointInfo;
        let mut m = EndpointInfo::<[u32; 4]>::new();
        m.set_ump_version_major(ump_version.major);
        m.set_ump_version_minor(ump_version.minor);
        m.set_static_function_blocks(function_blocks.is_static);
        m.set_number_of_function_blocks(u7::new(function_blocks.count & 0x7F));
        m.set_supports_midi2_protocol(supports.contains(EndpointCapabilities::MIDI2_PROTOCOL));
        m.set_supports_midi1_protocol(supports.contains(EndpointCapabilities::MIDI1_PROTOCOL));
        m.set_supports_sending_jr_timestamps(supports.contains(EndpointCapabilities::SEND_JR));
        m.set_supports_receiving_jr_timestamps(supports.contains(EndpointCapabilities::RECEIVE_JR));
        Self::from_ump(0, m.data())
    }

    /// UMP Stream **Stream Configuration Notification** — the reply to an
    /// Endpoint Discovery `request_stream_configuration`, and the message an
    /// endpoint sends when it changes protocol. `jr` selects which JR-timestamp
    /// directions are active on the configured [`Protocol`].
    #[inline]
    pub fn stream_configuration_notification(protocol: Protocol, jr: JrTimestamps) -> Self {
        use midi2::ump_stream::StreamConfigurationNotification;
        let mut m = StreamConfigurationNotification::<[u32; 4]>::new();
        m.set_protocol(protocol as u8);
        m.set_receive_jr_timestamps(jr.contains(JrTimestamps::RECEIVE));
        m.set_send_jr_timestamps(jr.contains(JrTimestamps::SEND));
        Self::from_ump(0, m.data())
    }

    /// UMP Stream **Device Identity Notification** — the reply to an Endpoint
    /// Discovery `request_device_identity`. Carries the SysEx-style device id:
    /// a 3-byte `manufacturer`, 14-bit `family` and `family_model`, and a 4-byte
    /// `software_version`.
    #[inline]
    pub fn device_identity(
        manufacturer: [u8; 3],
        family: u16,
        family_model: u16,
        software_version: [u8; 4],
    ) -> Self {
        use midi2::ump_stream::DeviceIdentity;
        let mut m = DeviceIdentity::<[u32; 4]>::new();
        m.set_device_manufacturer(manufacturer.map(|b| u7::new(b & 0x7F)));
        m.set_device_family(u14::new(family & 0x3FFF));
        m.set_device_family_model_number(u14::new(family_model & 0x3FFF));
        m.set_software_version(software_version.map(|b| u7::new(b & 0x7F)));
        Self::from_ump(0, m.data())
    }
}

/// Split a multi-packet UMP-Stream message's words into 4-word [`MidiEvent`]s,
/// appended to `out`. UMP-Stream text messages (name / product id) can span
/// several 128-bit packets; each becomes one `MidiEvent`.
fn push_ump_stream_packets(words: &[u32], out: &mut Vec<MidiEvent>) {
    for packet in words.chunks(4) {
        out.push(MidiEvent::from_ump(0, packet));
    }
}

/// UMP Stream **Endpoint Name Notification** — the reply to an Endpoint
/// Discovery `request_endpoint_name`. `name` is UTF-8 and may span several
/// packets, so this appends one or more [`MidiEvent`]s to `out` (mirroring
/// [`MidiEvent::sysex7_fragments`]).
pub fn endpoint_name(name: &str, out: &mut Vec<MidiEvent>) {
    use midi2::ump_stream::EndpointName;
    let mut m = EndpointName::<Vec<u32>>::new();
    m.set_name(name);
    push_ump_stream_packets(m.data(), out);
}

/// UMP Stream **Product Instance Id Notification** — the reply to an Endpoint
/// Discovery `request_product_instance_id`. Like [`endpoint_name`], the id may
/// span several packets appended to `out`.
pub fn product_instance_id(id: &str, out: &mut Vec<MidiEvent>) {
    use midi2::ump_stream::ProductInstanceId;
    let mut m = ProductInstanceId::<Vec<u32>>::new();
    m.set_id(id);
    push_ump_stream_packets(m.data(), out);
}

/// UMP Stream **Function Block Name Notification** (status 0x12) for the block
/// at `block_number`.
///
/// [`MidiEvent::function_block_info`] carries a block's *topology* — group span
/// and direction — but not its name, so without this a discovered endpoint shows
/// "Block 0/1/2" in a device picker instead of "Keys"/"Drums". Like
/// [`endpoint_name`], the name is UTF-8 and may span several packets.
pub fn function_block_name(block_number: u8, name: &str, out: &mut Vec<MidiEvent>) {
    use midi2::ump_stream::FunctionBlockName;
    let mut m = FunctionBlockName::<Vec<u32>>::new();
    // Order matters: the block number is repeated in octet 2 of *every* packet
    // (midi2 validates that they agree on read), and `set_function_block` only
    // stamps the packets that exist when it runs. Setting the name first sizes
    // the buffer, so the number reaches them all.
    m.set_name(name);
    m.set_function_block(block_number);
    push_ump_stream_packets(m.data(), out);
}

/// Direction of a Function Block, for [`MidiEvent::function_block_info`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FunctionBlockDirection {
    Input,
    Output,
    Bidirectional,
}

/// UMP protocol version an endpoint speaks, for [`MidiEvent::endpoint_info`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UmpVersion {
    pub major: u8,
    pub minor: u8,
}

impl UmpVersion {
    /// UMP 1.1 — the version this engine implements.
    pub const V1_1: Self = Self { major: 1, minor: 1 };
}

/// An endpoint's Function Block set, for [`MidiEvent::endpoint_info`]: how many
/// blocks it exposes and whether that set is fixed (`is_static`) or may change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FunctionBlocks {
    pub count: u8,
    pub is_static: bool,
}

/// The MIDI protocol carried on a UMP stream, for
/// [`MidiEvent::stream_configuration_notification`]. Wire values per M2-104.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Protocol {
    Midi1 = 1,
    /// The default — this engine is MIDI-2-native.
    #[default]
    Midi2 = 2,
}

bitflags! {
    /// Which capabilities an endpoint advertises in its Endpoint Info
    /// (M2-104 §7.1.2): supported protocols and JR-timestamp directions.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct EndpointCapabilities: u8 {
        const MIDI2_PROTOCOL = 1 << 0;
        const MIDI1_PROTOCOL = 1 << 1;
        const SEND_JR        = 1 << 2;
        const RECEIVE_JR     = 1 << 3;
    }
}

bitflags! {
    /// Active JR-timestamp directions on a configured stream, for
    /// [`MidiEvent::stream_configuration_notification`].
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct JrTimestamps: u8 {
        const SEND    = 1 << 0;
        const RECEIVE = 1 << 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_discovery_decodes_via_midi2() {
        use midi2::ump_stream::UmpStream;
        use midi2::UmpMessage;
        let ev = MidiEvent::endpoint_discovery(
            1,
            1,
            EndpointDiscoveryRequest::ENDPOINT_INFO | EndpointDiscoveryRequest::ENDPOINT_NAME,
        );
        match UmpMessage::try_from(ev.data_words()).unwrap() {
            UmpMessage::UmpStream(UmpStream::EndpointDiscovery(m)) => {
                assert_eq!(m.ump_version_major(), 1);
                assert_eq!(m.ump_version_minor(), 1);
                assert!(m.request_endpoint_info());
                assert!(!m.request_device_identity());
                assert!(m.request_endpoint_name());
            }
            other => panic!("expected EndpointDiscovery, got {other:?}"),
        }
    }

    #[test]
    fn function_block_info_decodes_via_midi2() {
        use midi2::ump_stream::{Direction, UmpStream};
        use midi2::UmpMessage;
        let ev = MidiEvent::function_block_info(true, 2, 4, 1, FunctionBlockDirection::Output);
        match UmpMessage::try_from(ev.data_words()).unwrap() {
            UmpMessage::UmpStream(UmpStream::FunctionBlockInfo(m)) => {
                assert!(m.active());
                assert_eq!(u8::from(m.function_block_number()), 2);
                assert_eq!(u8::from(m.first_group()), 4);
                assert_eq!(m.number_of_groups_spanned(), 1);
                assert_eq!(m.direction(), Direction::Output);
            }
            other => panic!("expected FunctionBlockInfo, got {other:?}"),
        }
    }

    #[test]
    fn endpoint_info_decodes_via_midi2() {
        use midi2::ump_stream::UmpStream;
        use midi2::UmpMessage;
        let ev = MidiEvent::endpoint_info(
            UmpVersion::V1_1,
            FunctionBlocks {
                count: 3,
                is_static: true,
            },
            EndpointCapabilities::MIDI2_PROTOCOL | EndpointCapabilities::MIDI1_PROTOCOL,
        );
        match UmpMessage::try_from(ev.data_words()).unwrap() {
            UmpMessage::UmpStream(UmpStream::EndpointInfo(m)) => {
                assert_eq!(m.ump_version_major(), 1);
                assert_eq!(u8::from(m.number_of_function_blocks()), 3);
                assert!(m.static_function_blocks());
                assert!(m.supports_midi2_protocol());
                assert!(m.supports_midi1_protocol());
                assert!(!m.supports_sending_jr_timestamps());
            }
            other => panic!("expected EndpointInfo, got {other:?}"),
        }
    }

    #[test]
    fn device_identity_decodes_via_midi2() {
        use midi2::ump_stream::UmpStream;
        use midi2::UmpMessage;
        let ev = MidiEvent::device_identity([0x00, 0x21, 0x09], 0x1234, 0x0001, [1, 2, 3, 4]);
        match UmpMessage::try_from(ev.data_words()).unwrap() {
            UmpMessage::UmpStream(UmpStream::DeviceIdentity(m)) => {
                assert_eq!(u8::from(m.device_manufacturer()[2]), 0x09);
                assert_eq!(u16::from(m.device_family()), 0x1234);
            }
            other => panic!("expected DeviceIdentity, got {other:?}"),
        }
    }

    #[test]
    fn stream_configuration_decodes_via_midi2() {
        use midi2::ump_stream::UmpStream;
        use midi2::UmpMessage;
        let ev = MidiEvent::stream_configuration_notification(
            Protocol::Midi2,
            JrTimestamps::SEND | JrTimestamps::RECEIVE,
        );
        match UmpMessage::try_from(ev.data_words()).unwrap() {
            UmpMessage::UmpStream(UmpStream::StreamConfigurationNotification(m)) => {
                assert_eq!(m.protocol(), 2);
                assert!(m.send_jr_timestamps());
                assert!(m.receive_jr_timestamps());
            }
            other => panic!("expected StreamConfigurationNotification, got {other:?}"),
        }
    }

    #[test]
    fn function_block_name_round_trips() {
        use midi2::ump_stream::UmpStream;
        use midi2::UmpMessage;

        // Short name: one packet, decodes directly.
        let mut out = Vec::new();
        function_block_name(2, "Keys", &mut out);
        assert_eq!(out.len(), 1);
        match UmpMessage::try_from(out[0].data_words()).unwrap() {
            UmpMessage::UmpStream(UmpStream::FunctionBlockName(m)) => {
                assert_eq!(m.function_block(), 2);
                assert_eq!(m.name(), "Keys");
            }
            other => panic!("expected FunctionBlockName, got {other:?}"),
        }

        // A name too long for one packet fragments; reassembling the words
        // recovers it, so the block's label survives however it is split.
        let long = "Grand Piano — Upper Manual, Layered Strings";
        let mut out = Vec::new();
        function_block_name(7, long, &mut out);
        assert!(out.len() > 1, "long name spans packets");
        let words: Vec<u32> = out.iter().flat_map(|e| e.data_words().to_vec()).collect();
        match UmpMessage::try_from(&words[..]).unwrap() {
            UmpMessage::UmpStream(UmpStream::FunctionBlockName(m)) => {
                assert_eq!(m.function_block(), 7);
                assert_eq!(m.name(), long);
            }
            other => panic!("expected FunctionBlockName, got {other:?}"),
        }
    }
}
