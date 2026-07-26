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

use super::messages::{AudioResponse, BridgeEvent, Command};
use crossbeam::queue::ArrayQueue;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::Thread;

const COMMAND_QUEUE_SIZE: usize = 128;
const RESPONSE_QUEUE_SIZE: usize = 128;
const EVENT_QUEUE_SIZE: usize = 128;

#[derive(Clone)]
pub(super) struct Channels {
    commands: Arc<ArrayQueue<Command>>,
    audio_responses: Arc<ArrayQueue<AudioResponse>>,
    unsolicited: Arc<ArrayQueue<BridgeEvent>>,
    buffer_id_counter: Arc<AtomicU32>,
    /// Handle to the bridge thread, so a pushed command can wake it
    /// immediately instead of waiting out its park timeout. Published once at
    /// spawn (see [`Self::register_worker`]) and read-only thereafter.
    worker: Arc<Mutex<Option<Thread>>>,
    /// The negotiated sample rate, as `f64::to_bits`. Lives here rather than
    /// only on `AudioBridge` because *both* threads size a timeout from the
    /// block period — the audio thread its wait budget, the bridge thread its
    /// reply timeout. One shared source keeps the two from drifting apart,
    /// which is what let a 500 ms constant sit ~750x above the audio thread's
    /// 667 µs budget and starve the command queue.
    sample_rate_bits: Arc<AtomicU64>,
}

impl Channels {
    pub(super) fn new(sample_rate: f64) -> Self {
        Self {
            commands: Arc::new(ArrayQueue::new(COMMAND_QUEUE_SIZE)),
            audio_responses: Arc::new(ArrayQueue::new(RESPONSE_QUEUE_SIZE)),
            unsolicited: Arc::new(ArrayQueue::new(EVENT_QUEUE_SIZE)),
            buffer_id_counter: Arc::new(AtomicU32::new(0)),
            worker: Arc::new(Mutex::new(None)),
            sample_rate_bits: Arc::new(AtomicU64::new(sample_rate.to_bits())),
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

    pub(super) fn next_buffer_id(&self) -> u32 {
        self.buffer_id_counter.fetch_add(1, Ordering::Relaxed)
    }

    /// The id the *next* `Process` will be issued — i.e. one past the newest
    /// block the audio thread has submitted.
    ///
    /// The bridge thread uses this to tell how far behind a command it just
    /// dequeued is, without draining the queue to look. `Relaxed` is right for
    /// the same reason as `next_buffer_id`'s: this only decides whether to skip
    /// work that is already provably useless, so reading a value one block stale
    /// costs at most one extra dead block.
    pub(super) fn issued_buffer_ids(&self) -> u32 {
        self.buffer_id_counter.load(Ordering::Relaxed)
    }

    /// Pushes, then wakes the bridge thread. `Thread::unpark` is a non-blocking
    /// futex/semaphore post — no allocation, no waiting — so it is safe from
    /// the audio thread, and it removes the poll-interval latency from the
    /// bounded wait in `AudioBridge::process`.
    ///
    /// `try_lock` on the worker slot keeps that promise absolute: the slot is
    /// written exactly once at spawn, so contention is effectively impossible,
    /// and if it ever did happen the bridge thread's park timeout still picks
    /// the command up.
    pub(super) fn push_command(&self, cmd: Command) -> bool {
        let pushed = self.commands.push(cmd).is_ok();
        if pushed {
            if let Some(guard) = self.worker.try_lock() {
                if let Some(thread) = guard.as_ref() {
                    thread.unpark();
                }
            }
        }
        pushed
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

    /// Overflow drops the oldest event to keep the producer lock-free.
    pub(super) fn push_unsolicited(&self, ev: BridgeEvent) {
        if self.unsolicited.push(ev).is_err() {
            let _ = self.unsolicited.pop();
        }
    }

    pub(super) fn pop_unsolicited(&self) -> Option<BridgeEvent> {
        self.unsolicited.pop()
    }
}
