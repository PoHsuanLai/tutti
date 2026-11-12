//! The discovering side of a UMP connection (M2-100 Table 4, steps 2 to 4;
//! M2-104 §7.1.6.1).
//!
//! A [`Connection`] is a state machine over received messages and time. It
//! reads no clock and sends nothing itself: every step returns an [`Action`]
//! (a packet to send, or the result), and time comes in as an [`Instant`]
//! value from whoever drives it.
//!
//! 1. **Endpoint Discovery** (Table 4 step 2): ask for everything, fold Endpoint
//!    Info, Device Identity, Endpoint Name, Product Instance Id and the Stream
//!    Configuration Notification into an [`Endpoint`].
//! 2. **Select Protocol** (step 3): if the Notification does not match what is
//!    wanted, narrowed to what the Endpoint supports, send a Stream
//!    Configuration Request and wait for the Notification, which is then the
//!    truth (§7.1.6.1 step 3: the peer may refuse any part).
//! 3. **Function Blocks** (step 4): ask for every block's Info and Name.
//!
//! Step 5, MIDI-CI Discovery, is the CI codec's and has no state machine yet
//! (design 017 §5, "MIDI-CI").
//!
//! A step that gets no answer within [`STEP_TIMEOUT`] is sent again, up to
//! [`MAX_ATTEMPTS`] times in all. Then Discovery and Select Protocol give up
//! and the peer is a MIDI 1.0 device ([`Ending::Midi1Fallback`]); Function
//! Blocks settle with the blocks that arrived. Once every block's Info is in,
//! the Names have [`NAME_GRACE`] to follow, since a block with no name sends
//! none.

use std::time::{Duration, Instant};

use bitflags::bitflags;
use tutti_ump::{
    BlockFilter, BlockSelect, DiscoveryFilter, Form, Message, Protocol, Stream, StreamConfig, Ump,
};

use crate::endpoint::{Endpoint, MAX_FUNCTION_BLOCKS, UMP_VERSION};
use crate::name::{Assembler, BlockName, EndpointName, ProductInstanceId};

/// How long a step waits for its answer before it is sent again.
///
/// M2-104 and M2-100 give no timeout for the UMP Stream steps (the 3 s of
/// M2-101 §5.5.5 is MIDI-CI Discovery's). One second is ours: UMP Stream
/// replies come from the peer's firmware or driver, not a user.
pub const STEP_TIMEOUT: Duration = Duration::from_secs(1);

/// How long Function Block discovery waits for Names once every Info is in. A
/// block with no name never sends one (§7.1.9: "may"), so a Name is not
/// something to wait for without end; when every block's Info and Name are in
/// it settles at once. 100 ms is ours.
pub const NAME_GRACE: Duration = Duration::from_millis(100);

/// How many times a step is sent in all, the first included. Ours: three
/// attempts, so a silent peer settles as MIDI 1.0 after
/// `3 x` [`STEP_TIMEOUT`].
pub const MAX_ATTEMPTS: u8 = 3;

/// How a connection ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    /// The steps ran: the Endpoint's active configuration is what its last
    /// Stream Configuration Notification reported, whether or not that is what
    /// was wanted. The Function Blocks are those that were reported.
    Negotiated,
    /// A step went unanswered: the peer is treated as a MIDI 1.0 device,
    /// whatever it had said (MIDI 1.0 Protocol, no JR Timestamps, and one
    /// inferred Function Block if none was reported).
    Midi1Fallback,
}

/// What the driver does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Nothing; wait for a message, or for the [`deadline`](Connection::deadline).
    Wait,
    /// Send this packet to the peer.
    Send(Ump),
    /// The connection is over; read [`Connection::endpoint`].
    Settled(Ending),
    /// After a [`Negotiated`](Ending::Negotiated) settle, the peer told
    /// something that changes what [`Connection::endpoint`] holds: a Stream
    /// Configuration Notification that moves the Protocol or JR Timestamps in
    /// use, or (when the Endpoint did not declare its Function Blocks static)
    /// a Function Block Info or Name that differs from what is held.
    Changed,
}

bitflags! {
    /// What Discovery has heard that it cannot go on without.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Heard: u8 {
        const INFO = 1;
        const CONFIG = 1 << 1;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Discover(Heard),
    Request(StreamConfig),
    /// Bit `n` of each: block `n`'s Info, and its Name, have arrived. `infos`
    /// is when the last Info came, which starts the [`NAME_GRACE`].
    Blocks {
        info: u32,
        name: u32,
        infos: Option<Instant>,
    },
    Done(Ending),
}

/// The discovering side of one UMP connection.
#[derive(Clone, Debug)]
pub struct Connection {
    wanted: StreamConfig,
    step: Step,
    sent: Instant,
    attempts: u8,
    endpoint: Endpoint,
    endpoint_name: Assembler<98>,
    instance_id: Assembler<42>,
    block_name: Assembler<91>,
    block_name_of: u8,
    block_names: [Option<BlockName>; MAX_FUNCTION_BLOCKS],
}

impl Connection {
    /// A connection that asks for the MIDI 2.0 Protocol with JR Timestamps both
    /// ways, where the peer supports them; with the Endpoint Discovery to send.
    #[must_use]
    pub fn new(now: Instant) -> (Self, Action) {
        Self::with_wanted(
            StreamConfig {
                protocol: Protocol::Midi2,
                rx_jr: true,
                tx_jr: true,
            },
            now,
        )
    }

    /// A connection that asks for `wanted`, as the Stream Configuration Request
    /// would carry it: `rx_jr` is the peer receiving JR Timestamps, `tx_jr`
    /// the peer sending them. Each part the peer does not support is dropped
    /// from the request, and a reserved Protocol asks for MIDI 2.0.
    #[must_use]
    pub fn with_wanted(wanted: StreamConfig, now: Instant) -> (Self, Action) {
        let connection = Self {
            wanted,
            step: Step::Discover(Heard::empty()),
            sent: now,
            attempts: 1,
            endpoint: Endpoint::blank(),
            endpoint_name: Assembler::new(),
            instance_id: Assembler::new(),
            block_name: Assembler::new(),
            block_name_of: 0,
            block_names: [None; MAX_FUNCTION_BLOCKS],
        };
        let first = Action::Send(connection.packet_for_step());
        (connection, first)
    }

    /// What the connection learned so far. Final once [`settled`](Self::settled).
    #[must_use]
    pub const fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// How it ended, or `None` while a step is waiting.
    #[must_use]
    pub const fn settled(&self) -> Option<Ending> {
        match self.step {
            Step::Done(ending) => Some(ending),
            _ => None,
        }
    }

    /// When [`on_timeout`](Self::on_timeout) next has something to do, or
    /// `None` once settled.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        match self.step {
            Step::Done(_) => None,
            Step::Blocks {
                infos: Some(at), ..
            } => Some(at + NAME_GRACE),
            _ => Some(self.sent + STEP_TIMEOUT),
        }
    }

    /// Folds one message from the peer. One that is not a UMP Stream message,
    /// and one the step does not wait for, are ignored.
    ///
    /// After a [`Negotiated`](Ending::Negotiated) settle the peer may still
    /// change what it said, and the connection follows it, answering
    /// [`Action::Changed`]: a Stream Configuration Notification is the
    /// Protocol and JR Timestamps in use whenever it comes (§7.1.6.1 step 3:
    /// the Sender starts "using the Protocol and JR Timestamps configuration
    /// as declared in the Stream Configuration Notification"), but a Function
    /// Block Info or Name is read only when the Endpoint did not declare its
    /// Function Blocks static (§6.1.1: such an Endpoint "will not change any of
    /// the properties of its Function Blocks after initial discovery", and
    /// §7.1.8 and §7.1.9 send the Notifications for a change only from one that
    /// has not). A Function Block outside the count Endpoint Info declared is
    /// never read. After a [`Midi1Fallback`](Ending::Midi1Fallback) settle
    /// everything is ignored: the peer is a MIDI 1.0 device.
    pub fn on_message(&mut self, message: Message, now: Instant) -> Action {
        let Message::Stream(stream) = message else {
            return Action::Wait;
        };
        match (self.step, stream) {
            (Step::Done(Ending::Negotiated), _) => self.follow(stream),
            (Step::Done(Ending::Midi1Fallback), _) => Action::Wait,
            (Step::Discover(heard), _) => self.discover(heard, stream, now),
            (Step::Request(_), Stream::StreamConfigurationNotification(config)) => {
                self.endpoint.active = settle_protocol(config);
                self.start_blocks(now)
            }
            (Step::Blocks { info, name, infos }, _) => self.blocks(info, name, infos, stream, now),
            (Step::Request(_), _) => Action::Wait,
        }
    }

    /// Handles time having passed: when the current step has waited
    /// [`STEP_TIMEOUT`] since it was sent, sends it again, or, after
    /// [`MAX_ATTEMPTS`], gives up as the step says (see [`Ending`]). Calling it
    /// early is harmless.
    pub fn on_timeout(&mut self, now: Instant) -> Action {
        if let Step::Blocks {
            infos: Some(at), ..
        } = self.step
        {
            return if now.saturating_duration_since(at) >= NAME_GRACE {
                self.settle(Ending::Negotiated)
            } else {
                Action::Wait
            };
        }
        if matches!(self.step, Step::Done(_))
            || now.saturating_duration_since(self.sent) < STEP_TIMEOUT
        {
            return Action::Wait;
        }
        if self.attempts < MAX_ATTEMPTS {
            self.attempts += 1;
            self.sent = now;
            return Action::Send(self.packet_for_step());
        }
        match self.step {
            Step::Blocks { .. } => self.settle(Ending::Negotiated),
            _ => {
                self.endpoint.fall_back_to_midi1();
                self.settle(Ending::Midi1Fallback)
            }
        }
    }

    fn discover(&mut self, mut heard: Heard, stream: Stream, now: Instant) -> Action {
        match stream {
            Stream::EndpointInfo(info) => {
                // A count above 32 is not an Endpoint (§7.1.7: the numbers are
                // 0 to 31): the Info is refused, so it is never heard and the
                // step ends as a silent peer's does.
                if self.endpoint.apply_info(&info) {
                    heard |= Heard::INFO;
                }
            }
            Stream::DeviceIdentity(identity) => self.endpoint.identity = Some(identity),
            Stream::EndpointName(form, bytes) => {
                if let Some(text) = self.endpoint_name.feed(form, &bytes) {
                    self.endpoint.name = EndpointName::from_bytes(text);
                }
            }
            Stream::ProductInstanceId(form, bytes) => {
                if let Some(text) = self.instance_id.feed(form, &bytes) {
                    self.endpoint.instance_id = ProductInstanceId::from_bytes(text);
                }
            }
            Stream::StreamConfigurationNotification(config) => {
                self.endpoint.active = settle_protocol(config);
                heard |= Heard::CONFIG;
            }
            _ => {}
        }
        self.step = Step::Discover(heard);
        if heard != Heard::all() {
            return Action::Wait;
        }
        let wanted = narrow(self.wanted, &self.endpoint);
        if wanted == self.endpoint.active {
            return self.start_blocks(now);
        }
        self.step = Step::Request(wanted);
        self.send_step(now)
    }

    fn start_blocks(&mut self, now: Instant) -> Action {
        if self.endpoint.declared_blocks == 0 {
            return self.settle(Ending::Negotiated);
        }
        self.step = Step::Blocks {
            info: 0,
            name: 0,
            infos: None,
        };
        self.send_step(now)
    }

    fn blocks(
        &mut self,
        mut info: u32,
        mut name: u32,
        mut infos: Option<Instant>,
        stream: Stream,
        now: Instant,
    ) -> Action {
        match stream {
            Stream::FunctionBlockInfo(wire) => {
                if let Some(block) = self.endpoint.read_block(&wire) {
                    info |= 1 << block.id().get();
                    self.endpoint.insert_block(block);
                }
            }
            Stream::FunctionBlockName { block, form, bytes } => {
                // One name is open at a time: another block's Start drops it.
                if form == Form::Start || form == Form::Complete {
                    self.block_name_of = block.get();
                }
                if block.get() == self.block_name_of
                    && let Some(text) = self.block_name.feed(form, &bytes)
                    && let (Some(name_text), Some(slot)) = (
                        BlockName::from_bytes(text),
                        self.block_names.get_mut(usize::from(block.get())),
                    )
                {
                    *slot = Some(name_text);
                    name |= 1 << block.get();
                }
            }
            Stream::StreamConfigurationNotification(config) => {
                self.endpoint.active = settle_protocol(config);
            }
            _ => {}
        }
        let every = if self.endpoint.declared_blocks >= 32 {
            u32::MAX
        } else {
            (1u32 << self.endpoint.declared_blocks) - 1
        };
        if info & every == every && infos.is_none() {
            infos = Some(now);
        }
        self.step = Step::Blocks { info, name, infos };
        if info & every == every && name & every == every {
            self.settle(Ending::Negotiated)
        } else {
            Action::Wait
        }
    }

    /// A message after a negotiated settle (see [`on_message`](Self::on_message)).
    fn follow(&mut self, stream: Stream) -> Action {
        match stream {
            Stream::StreamConfigurationNotification(config) => {
                let active = settle_protocol(config);
                if active == self.endpoint.active {
                    return Action::Wait;
                }
                self.endpoint.active = active;
                Action::Changed
            }
            _ if self.endpoint.static_function_blocks => Action::Wait,
            Stream::FunctionBlockInfo(wire) => {
                let Some(mut block) = self.endpoint.read_block(&wire) else {
                    return Action::Wait;
                };
                // The Info carries no name: the one held stays.
                block.name = self
                    .endpoint
                    .function_block(block.id())
                    .and_then(|b| b.name);
                if self.endpoint.function_block(block.id()) == Some(&block) {
                    return Action::Wait;
                }
                self.endpoint.insert_block(block);
                Action::Changed
            }
            Stream::FunctionBlockName { block, form, bytes } => {
                if form == Form::Start || form == Form::Complete {
                    self.block_name_of = block.get();
                }
                if block.get() != self.block_name_of {
                    return Action::Wait;
                }
                let Some(text) = self.block_name.feed(form, &bytes) else {
                    return Action::Wait;
                };
                let (Some(name), Some(Some(held))) = (
                    BlockName::from_bytes(text),
                    self.endpoint.blocks.get_mut(usize::from(block.get())),
                ) else {
                    return Action::Wait;
                };
                if held.name == Some(name) {
                    return Action::Wait;
                }
                held.name = Some(name);
                Action::Changed
            }
            _ => Action::Wait,
        }
    }

    fn settle(&mut self, ending: Ending) -> Action {
        for (i, name) in self.block_names.iter().enumerate() {
            if let (Some(name), Some(Some(block))) = (name, self.endpoint.blocks.get_mut(i)) {
                block.name = Some(*name);
            }
        }
        self.step = Step::Done(ending);
        Action::Settled(ending)
    }

    fn send_step(&mut self, now: Instant) -> Action {
        self.sent = now;
        self.attempts = 1;
        Action::Send(self.packet_for_step())
    }

    fn packet_for_step(&self) -> Ump {
        let stream = match self.step {
            Step::Request(wanted) => Stream::StreamConfigurationRequest(wanted),
            Step::Blocks { .. } => Stream::FunctionBlockDiscovery {
                block: BlockSelect::All,
                filter: BlockFilter {
                    info: true,
                    name: true,
                },
            },
            Step::Discover(_) | Step::Done(_) => Stream::EndpointDiscovery {
                version: UMP_VERSION,
                filter: DiscoveryFilter {
                    endpoint_info: true,
                    device_identity: true,
                    endpoint_name: true,
                    product_instance_id: true,
                    stream_configuration: true,
                },
            },
        };
        Ump::from(Message::Stream(stream))
    }
}

/// A Notification's configuration as the connection keeps it: a reserved
/// Protocol is one it cannot use, so it is MIDI 1.0 (ours; the spec says
/// "reserved").
const fn settle_protocol(config: StreamConfig) -> StreamConfig {
    match config.protocol {
        Protocol::Reserved(_) => StreamConfig {
            protocol: Protocol::Midi1,
            ..config
        },
        _ => config,
    }
}

/// `wanted` reduced to what `endpoint` supports (§7.1.2): MIDI 2.0 only when
/// it supports it, JR only in the directions it supports.
const fn narrow(wanted: StreamConfig, endpoint: &Endpoint) -> StreamConfig {
    let caps = endpoint.capabilities;
    let protocol = match (wanted.protocol, caps.midi1, caps.midi2) {
        (Protocol::Midi1, true, _) | (_, true, false) => Protocol::Midi1,
        (Protocol::Midi1, false, true) | (_, _, true) => Protocol::Midi2,
        // It supports no Protocol at all: MIDI 1.0 is the one it is in.
        (_, false, false) => Protocol::Midi1,
    };
    StreamConfig {
        protocol,
        rx_jr: wanted.rx_jr && caps.rx_jr,
        tx_jr: wanted.tx_jr && caps.tx_jr,
    }
}

#[cfg(test)]
mod tests;
