//! What one UMP Endpoint declares, and the answers it gives (M2-104 §7.1).
//!
//! An [`Endpoint`] is a value: the UMP version, the protocols and Jitter
//! Reduction (JR) timestamps it supports and has active, who it is, and its
//! Function Blocks. [`Endpoint::reply`] is the answering side as one pure step
//! over a received message, with no allocation. The asking side is
//! [`Connection`](crate::Connection).

use tutti_ump::{
    BlockFilter, BlockSelect, DeviceIdentity, Direction as BlockDirection, DiscoveryFilter,
    FirstGroup, FunctionBlockId, FunctionBlockInfo, GroupSpan, Message, Midi1Port, Protocol,
    Stream, StreamConfig, U7, UiHint, Ump, UmpVersion,
};

use crate::name::{BlockName, EndpointName, ProductInstanceId, packets};

/// How many Function Blocks an Endpoint holds (M2-104 §7.1.7: numbers `0..=31`).
pub const MAX_FUNCTION_BLOCKS: usize = 32;

/// The UMP version this core speaks (M2-104 Table 5: 1.1).
pub const UMP_VERSION: UmpVersion = UmpVersion { major: 1, minor: 1 };

/// What an Endpoint supports (M2-104 §7.1.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// Sends and receives the MIDI 1.0 Protocol (`M1`).
    pub midi1: bool,
    /// Sends and receives the MIDI 2.0 Protocol (`M2`).
    pub midi2: bool,
    /// Receives JR Timestamps (`RXJR`).
    pub rx_jr: bool,
    /// Sends JR Timestamps (`TXJR`).
    pub tx_jr: bool,
}

/// One Function Block (M2-104 §7.1.8): a set of Groups that act as one
/// function of the Endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FunctionBlock {
    id: FunctionBlockId,
    active: bool,
    direction: BlockDirection,
    ui_hint: UiHint,
    midi1: Midi1Port,
    first_group: FirstGroup,
    groups: GroupSpan,
    midi_ci_version: u8,
    max_sysex8_streams: u8,
    pub(crate) name: Option<BlockName>,
}

impl FunctionBlock {
    /// An active block of `groups` Groups from `first_group`, with no UI hint,
    /// not a MIDI 1.0 port, no MIDI-CI and no SysEx 8.
    ///
    /// `None` when it would run past Group 16 (`first_group + groups > 16`),
    /// or when `direction` is the reserved `0b00`.
    #[must_use]
    pub fn new(
        id: FunctionBlockId,
        direction: BlockDirection,
        first_group: FirstGroup,
        groups: GroupSpan,
    ) -> Option<Self> {
        let reserved = matches!(direction, BlockDirection::Reserved(_));
        if reserved || u16::from(first_group.get()) + u16::from(groups.get()) > 16 {
            return None;
        }
        Some(Self {
            id,
            active: true,
            direction,
            ui_hint: UiHint::Unknown,
            midi1: Midi1Port::No,
            first_group,
            groups,
            midi_ci_version: 0,
            max_sysex8_streams: 0,
            name: None,
        })
    }

    /// The block with `active` set. A block of an Endpoint that declares its
    /// blocks static must stay active (§7.1.8); this core does not check it.
    #[must_use]
    pub const fn with_active(mut self, active: bool) -> Self {
        self.active = active;
        self
    }

    /// The block with a UI hint.
    #[must_use]
    pub const fn with_ui_hint(mut self, hint: UiHint) -> Self {
        self.ui_hint = hint;
        self
    }

    /// The block declared a MIDI 1.0 port. `None` for the reserved `0x03`, or
    /// for a port over more than one Group (§7.1.8: a port spans one).
    #[must_use]
    pub fn with_midi1(mut self, port: Midi1Port) -> Option<Self> {
        match port {
            Midi1Port::No => {}
            Midi1Port::Unrestricted | Midi1Port::Restricted if self.groups.get() == 1 => {}
            _ => return None,
        }
        self.midi1 = port;
        Some(self)
    }

    /// The block with its MIDI-CI Message Version/Format (`0` is none, §7.1.8)
    /// and the number of simultaneous SysEx 8 streams it supports (`0` none,
    /// `1` one, `2..=255` that many).
    #[must_use]
    pub const fn with_midi_ci(mut self, version: u8, max_sysex8_streams: u8) -> Self {
        self.midi_ci_version = version;
        self.max_sysex8_streams = max_sysex8_streams;
        self
    }

    /// The block with a name.
    #[must_use]
    pub const fn with_name(mut self, name: BlockName) -> Self {
        self.name = Some(name);
        self
    }

    /// Its number.
    #[must_use]
    pub const fn id(&self) -> FunctionBlockId {
        self.id
    }

    /// Whether it is active.
    #[must_use]
    pub const fn active(&self) -> bool {
        self.active
    }

    /// Its direction.
    #[must_use]
    pub const fn direction(&self) -> BlockDirection {
        self.direction
    }

    /// Its UI hint.
    #[must_use]
    pub const fn ui_hint(&self) -> UiHint {
        self.ui_hint
    }

    /// Whether it is a MIDI 1.0 port.
    #[must_use]
    pub const fn midi1(&self) -> Midi1Port {
        self.midi1
    }

    /// The first Group it spans.
    #[must_use]
    pub const fn first_group(&self) -> FirstGroup {
        self.first_group
    }

    /// How many Groups it spans.
    #[must_use]
    pub const fn groups(&self) -> GroupSpan {
        self.groups
    }

    /// Its MIDI-CI Message Version/Format, `0` for none.
    #[must_use]
    pub const fn midi_ci_version(&self) -> u8 {
        self.midi_ci_version
    }

    /// Its simultaneous SysEx 8 streams.
    #[must_use]
    pub const fn max_sysex8_streams(&self) -> u8 {
        self.max_sysex8_streams
    }

    /// Its name, when it has one.
    #[must_use]
    pub const fn name(&self) -> Option<&BlockName> {
        self.name.as_ref()
    }

    fn info(&self) -> FunctionBlockInfo {
        FunctionBlockInfo {
            active: self.active,
            id: self.id,
            direction: self.direction,
            ui_hint: self.ui_hint,
            midi1: self.midi1,
            first_group: self.first_group,
            groups_spanned: self.groups,
            midi_ci_version: self.midi_ci_version,
            max_sysex8_streams: self.max_sysex8_streams,
        }
    }

    /// The block a peer's Function Block Info describes, or `None` when a
    /// field the wire kept is out of range: a number above 31, a Group span
    /// off the end, or a reserved direction.
    pub(crate) fn from_info(info: &FunctionBlockInfo) -> Option<Self> {
        let id = FunctionBlockId::new(info.id.get())?;
        let first_group = FirstGroup::new(info.first_group.get())?;
        let groups = GroupSpan::new(info.groups_spanned.get())?;
        let mut block = Self::new(id, info.direction, first_group, groups)?;
        block.active = info.active;
        block.ui_hint = info.ui_hint;
        // A peer's reserved MIDI 1.0 value, or a port over several Groups, is
        // kept as "not a port": the block is still usable as MIDI 2.0.
        block.midi1 = match info.midi1 {
            Midi1Port::Unrestricted | Midi1Port::Restricted if groups.get() == 1 => info.midi1,
            _ => Midi1Port::No,
        };
        block.midi_ci_version = info.midi_ci_version;
        block.max_sysex8_streams = info.max_sysex8_streams;
        Some(block)
    }
}

/// What one UMP Endpoint declares and has active (M2-104 §7.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub(crate) version: UmpVersion,
    pub(crate) capabilities: Capabilities,
    pub(crate) active: StreamConfig,
    pub(crate) identity: Option<DeviceIdentity>,
    pub(crate) name: Option<EndpointName>,
    pub(crate) instance_id: Option<ProductInstanceId>,
    pub(crate) static_function_blocks: bool,
    /// The count Endpoint Info declares: `0..=32`.
    pub(crate) declared_blocks: u8,
    pub(crate) blocks: [Option<FunctionBlock>; MAX_FUNCTION_BLOCKS],
}

const MIDI1_NO_JR: StreamConfig = StreamConfig {
    protocol: Protocol::Midi1,
    rx_jr: false,
    tx_jr: false,
};

impl Endpoint {
    /// An Endpoint of UMP version [`UMP_VERSION`] with these capabilities, no
    /// Function Block and no name.
    ///
    /// It starts in the MIDI 1.0 Protocol (MIDI 2.0 when that is all it
    /// supports) with JR Timestamps off. The spec names no default; this is
    /// ours, and [`Endpoint::reply`] changes it on a Stream Configuration
    /// Request.
    #[must_use]
    pub const fn new(capabilities: Capabilities) -> Self {
        let protocol = if capabilities.midi2 && !capabilities.midi1 {
            Protocol::Midi2
        } else {
            Protocol::Midi1
        };
        Self {
            version: UMP_VERSION,
            capabilities,
            active: StreamConfig {
                protocol,
                rx_jr: false,
                tx_jr: false,
            },
            identity: None,
            name: None,
            instance_id: None,
            static_function_blocks: false,
            declared_blocks: 0,
            blocks: [None; MAX_FUNCTION_BLOCKS],
        }
    }

    /// A MIDI 1.0 device (design 017 §5): no Stream messages exist for it, so
    /// it is an Endpoint with the MIDI 1.0 Protocol and one inferred Function
    /// Block, number 0, active, bidirectional, Group 0 alone, a MIDI 1.0 port
    /// of unrestricted bandwidth, with a UI hint of both. Its blocks are
    /// static.
    #[must_use]
    pub fn midi1_device(name: Option<EndpointName>) -> Self {
        let mut endpoint = Self::new(Capabilities {
            midi1: true,
            ..Capabilities::default()
        })
        .with_static_function_blocks(true);
        endpoint.name = name;
        endpoint.insert_inferred_block();
        endpoint
    }

    fn insert_inferred_block(&mut self) {
        let (Some(id), Some(first), Some(span)) = (
            FunctionBlockId::new(0),
            FirstGroup::new(0),
            GroupSpan::new(1),
        ) else {
            return;
        };
        let block = FunctionBlock::new(id, BlockDirection::Bidirectional, first, span)
            .map(|b| b.with_ui_hint(UiHint::Both))
            .and_then(|b| b.with_midi1(Midi1Port::Unrestricted));
        if let Some(block) = block {
            self.insert_block(block);
        }
    }

    /// An Endpoint with nothing known, for a discoverer to fill.
    pub(crate) const fn blank() -> Self {
        let mut endpoint = Self::new(Capabilities {
            midi1: false,
            midi2: false,
            rx_jr: false,
            tx_jr: false,
        });
        endpoint.version = UmpVersion { major: 0, minor: 0 };
        endpoint
    }

    /// The Endpoint with the Protocol and JR Timestamps in use now set, for an
    /// Endpoint whose state an OS reports rather than one that was asked
    /// (§7.1.6.3). Nothing is checked against its capabilities.
    #[must_use]
    pub const fn with_active(mut self, active: StreamConfig) -> Self {
        self.active = active;
        self
    }

    /// The Endpoint with a Device Identity (§7.1.3).
    #[must_use]
    pub const fn with_identity(mut self, identity: DeviceIdentity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// The Endpoint with a name (§7.1.4).
    #[must_use]
    pub const fn with_name(mut self, name: EndpointName) -> Self {
        self.name = Some(name);
        self
    }

    /// The Endpoint with a Product Instance Id (§7.1.5).
    #[must_use]
    pub const fn with_instance_id(mut self, id: ProductInstanceId) -> Self {
        self.instance_id = Some(id);
        self
    }

    /// The Endpoint declaring whether its Function Blocks never change
    /// (§7.1.2).
    #[must_use]
    pub const fn with_static_function_blocks(mut self, is_static: bool) -> Self {
        self.static_function_blocks = is_static;
        self
    }

    /// The Endpoint with a Function Block, replacing the one of that number.
    /// The block count it declares is the highest number plus one.
    #[must_use]
    pub fn with_function_block(mut self, block: FunctionBlock) -> Self {
        self.insert_block(block);
        self
    }

    pub(crate) fn insert_block(&mut self, block: FunctionBlock) {
        let i = usize::from(block.id.get());
        self.blocks[i] = Some(block);
        let count = u8::try_from(i + 1).unwrap_or(u8::MAX);
        self.declared_blocks = self.declared_blocks.max(count);
    }

    /// The UMP version it declares.
    #[must_use]
    pub const fn version(&self) -> UmpVersion {
        self.version
    }

    /// The protocols and JR Timestamps it supports.
    #[must_use]
    pub const fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    /// The Protocol and JR Timestamps in use now (§7.1.6.3).
    #[must_use]
    pub const fn active(&self) -> StreamConfig {
        self.active
    }

    /// Its Device Identity.
    #[must_use]
    pub const fn identity(&self) -> Option<&DeviceIdentity> {
        self.identity.as_ref()
    }

    /// Its name.
    #[must_use]
    pub const fn name(&self) -> Option<&EndpointName> {
        self.name.as_ref()
    }

    /// Its Product Instance Id.
    #[must_use]
    pub const fn instance_id(&self) -> Option<&ProductInstanceId> {
        self.instance_id.as_ref()
    }

    /// Whether it declares its Function Blocks static.
    #[must_use]
    pub const fn static_function_blocks(&self) -> bool {
        self.static_function_blocks
    }

    /// The number of Function Blocks it declares (§7.1.2), `0..=32`.
    #[must_use]
    pub const fn declared_function_blocks(&self) -> u8 {
        self.declared_blocks
    }

    /// Function Block `id`, when it is known.
    #[must_use]
    pub fn function_block(&self, id: FunctionBlockId) -> Option<&FunctionBlock> {
        self.blocks[usize::from(id.get())].as_ref()
    }

    /// The known Function Blocks, by number.
    pub fn function_blocks(&self) -> impl Iterator<Item = &FunctionBlock> {
        self.blocks.iter().flatten()
    }

    /// Answers one received message into `out`, in the order of the spec; the
    /// step allocates nothing itself.
    ///
    /// - Endpoint Discovery (§7.1.1): one reply for each filter bit set, in
    ///   the order Endpoint Info, Device Identity, Endpoint Name, Product
    ///   Instance Id, Stream Configuration Notification. A reply for a part
    ///   the Endpoint does not have (no identity, no name) is left out.
    /// - Function Block Discovery (§7.1.7): Function Block Info (§7.1.8) for
    ///   the block asked for, or for every known block for `0xFF`, then, if
    ///   asked, each Function Block Name (§7.1.9). A number above 31, or of a
    ///   block that is not known, answers nothing (the spec names no reply).
    /// - Stream Configuration Request (§7.1.6.2): each part it can honour is
    ///   taken (a Protocol it supports, JR directions it supports, JR off
    ///   always), and the Notification (§7.1.6.3) reports what is now
    ///   active, which is the current configuration for a part it could not
    ///   change (§7.1.6.1 step 3).
    ///
    /// Any other message answers nothing.
    ///
    /// The requester's UMP version (§7.1.1) changes no answer, and that is the
    /// rule applied: Table 5 gives 1.1 as the first version, and every field
    /// this reads (the five filter bits) is defined in it, so a higher version
    /// is answered with the fields of 1.1 (its appended fields and reserved
    /// bits are not read), and the spec defines no lower one to answer in.
    pub fn reply(&mut self, message: Message, out: &mut impl Extend<Ump>) {
        let Message::Stream(stream) = message else {
            return;
        };
        match stream {
            Stream::EndpointDiscovery { filter, .. } => self.answer_discovery(filter, out),
            Stream::FunctionBlockDiscovery { block, filter } => {
                self.answer_block_discovery(block, filter, out);
            }
            Stream::StreamConfigurationRequest(request) => {
                self.configure(request);
                send(out, Stream::StreamConfigurationNotification(self.active));
            }
            _ => {}
        }
    }

    fn answer_discovery(&self, filter: DiscoveryFilter, out: &mut impl Extend<Ump>) {
        if filter.endpoint_info {
            send(
                out,
                Stream::EndpointInfo(tutti_ump::EndpointInfo {
                    version: self.version,
                    static_function_blocks: self.static_function_blocks,
                    function_blocks: U7::masked(self.declared_blocks),
                    midi2: self.capabilities.midi2,
                    midi1: self.capabilities.midi1,
                    rx_jr: self.capabilities.rx_jr,
                    tx_jr: self.capabilities.tx_jr,
                }),
            );
        }
        if let (true, Some(identity)) = (filter.device_identity, self.identity) {
            send(out, Stream::DeviceIdentity(identity));
        }
        if let (true, Some(name)) = (filter.endpoint_name, &self.name) {
            packets::<14>(name.as_bytes(), |form, b| {
                send(out, Stream::EndpointName(form, b));
            });
        }
        if let (true, Some(id)) = (filter.product_instance_id, &self.instance_id) {
            packets::<14>(id.as_str().as_bytes(), |form, b| {
                send(out, Stream::ProductInstanceId(form, b));
            });
        }
        if filter.stream_configuration {
            send(out, Stream::StreamConfigurationNotification(self.active));
        }
    }

    fn answer_block_discovery(
        &self,
        select: BlockSelect,
        filter: BlockFilter,
        out: &mut impl Extend<Ump>,
    ) {
        let one;
        let asked: &[Option<FunctionBlock>] = match select {
            BlockSelect::All => &self.blocks,
            BlockSelect::One(id) => match self.blocks.get(usize::from(id.get())) {
                Some(slot) => {
                    one = [*slot];
                    &one
                }
                None => return,
            },
        };
        if filter.info {
            for block in asked.iter().flatten() {
                send(out, Stream::FunctionBlockInfo(block.info()));
            }
        }
        if filter.name {
            for block in asked.iter().flatten() {
                if let Some(name) = &block.name {
                    packets::<13>(name.as_bytes(), |form, bytes| {
                        send(
                            out,
                            Stream::FunctionBlockName {
                                block: block.id,
                                form,
                                bytes,
                            },
                        );
                    });
                }
            }
        }
    }

    fn configure(&mut self, request: StreamConfig) {
        let caps = self.capabilities;
        self.active.protocol = match request.protocol {
            Protocol::Midi1 if caps.midi1 => Protocol::Midi1,
            Protocol::Midi2 if caps.midi2 => Protocol::Midi2,
            _ => self.active.protocol,
        };
        // Turning JR off is always possible; on, only where it is supported.
        if !request.rx_jr || caps.rx_jr {
            self.active.rx_jr = request.rx_jr;
        }
        if !request.tx_jr || caps.tx_jr {
            self.active.tx_jr = request.tx_jr;
        }
    }

    /// Folds a peer's Endpoint Info into this (discoverer) value.
    pub(crate) fn apply_info(&mut self, info: &tutti_ump::EndpointInfo) {
        self.version = info.version;
        self.static_function_blocks = info.static_function_blocks;
        self.declared_blocks = info
            .function_blocks
            .get()
            .min(u8::try_from(MAX_FUNCTION_BLOCKS).unwrap_or(32));
        self.capabilities = Capabilities {
            midi1: info.midi1,
            midi2: info.midi2,
            rx_jr: info.rx_jr,
            tx_jr: info.tx_jr,
        };
    }

    /// What a MIDI 1.0 device is: the Endpoint as the discoverer knows it, but
    /// in the MIDI 1.0 Protocol with no JR Timestamps, and with the inferred
    /// block if no block is known.
    pub(crate) fn fall_back_to_midi1(&mut self) {
        self.active = MIDI1_NO_JR;
        self.capabilities.midi1 = true;
        if self.function_blocks().next().is_none() {
            self.declared_blocks = 0;
            self.insert_inferred_block();
        }
    }
}

fn send(out: &mut impl Extend<Ump>, stream: Stream) {
    out.extend(std::iter::once(Ump::from(Message::Stream(stream))));
}

#[cfg(test)]
mod tests;
