//! Lock-free message bus between audio thread and bridge thread.
//!
//! Two queues survive at this level: the command queue (audio thread → bridge
//! thread) and the audio-response queue (bridge thread → audio thread, RT).
//! Reply-bearing commands carry their own per-request `Reply<T>` so each
//! response is routed back to its specific caller — no shared
//! control-response queue, no risk of mismatched responses.
//!
//! `ProcessPayload` recycling lives in [`PayloadPool`](super::payload_pool::PayloadPool),
//! a separate primitive whose `acquire`/`recycle` names document the RT
//! contract.

use super::messages::{AudioResponse, Command};
use crossbeam::queue::ArrayQueue;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::Thread;

/// How many commands the audio thread can queue before `push_command` fails.
/// This also bounds how many `Process` payloads can be outstanding at once,
/// which is why [`super::payload_pool::PayloadPool`] sizes itself from it.
pub(super) const COMMAND_QUEUE_SIZE: usize = 128;
const RESPONSE_QUEUE_SIZE: usize = 128;

#[derive(Clone)]
pub(super) struct Channels {
    commands: Arc<ArrayQueue<Command>>,
    audio_responses: Arc<ArrayQueue<AudioResponse>>,
    /// The newest block sequence the audio thread has submitted. Owned here
    /// rather than on the batcher because the *bridge* thread is what reads it,
    /// to decide whether a dequeued block is still worth sending.
    newest_seq: Arc<AtomicU64>,
    /// Handle to the bridge thread, so a pushed command can wake it
    /// immediately instead of waiting out its park timeout. Published once at
    /// spawn (see [`Self::register_worker`]) and read-only thereafter.
    worker: Arc<Mutex<Option<Thread>>>,
    /// The negotiated sample rate, as `f64::to_bits`. Lives here rather than
    /// only on `AudioBridge` because *both* threads size a timeout from the
    /// block period — the bridge thread its reply timeout, and the staleness
    /// bound its notion of how far behind a block may be. One shared source
    /// keeps the two from drifting apart: two independently-derived periods let
    /// a constant sit orders of magnitude off the real block period, which
    /// starves the command queue.
    sample_rate_bits: Arc<AtomicU64>,
    /// How many owed replies the drain has taken back off the socket, ever.
    ///
    /// Pure instrumentation, and the only externally visible evidence that the
    /// drain ran at all. Everything else it does is invisible by construction:
    /// the audio it recovers is already stale and rejected by the slab's
    /// sequence check, and the MIDI it forwards is indistinguishable from MIDI
    /// that arrived on time. Without this counter a test can only observe the
    /// drain's *absence* statistically — as a backlog that grows over a long
    /// enough run — which is a load-dependent assertion of exactly the kind this
    /// work exists to remove.
    settled: Arc<AtomicU64>,
    /// How many blocks the bridge thread abandoned on a missed budget, ever.
    /// The other half of [`settled`](Self::settled): every abandoned reply is
    /// owed back, so a test can bound what the drain took by what was owed
    /// instead of by how many budgets a loaded machine happened to miss.
    abandoned: Arc<AtomicU64>,
    /// How many unsolicited parameter events the bridge thread dropped
    /// because more arrived during one command than its inbox holds, ever.
    /// Each drop also raises a `Resync(ParamValues)`, so the host re-reads.
    dropped_param_events: Arc<AtomicU64>,
    /// MIDI events the bridge thread did not send, ever: a stale block's,
    /// and the rest of a SysEx that lost packets with it. Shared with the
    /// plugin node's controls, which add it to the node's own count.
    dropped_midi: Arc<AtomicU64>,
}

impl Channels {
    pub(super) fn new(sample_rate: f64) -> Self {
        Self {
            commands: Arc::new(ArrayQueue::new(COMMAND_QUEUE_SIZE)),
            audio_responses: Arc::new(ArrayQueue::new(RESPONSE_QUEUE_SIZE)),
            newest_seq: Arc::new(AtomicU64::new(0)),
            worker: Arc::new(Mutex::new(None)),
            sample_rate_bits: Arc::new(AtomicU64::new(sample_rate.to_bits())),
            settled: Arc::new(AtomicU64::new(0)),
            abandoned: Arc::new(AtomicU64::new(0)),
            dropped_param_events: Arc::new(AtomicU64::new(0)),
            dropped_midi: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The current sample rate. `Relaxed` suffices: it only sizes a timeout, so
    /// reading a value one block stale is harmless.
    pub(super) fn sample_rate(&self) -> f64 {
        f64::from_bits(self.sample_rate_bits.load(Ordering::Relaxed))
    }

    pub(super) fn set_sample_rate(&self, rate: f64) {
        self.sample_rate_bits
            .store(rate.to_bits(), Ordering::Relaxed);
    }

    /// Called once by the bridge thread with its own handle, before it starts
    /// polling. Until then `push_command` simply doesn't unpark and the thread
    /// falls back on its park timeout.
    pub(super) fn register_worker(&self, thread: Thread) {
        *self.worker.lock() = Some(thread);
    }

    /// Record that the audio thread has submitted block `seq`. Called from the
    /// RT path, so it is a single relaxed store and nothing else.
    pub(super) fn note_submitted(&self, seq: u64) {
        self.newest_seq.store(seq, Ordering::Relaxed);
    }

    /// The newest block the audio thread has submitted.
    ///
    /// The bridge thread uses this to tell how far behind a command it just
    /// dequeued is, without draining the queue to look. `Relaxed` suffices: it
    /// only decides whether to skip work that is already provably useless, so
    /// reading a value one block stale costs at most one extra dead block.
    pub(super) fn newest_submitted(&self) -> u64 {
        self.newest_seq.load(Ordering::Relaxed)
    }

    /// Record that the drain took `n` owed replies off the socket. Called by the
    /// bridge thread only. `Release`, paired with the `Acquire` in
    /// [`settled`](Self::settled): a reader that sees a settle also sees the
    /// [`note_abandoned`](Self::note_abandoned) that owed it, so a test may
    /// assert `settled <= abandoned` by reading `settled` first.
    pub(super) fn note_settled(&self, n: u32) {
        if n > 0 {
            self.settled.fetch_add(n as u64, Ordering::Release);
        }
    }

    /// How many owed replies the drain has taken, over the life of the bridge.
    #[cfg(test)]
    pub(super) fn settled(&self) -> u64 {
        self.settled.load(Ordering::Acquire)
    }

    /// Record that a block's reply missed its budget and is now owed. Called
    /// by the bridge thread only, before any settle of that reply; `Relaxed`
    /// because the `Release` on [`note_settled`](Self::note_settled) orders it.
    pub(super) fn note_abandoned(&self) {
        self.abandoned.fetch_add(1, Ordering::Relaxed);
    }

    /// How many blocks' replies were abandoned, over the life of the bridge.
    #[cfg(test)]
    pub(super) fn abandoned(&self) -> u64 {
        self.abandoned.load(Ordering::Relaxed)
    }

    /// Record `n` dropped unsolicited parameter events. Bridge thread only;
    /// `Relaxed`: a count nothing branches on.
    pub(super) fn note_dropped_param_events(&self, n: u64) {
        if n > 0 {
            self.dropped_param_events.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// Record `n` MIDI events not sent. Bridge thread only; `Relaxed`: a
    /// count nothing branches on.
    pub(super) fn note_dropped_midi(&self, n: u64) {
        if n > 0 {
            self.dropped_midi.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// The cell [`note_dropped_midi`](Self::note_dropped_midi) counts into.
    pub(super) fn dropped_midi_cell(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.dropped_midi)
    }

    /// How many unsolicited parameter events were dropped, over the life of
    /// the bridge.
    pub(super) fn dropped_param_events(&self) -> u64 {
        self.dropped_param_events.load(Ordering::Relaxed)
    }

    /// Pushes, then wakes the bridge thread. `Thread::unpark` is a non-blocking
    /// futex/semaphore post — no allocation, no waiting — so it is safe from
    /// the audio thread, and it starts the socket round-trip immediately rather
    /// than after a poll interval. The audio thread never waits on the reply,
    /// but the unpark still matters — it is what keeps a block's reply arriving
    /// in time to be collected on the *next* block rather than the one after.
    ///
    /// `try_lock` on the worker slot keeps that promise absolute: the slot is
    /// written exactly once at spawn, so contention is effectively impossible,
    /// and if it ever did happen the bridge thread's park timeout still picks
    /// the command up.
    pub(super) fn push_command(&self, cmd: Command) -> bool {
        self.try_push_command(cmd).is_ok()
    }

    /// [`push_command`](Self::push_command), handing a refused command back
    /// rather than dropping it, so the audio thread can return what it holds
    /// to its pools instead of freeing it.
    pub(super) fn try_push_command(&self, cmd: Command) -> Result<(), Command> {
        self.commands.push(cmd)?;
        if let Some(guard) = self.worker.try_lock() {
            if let Some(thread) = guard.as_ref() {
                thread.unpark();
            }
        }
        Ok(())
    }

    pub(super) fn pop_command(&self) -> Option<Command> {
        self.commands.pop()
    }

    pub(super) fn push_audio_response(&self, resp: AudioResponse) {
        let _ = self.audio_responses.push(resp);
    }

    pub(super) fn pop_audio_response(&self) -> Option<AudioResponse> {
        self.audio_responses.pop()
    }
}
