//! The tap after the fader: "what I'm hearing".
//!
//! A tap in the plan (`tutti-graph`) copies the graph's signal before the
//! engine's declick fader and the fold to the device width. This is the other
//! kind: [`Engine`](crate::Engine) writes each chunk of its output buffer,
//! device-width and after the fader, to the rings of a [`TapReader`], the same
//! reader type. It is written by [`Engine::process`](crate::Engine::process)
//! and [`Engine::process_segment`](crate::Engine::process_segment), so a
//! device callback and an offline session's `render` feed it; an export forks
//! the graph and renders without the engine, so it never does.
//!
//! # The hand-over
//!
//! The writer reaches the audio thread over a bounded `ringbuf` queue of boxed
//! writers (the editor-to-executor pattern; no new lock-free structure, so
//! nothing for `tutti-rt` and no loom model). The audio thread pops installs
//! at the start of each call, and the writer it replaces goes back over a
//! second queue to be freed on the control side: the audio thread never
//! drops one (`TapWriter` panics in debug if it is dropped inside
//! [`RtScope`](tutti_rt::RtScope), which the engine enters). The control ends
//! of both queues sit behind a `Mutex` that only control threads take.
//!
//! **When the first frame arrives.** An install is seen at the start of the
//! next [`process`](crate::Engine::process) call that begins after
//! [`open`](HeardTaps::open) returns, and that call's whole block is the first
//! thing written: a call already in progress when `open` ran is not. A
//! [`close`](HeardTaps::close) stops the writes from the next call on, the
//! same way. Opens and closes are applied in the order they took the lock, so
//! racing ones leave the state the last of them asked for.
//!
//! A block of another width than the tap's counts as lost, whole.

use std::sync::{Arc, Mutex, PoisonError, Weak};

use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use tutti_graph::{TapReader, TapWriter};
use tutti_types::{ChannelLayout, Samples, TapId};

/// Installs that can be in flight at once.
const INSTALLS: usize = 4;

/// An install: a writer, or `None` to close.
type Install = Option<Box<TapWriter>>;

/// Why [`HeardTaps::open`] refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum HeardTapError {
    /// A reader of the engine's tap is alive. Close it first (or drop it:
    /// the session's reclaim phase closes a tap whose reader is gone).
    #[error("the output tap already has a reader; close it before opening again")]
    Busy,
    /// The audio thread has not yet taken the last installs (it is not
    /// rendering). Try again once it has.
    #[error("the audio thread has not yet taken the last tap installs")]
    Backpressure,
}

/// The control side of the engine's after-the-fader tap: cheap to clone,
/// usable from any control thread. Get it from
/// [`Engine::heard_taps`](crate::Engine::heard_taps).
#[derive(Clone, Debug)]
pub struct HeardTaps(Arc<Mutex<Control>>);

struct Control {
    install: HeapProd<Install>,
    retired: HeapCons<Box<TapWriter>>,
    /// The open tap's reader, to see when it was dropped.
    reader: Option<Weak<()>>,
}

impl std::fmt::Debug for Control {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Control").finish_non_exhaustive()
    }
}

impl Control {
    /// Frees what the audio thread retired.
    fn free(&mut self) {
        while self.retired.try_pop().is_some() {}
    }

    fn alive(&self) -> bool {
        self.reader.as_ref().is_some_and(|r| r.strong_count() > 0)
    }
}

/// The audio thread's ends: installs in, retired writers out, the live one.
pub(crate) struct Hearing {
    install: HeapCons<Install>,
    retire: HeapProd<Box<TapWriter>>,
    writer: Option<Box<TapWriter>>,
}

/// A linked pair, built with the engine.
pub(crate) fn link() -> (HeardTaps, Hearing) {
    let (install_tx, install_rx) = HeapRb::<Install>::new(INSTALLS).split();
    // A retired writer waits here for the control side; this holds every
    // install that can be in flight and the live one.
    let (retire_tx, retire_rx) = HeapRb::<Box<TapWriter>>::new(INSTALLS + 2).split();
    (
        HeardTaps(Arc::new(Mutex::new(Control {
            install: install_tx,
            retired: retire_rx,
            reader: None,
        }))),
        Hearing {
            install: install_rx,
            retire: retire_tx,
            writer: None,
        },
    )
}

impl HeardTaps {
    fn control(&self) -> std::sync::MutexGuard<'_, Control> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Opens the tap: `width` channels (the device's, so every output channel
    /// is kept, no fold), each lane's ring `capacity` frames. What the reader
    /// does not drain in time is dropped and counted
    /// ([`TapReader::take_lost`]). Flows from the first call of the engine's
    /// render that begins after this returns.
    ///
    /// # Errors
    ///
    /// [`HeardTapError::Busy`] while another reader of the tap is alive;
    /// [`HeardTapError::Backpressure`] while the audio thread has not taken
    /// the last installs.
    pub fn open(
        &self,
        width: ChannelLayout,
        capacity: Samples,
    ) -> Result<TapReader, HeardTapError> {
        let mut c = self.control();
        c.free();
        if c.alive() {
            return Err(HeardTapError::Busy);
        }
        if c.install.vacant_len() == 0 {
            return Err(HeardTapError::Backpressure);
        }
        let (reader, writer) =
            TapReader::standalone(TapId(0), usize::from(width.count()), capacity);
        if c.install.try_push(Some(Box::new(writer))).is_err() {
            unreachable!("the queue has a free slot");
        }
        c.reader = Some(reader.watch());
        Ok(reader)
    }

    /// Closes the tap from the next call of the engine's render on. The
    /// reader keeps what it was written. Does nothing if none is open.
    pub fn close(&self) {
        let mut c = self.control();
        c.free();
        c.reader = None;
        // A full queue means the audio thread is behind; the close then waits
        // for the next control tick ([`reclaim`](Self::reclaim) retries).
        let _ = c.install.try_push(None);
    }

    /// Frees the writers the audio thread retired, and closes the tap if its
    /// reader was dropped. A control-side tick (the session's `Reclaim`
    /// phase).
    pub fn reclaim(&self) {
        let mut c = self.control();
        c.free();
        if c.reader.is_some() && !c.alive() {
            c.reader = None;
            let _ = c.install.try_push(None);
        }
    }
}

impl Hearing {
    /// Takes the installs that arrived, retiring the writers they replace.
    /// Audio thread: pops and pushes boxes, drops nothing, allocates nothing.
    #[inline]
    pub(crate) fn swap(&mut self) {
        while self.retire.vacant_len() > 0
            && let Some(install) = self.install.try_pop()
        {
            if let Some(old) = std::mem::replace(&mut self.writer, install)
                && let Err(old) = self.retire.try_push(old)
            {
                // Unreachable: the vacancy was checked above.
                std::mem::forget(old);
            }
        }
    }

    /// Writes a chunk of the (faded) output, `channels` wide.
    #[inline]
    pub(crate) fn write(&mut self, block: &[f32], channels: usize) {
        if let Some(writer) = self.writer.as_mut() {
            if writer.width() == channels {
                writer.push_interleaved(block);
            } else {
                writer.skip(block.len() / channels.max(1));
            }
        }
    }
}
