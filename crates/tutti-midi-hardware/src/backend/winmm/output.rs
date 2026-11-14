//! Sending MIDI 1.0 to a winmm output, each event at its time.

use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::super::legacy_port::bytes::{ByteOut, Wire};
use super::super::legacy_port::schedule::Schedule;
use crate::endpoints::DeviceOut;
use crate::error::{Error, Result};
use tutti_graph::UmpAt;
use tutti_midi_runtime::{MidiOut, Outgoing, SendAt};
use tutti_ump::Ump;
use tutti_winmm_ffi::{MidiOut as Port, Sleeper};

/// How many events wait at most; a queue past it takes a prefix and says so.
const MAX_PENDING: usize = 8192;

/// How long dropping an output waits for its thread: the thread ends within a
/// send of the stop, and a send is bounded by the SysEx wait.
const JOIN_BOUND: Duration = Duration::from_secs(1);

struct State {
    queue: Schedule<Ump>,
    stop: bool,
}

struct Shared {
    state: Mutex<State>,
    /// Ends the thread's wait when an earlier event arrives or the output stops.
    sleeper: Sleeper,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// An open winmm output, as a [`MidiOut`]: a thread holds each event until its
/// time and sends in order. Dropping it stops the thread and closes the port;
/// events not yet due are dropped.
pub struct WinMmOutput {
    shared: Arc<Shared>,
    /// Disconnects when the thread ends, however it ends.
    ended: Mutex<mpsc::Receiver<()>>,
    thread: Option<JoinHandle<()>>,
}

impl MidiOut for WinMmOutput {
    fn queue(&self, events: &[UmpAt]) -> usize {
        self.queue_timed(&Outgoing::now_batch(events))
    }

    /// Returns how many events were taken: an unbroken prefix, short only when
    /// [`MAX_PENDING`] events already wait. A send that then fails is logged
    /// on the thread.
    fn queue_timed(&self, events: &[Outgoing]) -> usize {
        let now = Instant::now();
        let mut state = self.shared.state();
        let mut taken = 0;
        for event in events {
            if state.queue.len() >= MAX_PENDING {
                break;
            }
            let due = match event.send {
                SendAt::Now => now,
                SendAt::At(at) => at,
            };
            state.queue.push(due, event.ump);
            taken += 1;
        }
        drop(state);
        if taken > 0
            && let Err(e) = self.shared.sleeper.wake()
        {
            tracing::warn!("winmm MIDI output wake: {e}");
        }
        taken
    }
}

impl DeviceOut for WinMmOutput {}

impl Drop for WinMmOutput {
    fn drop(&mut self) {
        self.shared.state().stop = true;
        if let Err(e) = self.shared.sleeper.wake() {
            tracing::warn!("winmm MIDI output wake: {e}");
        }
        if let Some(thread) = self.thread.take() {
            match self
                .ended
                .get_mut()
                .unwrap_or_else(PoisonError::into_inner)
                .recv_timeout(JOIN_BOUND)
            {
                Err(mpsc::RecvTimeoutError::Timeout) => tracing::warn!(
                    "winmm MIDI output did not stop within {JOIN_BOUND:?}; left running"
                ),
                // Disconnected: the thread has ended (or is ending).
                _ => {
                    let _ = thread.join();
                }
            }
        }
    }
}

/// Sends what is due, in order, then waits for the next time or a wake.
fn run(mut port: Port, shared: &Shared) {
    let mut bytes = ByteOut::new();
    loop {
        let (due, wait) = {
            let mut state = shared.state();
            if state.stop {
                return;
            }
            let now = Instant::now();
            let due: Vec<Ump> = std::iter::from_fn(|| state.queue.pop_due(now)).collect();
            (due, state.queue.wait(now))
        };
        if !due.is_empty() {
            for ump in due {
                let sent = bytes.encode(ump, |wire| match wire {
                    Wire::Short(message) => port.short(message),
                    Wire::Long(message) => port.long(message),
                });
                if let Err(e) = sent {
                    tracing::debug!("winmm MIDI send: {e}");
                }
            }
            // Look again: more may have come due while sending.
            continue;
        }
        if let Err(e) = shared.sleeper.wait(wait) {
            tracing::warn!("winmm MIDI output stopped: {e}");
            return;
        }
    }
}

/// Opens the output at `index`.
///
/// # Errors
///
/// [`Error::WinMm`] from the port's or the timer's creation, and
/// [`Error::MidiDevice`] if the thread cannot be spawned.
pub(super) fn open(index: u32) -> Result<WinMmOutput> {
    let port = Port::open(index)?;
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            queue: Schedule::new(),
            stop: false,
        }),
        sleeper: Sleeper::new()?,
    });
    let (ending, ended) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("tutti-midi-out".to_string())
        .spawn({
            let shared = Arc::clone(&shared);
            move || {
                // Dropped when the thread returns or unwinds.
                let _ending: mpsc::Sender<()> = ending;
                run(port, &shared);
            }
        })
        .map_err(|e| Error::device_from("spawn MIDI output thread", e))?;
    Ok(WinMmOutput {
        shared,
        ended: Mutex::new(ended),
        thread: Some(thread),
    })
}
