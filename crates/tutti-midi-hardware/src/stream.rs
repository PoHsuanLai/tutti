//! What a device's UMP Stream conversation settled: [`DeviceStream`], and the
//! [`Negotiation`] that runs it (M2-104 §7.1.6.1, Table 4 steps 2 to 4).
//!
//! A [`Connection`] is the pure state machine; this is the part that has a
//! wire. One runs for each input the session opens on a device that is a UMP
//! Endpoint and has an output to send on:
//!
//! 1. The first [`poll`](Negotiation::poll) starts it: the Endpoint Discovery
//!    goes out on the device's output.
//! 2. Each poll hands the connection the Stream messages the device sent since
//!    (the port's stream ring, [`HardwareMidiInputs::drain_stream`]) and the
//!    time; what it answers goes out, and a step with no answer is sent again
//!    after [`STEP_TIMEOUT`] and given up after [`MAX_ATTEMPTS`].
//! 3. It ends as a [`DeviceStream`].
//!
//! Time is an argument. Nothing here reads a clock, waits or blocks: a device
//! that never answers (M2-104 Appendix I: a device that does not know UMP
//! Stream messages, or an OS that never delivers them) costs three sends and
//! then is a [`Basis::Unanswered`] device, described by the OS.
//!
//! # What is asked for
//!
//! The MIDI 2.0 Protocol where the device offers it, else MIDI 1.0
//! ([`Connection::with_wanted`] narrows the request to what the Endpoint
//! supports), with **JR Timestamps off both ways**. Input time comes from the
//! OS's arrival stamp (`InputProducerHandle::push`), which JR would only
//! repeat, and nothing in the graph reads a JR clock; and JR on the output
//! side is stamped by `UmpOut`'s flag, which nothing yet makes follow a
//! negotiation (design 017, L6(c)). A device put in JR mode that nothing then
//! fed would be worse than one left out of it.

use std::time::Instant;

use tutti_graph::Offset;
use tutti_midi_device::{Action, Connection, Ending, Endpoint};
use tutti_midi_runtime::MidiOut;
use tutti_ump::{Protocol, StreamConfig, Ump, decode};

use crate::capability::UmpCapability;
use crate::devices::OpenOutput;

/// The configuration the session asks every Endpoint for (see the module).
const WANTED: StreamConfig = StreamConfig {
    protocol: Protocol::Midi2,
    rx_jr: false,
    tx_jr: false,
};

/// How a device's [`DeviceStream`] came to be known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Basis {
    /// The device answered: the Endpoint is what it said, and the
    /// configuration is the last Stream Configuration Notification it sent
    /// (§7.1.6.1 step 3: the peer may refuse any part of a request).
    Negotiated,
    /// The device was asked and did not answer in time. The Endpoint is the
    /// OS's description of it, which for a device an OS has already
    /// discovered (the ALSA kernel does, for USB) is the true one.
    Unanswered,
    /// It was not asked: a MIDI 1.0 port has no UMP Stream to ask, and a
    /// device with no output has no way to ask. The Endpoint is the OS's
    /// description.
    Described,
}

/// What is known of the UMP Stream state of one device: its Endpoint, the
/// Protocol and JR Timestamps in use, and how that came to be known.
///
/// The output side reads [`config`](Self::config) to decide which messages a
/// device can be sent (design 017, L6(c)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceStream {
    endpoint: Endpoint,
    basis: Basis,
}

impl DeviceStream {
    /// The Endpoint: what the device declared, or what the OS says of it.
    #[must_use]
    pub const fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// How it came to be known.
    #[must_use]
    pub const fn basis(&self) -> Basis {
        self.basis
    }

    /// The Protocol and JR Timestamps in use.
    #[must_use]
    pub const fn config(&self) -> StreamConfig {
        self.endpoint.active()
    }

    /// The Endpoint as a [`UmpCapability`]: the protocol in use and the blocks
    /// it declares.
    #[must_use]
    pub fn capability(&self) -> UmpCapability {
        UmpCapability::of(&self.endpoint)
    }
}

/// One device's negotiation: its [`Connection`], while one runs, and the output
/// it sends on. Control side.
pub(crate) struct Negotiation {
    os: UmpCapability,
    out: Option<OpenOutput>,
    run: Run,
}

enum Run {
    /// Decided to ask, not yet started: the first poll starts it.
    Idle,
    Asking(Box<Connection>),
    Done(Box<DeviceStream>),
}

impl Negotiation {
    /// The negotiation for a device the OS describes as `os`, on `out` when it
    /// has an output. It asks only a UMP Endpoint that has an output; any
    /// other is [`Basis::Described`] at once.
    pub(crate) fn new(os: UmpCapability, out: Option<OpenOutput>) -> Self {
        let asks = os.is_ump_endpoint() && out.is_some();
        let run = if asks {
            Run::Idle
        } else {
            Run::Done(Box::new(DeviceStream {
                endpoint: os.endpoint(),
                basis: Basis::Described,
            }))
        };
        Self { os, out, run }
    }

    /// What is known, once it is settled.
    pub(crate) fn settled(&self) -> Option<DeviceStream> {
        match &self.run {
            Run::Done(stream) => Some(**stream),
            _ => None,
        }
    }

    /// Takes the Stream messages the device sent (`incoming`) and the time
    /// `now`: starts the negotiation, or steps it. Returns the stream when this
    /// call settled it.
    pub(crate) fn poll(
        &mut self,
        now: Instant,
        incoming: impl IntoIterator<Item = Ump>,
    ) -> Option<DeviceStream> {
        let mut actions = Vec::new();
        match &mut self.run {
            Run::Idle => {
                let (connection, discovery) = Connection::with_wanted(WANTED, now);
                actions.push(Action::Send(discovery));
                self.run = Run::Asking(Box::new(connection));
            }
            Run::Asking(connection) => {
                for ump in incoming {
                    if let Ok(message) = decode(ump) {
                        actions.push(connection.on_message(message, now));
                    }
                }
                if connection.deadline().is_some_and(|at| now >= at) {
                    actions.push(connection.on_timeout(now));
                }
            }
            Run::Done(_) => return None,
        }
        for action in actions {
            match action {
                Action::Wait => {}
                Action::Send(ump) => self.send(ump),
                Action::Settled(ending) => return self.settle(ending),
            }
        }
        None
    }

    fn send(&self, ump: Ump) {
        if let Some(out) = &self.out
            && out.queue(&[(Offset::ZERO, ump)]) == 0
        {
            // The step is sent again when it times out.
            tracing::debug!("a UMP Stream message was refused by {}", out.name());
        }
    }

    fn settle(&mut self, ending: Ending) -> Option<DeviceStream> {
        let Run::Asking(connection) = &self.run else {
            return None;
        };
        let stream = match ending {
            Ending::Negotiated => DeviceStream {
                endpoint: *connection.endpoint(),
                basis: Basis::Negotiated,
            },
            Ending::Midi1Fallback => DeviceStream {
                endpoint: self.os.endpoint(),
                basis: Basis::Unanswered,
            },
        };
        self.run = Run::Done(Box::new(stream));
        Some(stream)
    }
}

#[cfg(test)]
mod tests;
