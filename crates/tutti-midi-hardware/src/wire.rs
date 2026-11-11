//! The wire form of a device's output: what a packet becomes before it is
//! queued for the device ([`Wire`]).
//!
//! The graph's MIDI is one thing (MIDI 2.0 Channel Voice, type `0x4`, after the
//! input edge's `Translator`), and a device is in a Protocol of its own. The
//! rule is the spec's, applied at one place, [`OpenOutput::queue`](crate::OpenOutput):
//!
//! - **A MIDI 2.0 Protocol endpoint** gets the packets as they are.
//!   (M2-104 §3.3.2: "MIDI 2.0 Protocol Devices shall not use any messages from
//!   Message Type 0x2", and the graph carries none; a MIDI 1.0 Channel Voice
//!   packet that does arrive is not changed here.) §7.1.6.2 allows MIDI 1.0
//!   Protocol "optionally" to "that Function Block" when "a Function Block
//!   represents (declares) a MIDI 1.0 connection"; this does not take the
//!   option, because §6.2.2 puts the translation for such a block on the
//!   device ("it is up to the Device to handle the required translation"), and
//!   a per-Group narrowing would send type `0x2` into an endpoint whose Protocol
//!   is MIDI 2.0, which §3.3.2 forbids for the rest of it.
//! - **A MIDI 1.0 Protocol endpoint**, and a MIDI 1.0 port, gets MIDI 1.0 (§3.2:
//!   "UMP MIDI 1.0 Devices shall not use any messages from Message Type 0x4,
//!   MIDI 2.0 Channel Voice Messages"): each type `0x4` packet through one
//!   [`Narrower`] for the output (M2-104 Appendix D.2, per Group and Channel, so a
//!   parameter select is skipped only where the receiver has seen it); type
//!   `0x2`, System and SysEx7 and every other type pass as they are. A
//!   packet the narrower has no MIDI 1.0 form for (a per-note controller,
//!   D.2.8) is dropped.
//! - **JR Timestamps** are sent only when the device's configuration says it
//!   receives them (§7.1.6.2, RXJR: the Sender "shall not send JR Timestamps
//!   until after a Stream Configuration Notification message ... has been
//!   received"): a JR Clock or JR Timestamp is dropped when `rx_jr` is off.
//!   Delta Clockstamp and Ticks Per Quarter Note are a clip's, not a wire's, and
//!   are dropped always.
//!
//! The configuration is the device's [`StreamConfig`]: the OS's description
//! when the device is opened, and what the UMP Stream negotiation settled
//! (`MidiSession::poll`) after it.

use tutti_graph::UmpAt;
use tutti_midi_protocol::Narrower;
use tutti_ump::{Message, Midi1Parser, Protocol, StreamConfig, Ump, Utility, decode};

/// One device output's wire form: its configuration, the [`Narrower`] that
/// serves it for as long as it is in MIDI 1.0, and the buffers one batch is
/// converted in.
#[derive(Debug)]
pub(crate) struct Wire {
    config: StreamConfig,
    narrower: Narrower,
    /// The batch as it goes to the device.
    out: Vec<UmpAt>,
    /// For each input event, how many of `out` it had produced once it was
    /// done: what turns the device's count back into the caller's.
    ends: Vec<usize>,
}

impl Wire {
    pub(crate) fn new(config: StreamConfig) -> Self {
        Self {
            config,
            narrower: Narrower::new(),
            out: Vec::new(),
            ends: Vec::new(),
        }
    }

    pub(crate) const fn config(&self) -> StreamConfig {
        self.config
    }

    /// The device now has `config`. A change of Protocol makes the narrower
    /// forget what it believes the receiver has selected: a receiver that was
    /// in another Protocol has not seen it.
    pub(crate) fn set_config(&mut self, config: StreamConfig) {
        if config.protocol != self.config.protocol {
            self.narrower.reset();
        }
        self.config = config;
    }

    /// Whether the device is in a Protocol that has no MIDI 2.0 Channel Voice:
    /// MIDI 1.0, and the reserved values, which nobody can speak.
    fn narrows(&self) -> bool {
        self.config.protocol != Protocol::Midi2
    }

    /// Converts `events` into the batch for the device, which
    /// [`batch`](Self::batch) reads.
    pub(crate) fn convert(&mut self, events: &[UmpAt]) {
        self.out.clear();
        self.ends.clear();
        let narrows = self.narrows();
        let jr = self.config.rx_jr;
        for &(at, ump) in events {
            match ump.message_type() {
                0 => {
                    if let Some(kept) = utility(ump, jr) {
                        self.out.push((at, kept));
                    }
                }
                4 if narrows => {
                    if let Ok(message @ Message::Midi2(address, _)) = decode(ump) {
                        let out = &mut self.out;
                        self.narrower.feed(message, |bytes| {
                            if let Some(packet) =
                                Midi1Parser::message(address.group, bytes.as_slice())
                            {
                                out.push((at, Ump::from(packet)));
                            }
                        });
                    }
                }
                _ => self.out.push((at, ump)),
            }
            self.ends.push(self.out.len());
        }
    }

    /// The batch [`convert`](Self::convert) made.
    pub(crate) fn batch(&self) -> &[UmpAt] {
        &self.out
    }

    /// How many of the events [`convert`](Self::convert) was given the device
    /// has taken, when it took the first `taken` of the batch: the events whose
    /// packets all went, so an event that became nothing counts as taken.
    pub(crate) fn accepted(&self, taken: usize) -> usize {
        self.ends.partition_point(|&end| end <= taken)
    }
}

/// A Utility packet as the wire carries it: the JR messages when the device
/// takes them, a NOOP, and nothing else.
fn utility(ump: Ump, jr: bool) -> Option<Ump> {
    match decode(ump) {
        Ok(Message::Utility(Utility::JrClock(_) | Utility::JrTimestamp(_))) => jr.then_some(ump),
        Ok(Message::Utility(Utility::Noop)) | Err(_) => Some(ump),
        Ok(_) => None,
    }
}

#[cfg(test)]
mod tests;
