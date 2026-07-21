//! UMP Stream (MT=0xF): Endpoint Discovery + Function Blocks.
//!
//! UMP Stream messages configure the endpoint itself (protocol negotiation,
//! endpoint & Function Block topology) rather than carrying musical data
//! (M2-104 §7.1.1). The JR Timestamp constructor lives in the parent module;
//! Start/End-of-Clip (also UMP Stream) are built by the clip-file codec.

use bitflags::bitflags;
use midi2::prelude::*;

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
        m.set_request_device_identity(
            request.contains(EndpointDiscoveryRequest::DEVICE_IDENTITY),
        );
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
}

/// Direction of a Function Block, for [`MidiEvent::function_block_info`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FunctionBlockDirection {
    Input,
    Output,
    Bidirectional,
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
}
