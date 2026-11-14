//! Endpoints this process publishes for other applications to connect to.
//!
//! A [`VirtualSource`] is an endpoint other applications read from: this
//! process sends UMP into it. A [`VirtualDestination`] is one they write to:
//! this process receives what they send. The two are different types, so
//! receiving on a source and sending to a destination do not compile. Dropping
//! either removes the endpoint from the system.
//!
//! An endpoint is described by a [`VirtualSpec`]: its name, the UMP protocol it
//! carries, and the Function Blocks it declares. What a platform cannot do is an
//! error from the call that asks for it: creating on a platform with no backend
//! is [`Error::Unsupported`](crate::Error::Unsupported), and declaring Function
//! Blocks where the platform cannot publish them is
//! [`Error::VirtualBlocksUnsupported`](crate::Error::VirtualBlocksUnsupported). The protocol is
//! the exception, because a platform may carry the other one than asked for:
//! [`VirtualSource::protocol`] and [`VirtualDestination::protocol`] report the
//! one the endpoint was created with.
//!
//! A destination delivers into an [`InputProducerHandle`], exactly as a device
//! input does, so its events are stamped on the same clock and reach the graph
//! through the same inlets.

use std::sync::{Mutex, PoisonError};

use tutti_midi_device::{EndpointName, FunctionBlock};
use tutti_midi_protocol::{Translator, narrow_message};
use tutti_ump::Ump;

use crate::InputProducerHandle;
use crate::backend;
use crate::endpoints::InputConnection;
use crate::error::Result;

/// The UMP protocol an endpoint carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VirtualProtocol {
    /// MIDI 1.0 channel voice messages in UMP packets (message type 2).
    Midi1,
    /// MIDI 2.0 Protocol messages (message type 4).
    Midi2,
}

/// What a virtual endpoint is: built with [`new`](Self::new), which starts a
/// MIDI 2.0 endpoint with no Function Blocks.
#[derive(Clone, Debug)]
#[must_use = "a spec does nothing until an endpoint is opened from it"]
pub struct VirtualSpec {
    name: EndpointName,
    protocol: VirtualProtocol,
    blocks: Vec<FunctionBlock>,
}

impl VirtualSpec {
    /// An endpoint named `name`, carrying MIDI 2.0.
    pub fn new(name: EndpointName) -> Self {
        Self {
            name,
            protocol: VirtualProtocol::Midi2,
            blocks: Vec::new(),
        }
    }

    /// The endpoint carrying `protocol`. Where the platform has no such
    /// protocol the endpoint reports the one it got.
    pub fn with_protocol(mut self, protocol: VirtualProtocol) -> Self {
        self.protocol = protocol;
        self
    }

    /// The endpoint declaring `block`, replacing a block of the same number.
    pub fn with_function_block(mut self, block: FunctionBlock) -> Self {
        self.blocks.retain(|held| held.id() != block.id());
        self.blocks.push(block);
        self
    }

    /// The endpoint's name.
    #[must_use]
    pub fn name(&self) -> &EndpointName {
        &self.name
    }

    /// The protocol asked for.
    #[must_use]
    pub fn protocol(&self) -> VirtualProtocol {
        self.protocol
    }

    /// The Function Blocks declared, in the order they were added.
    #[must_use]
    pub fn function_blocks(&self) -> &[FunctionBlock] {
        &self.blocks
    }
}

/// The platform's side of a [`VirtualSource`].
pub(crate) trait SourceBackend: Send + Sync {
    /// Publishes one UMP message to the endpoint's subscribers.
    fn send(&self, words: &[u32]) -> Result<()>;
}

/// How a source brings a message to its protocol.
enum Conversion {
    /// A MIDI 1.0 endpoint: MIDI 2.0 voice messages narrow.
    Narrow,
    /// A MIDI 2.0 endpoint: MIDI 1.0 voice messages widen, holding the
    /// Registered and Assignable Controller runs a MIDI 1.0 stream spreads over
    /// several messages.
    Widen(Box<Mutex<Translator>>),
}

/// An endpoint other applications read from.
#[must_use = "dropping it removes the endpoint"]
pub struct VirtualSource {
    backend: Box<dyn SourceBackend>,
    conversion: Conversion,
    name: EndpointName,
    protocol: VirtualProtocol,
}

impl core::fmt::Debug for VirtualSource {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VirtualSource")
            .field("name", &self.name.as_str())
            .field("protocol", &self.protocol)
            .finish_non_exhaustive()
    }
}

impl VirtualSource {
    /// Publishes the endpoint `spec` describes.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`](crate::Error::Unsupported) on a platform with no
    /// backend, [`Error::VirtualBlocksUnsupported`](crate::Error::VirtualBlocksUnsupported)
    /// when `spec` declares Function Blocks the platform cannot publish, and the
    /// platform's own error when the OS refuses the endpoint.
    pub fn open(spec: &VirtualSpec) -> Result<Self> {
        let (backend, protocol) = backend::open_virtual_source(spec)?;
        let conversion = match protocol {
            VirtualProtocol::Midi1 => Conversion::Narrow,
            VirtualProtocol::Midi2 => Conversion::Widen(Box::new(Mutex::new(Translator::new()))),
        };
        Ok(Self {
            backend,
            conversion,
            name: spec.name,
            protocol,
        })
    }

    /// Sends `ump` to everything connected to the endpoint, in the endpoint's
    /// [`protocol`](Self::protocol).
    ///
    /// A MIDI 2.0 voice message narrows to its MIDI 1.0 message on a MIDI 1.0
    /// endpoint (M2-115), and a MIDI 1.0 voice message widens on a MIDI 2.0 one.
    /// A MIDI 1.0 Registered or Assignable Controller is several messages, so
    /// the widened controller is sent with the message that completes it and
    /// the earlier ones send nothing. Any other message is sent as it is.
    ///
    /// # Errors
    ///
    /// [`Error::NoMidi1Form`](crate::Error::NoMidi1Form) for a MIDI 2.0 message
    /// on a MIDI 1.0 endpoint that has no single MIDI 1.0 message (a per-note
    /// controller, a registered or assignable controller, per-note management);
    /// nothing is sent. Otherwise the platform's error when the OS refuses the
    /// message.
    pub fn send(&self, ump: Ump) -> Result<()> {
        let ump = match &self.conversion {
            Conversion::Narrow => match ump.decode() {
                Ok(message) => Ump::from(narrow_message(message)?),
                // A packet no message is defined for is forwarded as it is.
                Err(_) => ump,
            },
            Conversion::Widen(translator) => {
                let widened = translator
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .translate(ump);
                match widened {
                    Some(ump) => ump,
                    None => return Ok(()),
                }
            }
        };
        self.backend.send(ump.words())
    }

    /// The endpoint's name.
    #[must_use]
    pub fn name(&self) -> &EndpointName {
        &self.name
    }

    /// The protocol the endpoint was created with.
    #[must_use]
    pub fn protocol(&self) -> VirtualProtocol {
        self.protocol
    }
}

/// An endpoint other applications write to; what they send is delivered to the
/// [`InputProducerHandle`] it was opened with.
#[must_use = "dropping it removes the endpoint"]
pub struct VirtualDestination {
    connection: Box<dyn InputConnection>,
    name: EndpointName,
    protocol: VirtualProtocol,
}

impl core::fmt::Debug for VirtualDestination {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VirtualDestination")
            .field("name", &self.name.as_str())
            .field("protocol", &self.protocol)
            .finish_non_exhaustive()
    }
}

impl VirtualDestination {
    /// Publishes the endpoint `spec` describes, delivering into `producer`.
    ///
    /// # Errors
    ///
    /// As [`VirtualSource::open`].
    pub fn open(spec: &VirtualSpec, producer: InputProducerHandle) -> Result<Self> {
        let (connection, protocol) = backend::open_virtual_destination(spec, producer)?;
        Ok(Self {
            connection,
            name: spec.name,
            protocol,
        })
    }

    /// Whether the endpoint is still delivering: `false` once its reader ended
    /// on an error, while the endpoint is still published.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.connection.is_alive()
    }

    /// The endpoint's name.
    #[must_use]
    pub fn name(&self) -> &EndpointName {
        &self.name
    }

    /// The protocol the endpoint was created with.
    #[must_use]
    pub fn protocol(&self) -> VirtualProtocol {
        self.protocol
    }
}
