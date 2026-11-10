//! The tap after the fader: "what I'm hearing".
//!
//! A tap in the plan (`tutti-graph`) copies the graph's signal before the
//! engine's declick fader and the fold to the device width. This is the other
//! kind: [`Engine`](crate::Engine) writes each chunk of its output buffer,
//! device-width and after the fader (the engine's f32 output buffer, before
//! the device layer clamps and converts it), to the rings of a [`TapReader`], the same
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
use tutti_graph::TapId;
use tutti_graph::{TapReader, TapWriter};
use tutti_types::{ChannelLayout, Samples};

/// Opens that can be in flight at once. The install queue holds one more: a
/// slot kept for a close, so closing a tap never fails for want of room while
/// opens fill the queue (the audio thread behind).
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
    /// A close the install queue had no room for, owed to the audio thread:
    /// sent on the next open, close or reclaim, whichever comes first.
    owed_close: bool,
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

    /// Sends a close the audio thread must see: now if the queue has room,
    /// else it stays owed and the next control call sends it. The old writer
    /// keeps feeding its reader (which no longer exists for the caller) until
    /// then.
    fn send_close(&mut self) {
        self.owed_close = self.install.try_push(None).is_err();
    }

    /// Settles what is owed before anything new is queued behind it.
    fn settle(&mut self) {
        self.free();
        if self.owed_close {
            self.send_close();
        }
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
    let (install_tx, install_rx) = HeapRb::<Install>::new(INSTALLS + 1).split();
    // A retired writer waits here for the control side; this holds every
    // install that can be in flight and the live one.
    let (retire_tx, retire_rx) = HeapRb::<Box<TapWriter>>::new(INSTALLS + 3).split();
    (
        HeardTaps(Arc::new(Mutex::new(Control {
            install: install_tx,
            retired: retire_rx,
            reader: None,
            owed_close: false,
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
        c.settle();
        if c.alive() {
            return Err(HeardTapError::Busy);
        }
        // One slot stays free for a close.
        if c.owed_close || c.install.vacant_len() <= 1 {
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
        c.settle();
        c.reader = None;
        // A full queue means the audio thread is behind: the close stays owed
        // and goes out on the next open, close or [`reclaim`](Self::reclaim).
        c.send_close();
    }

    /// Frees the writers the audio thread retired, and closes the tap if its
    /// reader was dropped. A control-side tick (the session's `Reclaim`
    /// phase).
    pub fn reclaim(&self) {
        let mut c = self.control();
        c.settle();
        if c.reader.is_some() && !c.alive() {
            c.reader = None;
            c.send_close();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn writer(width: usize) -> Box<TapWriter> {
        Box::new(TapReader::standalone(TapId(0), width, Samples(8)).1)
    }

    fn width(h: &Hearing) -> Option<usize> {
        h.writer.as_ref().map(|w| w.width())
    }

    /// With the retire queue full the audio thread keeps the writer it has and
    /// leaves the install queued: it never drops the old writer (a drop in the
    /// engine's `RtScope` panics in debug, and the writer would be gone in
    /// release, which the width check sees) and loses nothing. Once the control
    /// side frees the queue, the swap goes through and the old writer is sent
    /// back. Unreachable through the public calls (each drains the retire queue
    /// first), so the queues are filled by hand.
    ///
    /// Mutation (run, recorded here): drop the `retire.vacant_len() > 0` guard
    /// in `swap` -> the full queue refuses the old writer, `swap` forgets it
    /// and replaces it, and the first width assertion fails.
    #[test]
    fn a_full_retire_queue_holds_the_install_and_drops_nothing() {
        let (taps, mut h) = link();
        h.writer = Some(writer(2));
        while h.retire.vacant_len() > 0 {
            assert!(h.retire.try_push(writer(9)).is_ok());
        }
        let full = h.retire.occupied_len();
        taps.control()
            .install
            .try_push(Some(writer(3)))
            .expect("room");
        {
            let _rt = tutti_rt::RtScope::enter();
            h.swap();
        }
        assert_eq!(width(&h), Some(2), "the writer is kept");
        assert_eq!(h.install.occupied_len(), 1, "the install waits");
        assert_eq!(h.retire.occupied_len(), full, "nothing was sent or dropped");

        taps.reclaim();
        {
            let _rt = tutti_rt::RtScope::enter();
            h.swap();
        }
        assert_eq!(width(&h), Some(3), "the install went through");
        assert_eq!(h.install.occupied_len(), 0);
        assert_eq!(h.retire.occupied_len(), 1, "the old writer was sent back");
    }
}
