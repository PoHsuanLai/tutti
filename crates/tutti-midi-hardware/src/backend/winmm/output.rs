//! Sending MIDI 1.0 to a winmm output.

use std::sync::{Mutex, PoisonError};

use super::super::legacy_port::bytes::{ByteOut, Wire};
use crate::endpoints::DeviceOut;
use crate::error::Result;
use tutti_graph::UmpAt;
use tutti_midi_runtime::{MidiOut, Outgoing};
use tutti_winmm_ffi::MidiOut as Port;

/// An open winmm output, as a [`MidiOut`].
pub struct WinMmOutput {
    /// Behind a `Mutex` because `MidiOut::queue` takes `&self` and a winmm
    /// handle is used from one thread at a time. Sending happens on the
    /// control thread's output drain, never the audio thread.
    inner: Mutex<Sender>,
}

struct Sender {
    port: Port,
    bytes: ByteOut,
}

impl MidiOut for WinMmOutput {
    fn queue(&self, events: &[UmpAt]) -> usize {
        self.queue_timed(&Outgoing::now_batch(events))
    }

    /// Returns how many events winmm took: an unbroken prefix, stopping at the
    /// first failure. An event's send time is not used; see the module's note.
    fn queue_timed(&self, events: &[Outgoing]) -> usize {
        // A poisoned lock guards nothing broken: `Sender` is a handle and a
        // reassembly buffer, and a send that panicked leaves the port usable.
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let Sender { port, bytes } = &mut *inner;
        let mut accepted = 0;
        for event in events {
            let sent = bytes.encode(event.ump, |wire| match wire {
                Wire::Short(message) => port.short(message),
                Wire::Long(message) => port.long(message),
            });
            if let Err(e) = sent {
                tracing::debug!("winmm MIDI send: {e}");
                break;
            }
            accepted += 1;
        }
        accepted
    }
}

impl DeviceOut for WinMmOutput {}

/// Opens the output at `index`.
///
/// # Errors
///
/// [`Error::WinMm`](crate::Error::WinMm) from the port's opening.
pub(super) fn open(index: u32) -> Result<WinMmOutput> {
    Ok(WinMmOutput {
        inner: Mutex::new(Sender {
            port: Port::open(index)?,
            bytes: ByteOut::new(),
        }),
    })
}
