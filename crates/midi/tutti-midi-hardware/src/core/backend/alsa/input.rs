//! Reading UMP from a sequencer port.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use super::client::SeqClient;
use super::enumerate::unpack_id;
use super::sys;
use crate::core::capability::EndpointId;
use crate::core::endpoints::InputConnection;
use crate::core::error::Result;
use crate::core::InputProducerHandle;
use tutti_midi_types::ump::split_ump_stream;

/// How long the pump sleeps when the queue is empty.
///
/// The client is opened **non-blocking**, so `snd_seq_ump_event_input` returns
/// `-EAGAIN` rather than parking. A blocking read would be tidier, but it cannot
/// be interrupted: the thread would sit in the kernel until an event arrived,
/// and a disconnect would hang until the device happened to send something.
/// Polling costs a wakeup per millisecond and makes shutdown immediate.
const IDLE_POLL: std::time::Duration = std::time::Duration::from_millis(1);

/// `-EAGAIN`. Not an error — "nothing queued yet".
const EAGAIN: i32 = -11;

/// An open ALSA input. Dropping it stops the pump and closes the port.
pub struct AlsaInput {
    running: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl InputConnection for AlsaInput {}

impl Drop for AlsaInput {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(t) = self.thread.take() {
            // Joining matters: the thread owns the `SeqClient`, and letting it
            // outlive this would leave the port subscribed to a device the
            // caller believes it disconnected.
            let _ = t.join();
        }
    }
}

/// Open `id` for input and start pumping its events into `producer`.
///
/// The pump runs on its own thread and stops when the returned connection is
/// dropped, which is also when the port unsubscribes.
///
/// # Errors
///
/// [`Error::Alsa`](crate::Error::Alsa) if the sequencer cannot be opened, the
/// port created, or the subscription made, and
/// [`Error::MidiDevice`](crate::Error::MidiDevice) if the pump thread cannot be
/// spawned.
pub fn open(id: EndpointId, producer: InputProducerHandle) -> Result<Box<dyn InputConnection>> {
    let (src_client, src_port) = unpack_id(id);

    // Non-blocking, so the pump can notice `running` going false. One client per
    // connection — see `SeqClient`.
    let seq = SeqClient::open("tutti-in", true)?;
    let port = seq.create_port(
        "tutti-in",
        sys::SND_SEQ_PORT_CAP_WRITE | sys::SND_SEQ_PORT_CAP_SUBS_WRITE,
        sys::SND_SEQ_PORT_TYPE_MIDI_GENERIC | sys::SND_SEQ_PORT_TYPE_APPLICATION,
    )?;

    // SAFETY: `seq` is open; the address came from enumeration.
    sys::check(
        unsafe { sys::snd_seq_connect_from(seq.raw(), port, src_client, src_port) },
        "snd_seq_connect_from",
    )?;

    let running = Arc::new(AtomicBool::new(true));
    let flag = running.clone();
    let thread = std::thread::Builder::new()
        .name("tutti-midi-in".to_string())
        .spawn(move || pump(seq, flag, producer))
        .map_err(|e| crate::core::error::Error::MidiDevice(format!("spawn MIDI pump: {e}")))?;

    Ok(Box::new(AlsaInput {
        running,
        thread: Some(thread),
    }))
}

/// Read UMP events until told to stop.
///
/// Takes `seq` **by value** so the client is dropped here, on this thread, when
/// the loop ends — the port unsubscribes exactly when the pump stops.
fn pump(seq: SeqClient, running: Arc<AtomicBool>, producer: InputProducerHandle) {
    while running.load(Ordering::Acquire) {
        let mut ev: *mut sys::snd_seq_ump_event_t = std::ptr::null_mut();
        // SAFETY: `seq` is open for the life of this function; `ev` is a valid
        // out-pointer. On success alsa-lib points it at a buffer it owns, valid
        // until the next call on this client — which is why the words are copied
        // out immediately below and the pointer never escapes.
        let rc = unsafe { sys::snd_seq_ump_event_input(seq.raw(), &mut ev) };

        if rc < 0 {
            // Empty queue is the common case, not a failure.
            if rc != EAGAIN {
                tracing::debug!("ALSA MIDI input: {}", super::client::strerror(rc));
            }
            std::thread::sleep(IDLE_POLL);
            continue;
        }
        if ev.is_null() {
            continue;
        }

        // SAFETY: `rc >= 0` and `ev` non-NULL means alsa-lib wrote a complete
        // event; copying by value ends this function's use of its buffer.
        let words = unsafe { (*ev).ump };
        let now = Instant::now();

        // A `snd_seq_ump_event_t` carries one UMP message in a fixed 4-word
        // field, so the trailing words are padding whenever the message is
        // shorter. `split_ump_stream` reads the length from the type nibble and
        // yields exactly that one message — using it rather than passing all
        // four words to `from_ump` is what keeps a 1-word message from
        // swallowing three words of zeros.
        let decoded = split_ump_stream(&words).next();
        if let Some(event) = decoded {
            if !producer.push(event, now) {
                tracing::debug!("MIDI input ring full, dropping event");
            }
        }
    }
}
