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

use tutti_midi_types::midi2::UmpMessage;
use tutti_midi_types::midi2::ump_stream::{Direction, UmpStream};
use tutti_midi_types::ump::{endpoint_name, function_block_name, product_instance_id};
use tutti_midi_types::{
    ALL_FUNCTION_BLOCKS, EndpointCapabilities, EndpointDiscoveryRequest, FunctionBlockDirection,
    FunctionBlockDiscoveryRequest, FunctionBlocks, JrTimestamps, MidiEvent, Protocol, UmpVersion,
};
use tutti_ump::MidiGroup;

/// One Function Block this endpoint exposes (M2-104 §7.1.3).
///
/// `name` is the block's label, reported in a Function Block Name Notification
/// (§7.1.9) when a [`FunctionBlockDiscovery`](MidiEvent::function_block_discovery)
/// asks for it. An empty name omits that reply — a device picker then shows the
/// block number instead. It is not `Copy` because of this field; the topology
/// fields alone were, but a name that must be truncated to stay `Copy` is worse
/// than a clone at discovery time.
// No `Default`: §7.1.8 makes direction 0b00 Reserved, so there is no honest
// default direction to synthesise.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionBlock {
    /// The block's index within the endpoint, `0..=31` (§7.1.8 gives the field
    /// 5 bits). [`ALL_FUNCTION_BLOCKS`] is the wildcard a discovery may name
    /// instead, and is not a valid value here.
    pub block_number: u8,
    /// The first UMP group this block covers.
    pub first_group: MidiGroup,
    /// How many consecutive groups the block spans, starting at `first_group`.
    /// A block owns `first_group ..= first_group + num_groups - 1`.
    pub num_groups: u8,
    /// Which way MIDI flows through the block. §7.1.8 makes `0b00` Reserved, so
    /// there is no honest default — hence no `Default` impl on this type.
    pub direction: FunctionBlockDirection,
    /// The block's label, reported in a Function Block Name Notification. Empty
    /// omits that reply entirely rather than claiming the block is named "".
    pub name: String,
}

/// SysEx-style device identity carried in a Device Identity notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceIdentity {
    /// The MMA/AMEI manufacturer SysEx id. A one-byte id is carried as
    /// `[id, 0, 0]`; a three-byte id occupies all three.
    pub manufacturer: [u8; 3],
    /// Manufacturer-assigned device family code.
    pub family: u16,
    /// Manufacturer-assigned model within [`family`](Self::family).
    pub family_model: u16,
    /// Software revision, four bytes whose meaning is the manufacturer's.
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
    #[must_use]
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
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: EndpointCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Override the currently-configured protocol + active JR-timestamp directions.
    #[must_use]
    pub fn with_stream_config(mut self, protocol: Protocol, jr: JrTimestamps) -> Self {
        self.protocol = protocol;
        self.jr = jr;
        self
    }

    /// Sets the human-readable endpoint name and product-instance id reported in
    /// their respective notifications. Empty strings omit those replies.
    #[must_use]
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
            out.push(Self::block_info(fb));
        }
        out
    }

    /// The Function Block Info notification for one block.
    fn block_info(fb: &FunctionBlock) -> MidiEvent {
        MidiEvent::function_block_info(
            true,
            fb.block_number,
            fb.first_group,
            fb.num_groups,
            fb.direction,
        )
    }

    /// The reply stream for an inbound **Function Block Discovery** `event`
    /// (M2-104 §7.1.7). Returns empty if `event` is not one.
    ///
    /// The requested block is either a single number or
    /// [`ALL_FUNCTION_BLOCKS`], in which case §7.1.8 requires "one Function
    /// Block Info Notification message per Function Block". Each filter bit
    /// produces its own reply (§7.1.7), so a request for both Info and Name on
    /// N blocks yields 2N messages, ordered Info-then-Name per block.
    ///
    /// A block whose `name` is empty contributes no Name notification even when
    /// asked: there is no such thing as a nameless Name message, and an empty
    /// one would claim the block is called "".
    pub fn respond_to_function_block_discovery(&self, event: &MidiEvent) -> Vec<MidiEvent> {
        let Ok(UmpMessage::UmpStream(UmpStream::FunctionBlockDiscovery(d))) =
            UmpMessage::try_from(event.data_words())
        else {
            return Vec::new();
        };

        let requested = d.function_block_number();
        let want_info = d.requesting_function_block_info();
        let want_name = d.requesting_function_block_name();

        let mut out = Vec::new();
        for fb in self
            .function_blocks
            .iter()
            .filter(|fb| requested == ALL_FUNCTION_BLOCKS || fb.block_number == requested)
        {
            if want_info {
                out.push(Self::block_info(fb));
            }
            if want_name && !fb.name.is_empty() {
                function_block_name(fb.block_number, &fb.name, &mut out);
            }
        }
        out
    }

    /// Builds an outbound Function Block Discovery asking `block_number` (or
    /// [`ALL_FUNCTION_BLOCKS`]) for the notifications in `request`. Use when
    /// tutti is the *discoverer* re-querying a peer whose blocks may have
    /// changed.
    pub fn function_block_discovery_request(
        block_number: u8,
        request: FunctionBlockDiscoveryRequest,
    ) -> MidiEvent {
        MidiEvent::function_block_discovery(block_number, request)
    }

    /// Builds the outbound Endpoint Discovery request this endpoint would send to
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
    /// The UMP version the peer declared, or `None` until its Endpoint Info
    /// notification arrives.
    pub ump_version: Option<UmpVersion>,
    /// Which protocols and JR-timestamp directions the peer *can* do. This is
    /// the capability set, not the current configuration — compare against
    /// [`protocol`](Self::protocol) and [`jr`](Self::jr) for what is live.
    pub capabilities: EndpointCapabilities,
    /// The protocol the peer is currently configured for, or `None` until its
    /// Stream Configuration notification arrives.
    pub protocol: Option<Protocol>,
    /// The JR-timestamp directions currently active, from the same Stream
    /// Configuration notification. Empty until it arrives.
    pub jr: JrTimestamps,
    /// The peer's SysEx-style identity, or `None` until its Device Identity
    /// notification arrives.
    pub identity: Option<DeviceIdentity>,
    /// The peer's endpoint name. Empty until its (possibly multi-packet) Name
    /// notification completes, and empty forever for a peer that sends none.
    pub name: String,
    /// The peer's product instance id, on the same terms as
    /// [`name`](Self::name).
    pub product_instance_id: String,
    /// One entry per Function Block Info notification received, in arrival
    /// order. A later Name notification fills in the matching block's
    /// [`name`](FunctionBlock::name).
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
    /// In-progress Function Block Name packet words.
    block_name_words: Vec<u32>,
}

impl EndpointInquiry {
    /// A fresh inquiry with no accumulated state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The outbound Endpoint Discovery this inquiry sends to probe a peer, asking
    /// for every reply. Identical to [`EndpointNegotiator::discovery_request`].
    pub fn request() -> MidiEvent {
        EndpointNegotiator::discovery_request()
    }

    /// Feeds one reply event from the peer. Non-UMP-Stream events are ignored.
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
                caps.set(
                    EndpointCapabilities::MIDI2_PROTOCOL,
                    m.supports_midi2_protocol(),
                );
                caps.set(
                    EndpointCapabilities::MIDI1_PROTOCOL,
                    m.supports_midi1_protocol(),
                );
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
                let block_number = u8::from(m.function_block_number());
                let info = FunctionBlock {
                    block_number,
                    first_group: MidiGroup::masked(u8::from(m.first_group())),
                    num_groups: m.number_of_groups_spanned(),
                    direction: match m.direction() {
                        Direction::Input => FunctionBlockDirection::Input,
                        Direction::Output => FunctionBlockDirection::Output,
                        _ => FunctionBlockDirection::Bidirectional,
                    },
                    // Carried by a separate Name notification, which may arrive
                    // either side of this one.
                    name: String::new(),
                };
                // §7.1.8: a non-static endpoint re-sends Info "when any property
                // in this message has changed", and a Function Block Discovery
                // can re-query one deliberately. Replacing in place keeps the
                // re-query idempotent — pushing would grow a duplicate every
                // time a block was polled. The name is preserved because it
                // travels in its own message and is not restated here.
                match self
                    .discovered
                    .function_blocks
                    .iter_mut()
                    .find(|fb| fb.block_number == block_number)
                {
                    Some(existing) => {
                        let name = std::mem::take(&mut existing.name);
                        *existing = FunctionBlock { name, ..info };
                    }
                    None => self.discovered.function_blocks.push(info),
                }
                true
            }
            UmpStream::FunctionBlockName(_) => {
                self.block_name_words.extend_from_slice(event.data_words());
                if let Some((block_number, name)) =
                    decode_function_block_name(&self.block_name_words)
                {
                    self.block_name_words.clear();
                    match self
                        .discovered
                        .function_blocks
                        .iter_mut()
                        .find(|fb| fb.block_number == block_number)
                    {
                        Some(existing) => existing.name = name,
                        // A Name can arrive before its Info — §7.1.7 lets a
                        // discovery ask for the name alone. Hold it against the
                        // block number so the label is not lost; the topology
                        // fields fill in when Info arrives, which is why the
                        // Info arm preserves an existing name.
                        None => self.discovered.function_blocks.push(FunctionBlock {
                            block_number,
                            first_group: MidiGroup::FIRST,
                            num_groups: 0,
                            direction: FunctionBlockDirection::Bidirectional,
                            name,
                        }),
                    }
                }
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

/// Decode a (possibly multi-packet) Function Block Name from accumulated
/// UMP-Stream words into `(block_number, name)`, or `None` if the words don't
/// yet form a complete message.
fn decode_function_block_name(words: &[u32]) -> Option<(u8, String)> {
    match UmpMessage::try_from(words).ok()? {
        UmpMessage::UmpStream(UmpStream::FunctionBlockName(m)) => {
            Some((m.function_block(), m.name()))
        }
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
mod tests;
