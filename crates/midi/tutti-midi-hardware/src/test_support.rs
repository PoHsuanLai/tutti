//! A MIDI backend with no OS behind it, for testing the session layer.
//!
//! [`MidiSession::with_backend`](crate::MidiSession::with_backend) is the seam
//! this exists for: the session owns no driver code, so a supplied
//! [`MidiEndpoints`] drives every connect / disconnect / send path
//! deterministically, on any platform, including one with no MIDI hardware at
//! all. Without it a test of that layer can assert only "does not panic".
//!
//! # Why it is exported rather than `#[cfg(test)]`
//!
//! It began as a private fixture in `session.rs`, which covered this crate's own
//! tests and nothing above it. `bevy-tutti` wires a `MidiSession` into the ECS
//! and had no way to test that wiring — a headless CI box has no MIDI port, so
//! every path through it was either untested or asserted against "no device
//! found". The fixture is the *only* thing standing between those tests and a
//! real device, and it is small, so exporting it is cheaper than each consumer
//! writing its own subtly different one.
//!
//! Behind the `test-support` feature, off by default: a host has no use for a
//! fake device list, and shipping one in a release build invites it into
//! production wiring by accident.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::capability::{EndpointId, EndpointInfo, UmpCapability};
use crate::endpoints::{InputConnection, MidiEndpoints};
use crate::error::{Error, Result};
use crate::InputProducerHandle;
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::MidiOut;

/// A backend with a fixed device list and no OS behind it.
///
/// Build one with [`FakeBackend::build`] for the default two-in / two-out list,
/// or [`build_with`](FakeBackend::build_with) to name your own. The counters it
/// returns are what make its behaviour observable: `sent` counts events handed
/// to its sink, `opens` counts input ports actually opened — the latter is how
/// "reconnecting does not reopen" is asserted at all, since a second driver
/// connection to one device duplicates every inbound event and is invisible from
/// the outside.
pub struct FakeBackend {
    inputs: Vec<EndpointInfo>,
    outputs: Vec<EndpointInfo>,
    sent: Arc<AtomicUsize>,
    opens: Arc<AtomicUsize>,
    /// How many events per batch the sink it hands out will accept.
    accepts: usize,
}

/// The counters a [`FakeBackend`] bumps, handed back at construction.
///
/// A struct rather than a tuple: the two are both `Arc<AtomicUsize>` and reading
/// the wrong one is a test that passes for the wrong reason.
#[derive(Clone, Debug)]
pub struct FakeCounters {
    /// Events handed to the fake sink, across every batch.
    pub sent: Arc<AtomicUsize>,
    /// Input ports opened. Counts *opens*, not connections, which is what makes
    /// a duplicate open detectable.
    pub opens: Arc<AtomicUsize>,
}

impl FakeCounters {
    /// Events handed to the sink so far.
    #[must_use]
    pub fn sent(&self) -> usize {
        self.sent.load(Ordering::SeqCst)
    }

    /// Input ports opened so far.
    #[must_use]
    pub fn opens(&self) -> usize {
        self.opens.load(Ordering::SeqCst)
    }
}

struct FakeConn;
impl InputConnection for FakeConn {}

/// A sink that counts what it was handed and accepts `accepts` of each batch.
///
/// `accepts` is what lets a test express a device that refuses. A
/// `MidiOut::queue` returning `()` would make that shape unrepresentable, and
/// therefore make `send`'s accepted count untestable.
struct FakeSink {
    sent: Arc<AtomicUsize>,
    accepts: usize,
}

impl MidiOut for FakeSink {
    fn queue(&self, events: &[MidiEvent]) -> usize {
        self.sent.fetch_add(events.len(), Ordering::SeqCst);
        self.accepts.min(events.len())
    }
}

/// An [`EndpointInfo`] with a raw id and a name, MIDI 2.0 capable.
#[must_use]
pub fn endpoint(raw: u64, name: &str) -> EndpointInfo {
    EndpointInfo {
        id: EndpointId::from_raw(raw),
        name: name.to_string(),
        capability: UmpCapability::midi2(),
    }
}

impl FakeBackend {
    /// The default backend: inputs `Keystep Pro` (id 1) and `IAC Bus 1` (id 2),
    /// outputs `Synth A` (id 10) and `IAC Bus 1` (id 11). Its sink accepts
    /// everything.
    ///
    /// Two names collide across the two directions deliberately — a session that
    /// keyed its connection map by name rather than by [`EndpointId`] would pass
    /// every test against a list where they did not.
    ///
    /// `build` rather than `new`: it returns the backend *and* its counters, and
    /// a `new` that does not return `Self` reads as one that does.
    #[must_use]
    pub fn build() -> (Box<dyn MidiEndpoints>, FakeCounters) {
        Self::build_with(
            vec![endpoint(1, "Keystep Pro"), endpoint(2, "IAC Bus 1")],
            vec![endpoint(10, "Synth A"), endpoint(11, "IAC Bus 1")],
            usize::MAX,
        )
    }

    /// As [`build`](Self::build), but the sink accepts at most `accepts` events
    /// per batch — `0` models a device refusing everything.
    #[must_use]
    pub fn build_accepting(accepts: usize) -> (Box<dyn MidiEndpoints>, FakeCounters) {
        Self::build_with(
            vec![endpoint(1, "Keystep Pro"), endpoint(2, "IAC Bus 1")],
            vec![endpoint(10, "Synth A"), endpoint(11, "IAC Bus 1")],
            accepts,
        )
    }

    /// A backend over caller-supplied device lists.
    ///
    /// For the cases the defaults cannot express — an empty list (no hardware),
    /// or a list whose ids a test wants to control.
    #[must_use]
    pub fn build_with(
        inputs: Vec<EndpointInfo>,
        outputs: Vec<EndpointInfo>,
        accepts: usize,
    ) -> (Box<dyn MidiEndpoints>, FakeCounters) {
        let counters = FakeCounters {
            sent: Arc::new(AtomicUsize::new(0)),
            opens: Arc::new(AtomicUsize::new(0)),
        };
        let backend = FakeBackend {
            inputs,
            outputs,
            sent: Arc::clone(&counters.sent),
            opens: Arc::clone(&counters.opens),
            accepts,
        };
        (Box::new(backend), counters)
    }
}

impl MidiEndpoints for FakeBackend {
    fn inputs(&self) -> Vec<EndpointInfo> {
        self.inputs.clone()
    }

    fn outputs(&self) -> Vec<EndpointInfo> {
        self.outputs.clone()
    }

    fn open_input(
        &self,
        id: EndpointId,
        _producer: InputProducerHandle,
    ) -> Result<Box<dyn InputConnection>> {
        if !self.inputs.iter().any(|e| e.id == id) {
            return Err(Error::MidiDevice("no such input".into()));
        }
        self.opens.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(FakeConn))
    }

    fn open_output(&self, id: EndpointId) -> Result<Box<dyn MidiOut>> {
        if !self.outputs.iter().any(|e| e.id == id) {
            return Err(Error::MidiDevice("no such output".into()));
        }
        Ok(Box::new(FakeSink {
            sent: Arc::clone(&self.sent),
            accepts: self.accepts,
        }))
    }
}
