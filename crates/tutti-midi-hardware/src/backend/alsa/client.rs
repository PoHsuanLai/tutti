//! Opening a UMP-mode ALSA sequencer client, through `tutti-alsa-ffi`.

use crate::error::Result;
use tutti_alsa_ffi::Client;

/// Sequencer clients this process has opened, for the tests that a quiet path
/// opens none: every open is announced to every watch on the machine.
#[cfg(test)]
pub(super) static OPENS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Opens a client in UMP MIDI-2.0 mode, named `name` to other clients.
///
/// One client per connection, not one per backend: a blocking read on a shared
/// client would have every input pump contending on one handle, and one device
/// going quiet would stall the others. Enumeration opens a short-lived client of
/// its own for the same reason: it must not block behind a pump.
pub(super) fn open(name: &str, nonblock: bool) -> Result<Client> {
    #[cfg(test)]
    OPENS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    Ok(Client::open(name, nonblock)?)
}
