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
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

const COMMAND_QUEUE_SIZE: usize = 128;
const RESPONSE_QUEUE_SIZE: usize = 128;
const EVENT_QUEUE_SIZE: usize = 128;

#[derive(Clone)]
pub(super) struct Channels {
    commands: Arc<ArrayQueue<Command>>,
    audio_responses: Arc<ArrayQueue<AudioResponse>>,
    unsolicited: Arc<ArrayQueue<BridgeEvent>>,
    buffer_id_counter: Arc<AtomicU32>,
}

impl Channels {
    pub(super) fn new() -> Self {
        Self {
            commands: Arc::new(ArrayQueue::new(COMMAND_QUEUE_SIZE)),
            audio_responses: Arc::new(ArrayQueue::new(RESPONSE_QUEUE_SIZE)),
            unsolicited: Arc::new(ArrayQueue::new(EVENT_QUEUE_SIZE)),
            buffer_id_counter: Arc::new(AtomicU32::new(0)),
        }
    }

    pub(super) fn next_buffer_id(&self) -> u32 {
        self.buffer_id_counter.fetch_add(1, Ordering::Relaxed)
    }

    pub(super) fn push_command(&self, cmd: Command) -> bool {
        self.commands.push(cmd).is_ok()
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
