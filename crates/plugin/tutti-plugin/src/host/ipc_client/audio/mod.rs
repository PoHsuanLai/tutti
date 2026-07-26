//! RT-safe audio bridge between the host audio thread and the
//! plugin-server subprocess.
//!
//! Composition:
//! - [`Channels`] — the five lock-free queues (commands, two response
//!   paths, recycle, buffer-id counter).
//! - [`Lifecycle`] — running/crashed flags.
//! - [`AudioSlab`] — bulk audio transport (created elsewhere; held as Arc).
//!
//! The bridge thread lives in [`BridgeThread`]; its `Drop` shuts it down.

mod ask;
mod channels;
mod dispatch;
mod lifecycle;
mod messages;
mod payload_pool;
mod thread;

use crate::error::Result;
use crate::protocol::{
    ChordChanges, MidiEventVec, NoteExpressionChanges, NoteExpressionIntChanges,
    NoteExpressionTextChanges, ParameterChanges, ParameterInfo, ScaleChanges, TransportInfo,
};
use crate::util::transport::shm::AudioSlab;

/// VST3 sequencer-context inputs for one process block, bundled to keep
/// [`AudioBridge::submit`]'s signature manageable. All default to empty.
#[derive(Debug, Default, Clone)]
pub struct HarmonyInputs {
    pub chords: ChordChanges,
    pub scales: ScaleChanges,
    pub expr_texts: NoteExpressionTextChanges,
    pub expr_ints: NoteExpressionIntChanges,
}
use ask::ask;
use channels::Channels;
use lifecycle::Lifecycle;
use messages::{AudioResponse, Command};
use parking_lot::Mutex;
use payload_pool::PayloadPool;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub use messages::{BridgeEvent, PluginInvalidation, PluginRefresh, ResyncClass, ResyncKind};
pub use thread::BridgeThread;

const STATE_TIMEOUT: Duration = Duration::from_secs(10);
const PARAM_TIMEOUT: Duration = Duration::from_secs(5);

/// Listener for plugin-originated unsolicited events. Invoked on the
/// bridge thread (a tokio current-thread runtime) — blocking is OK, but
/// prefer posting to a channel and returning promptly.
pub type BridgeListener = Arc<dyn Fn(BridgeEvent) + Send + Sync>;

pub(super) type ListenerSlot = Arc<Mutex<Option<BridgeListener>>>;

/// Clone is cheap (Arc-based).
#[derive(Clone)]
pub struct AudioBridge {
    channels: Channels,
    payloads: PayloadPool,
    lifecycle: Lifecycle,
    listener: ListenerSlot,
    audio_buffer: Arc<AudioSlab>,
}

impl AudioBridge {
    pub fn new(
        socket_path: PathBuf,
        audio_buffer: Arc<AudioSlab>,
        sample_rate: f64,
    ) -> Result<(Self, BridgeThread)> {
        // The rate lives on `Channels`, not here: the bridge thread sizes its
        // own reply timeout from the same block period, and two copies of the
        // rate would let the two timeouts drift apart.
        let channels = Channels::new(sample_rate);
        let payloads = PayloadPool::new();
        let lifecycle = Lifecycle::new();
        let listener: ListenerSlot = Arc::new(Mutex::new(None));
        let thread = BridgeThread::spawn(
            channels.clone(),
            payloads.clone(),
            lifecycle.clone(),
            Arc::clone(&listener),
            socket_path,
        );
        let bridge = Self {
            channels,
            payloads,
            lifecycle,
            listener,
            audio_buffer,
        };
        Ok((bridge, thread))
    }

    /// Install a listener for plugin-originated unsolicited events. Pass
    /// `None` to clear. The callback runs on the bridge thread after each
    /// dispatched command.
    pub fn set_listener(&self, listener: Option<BridgeListener>) {
        *self.listener.lock() = listener;
    }

    pub fn is_crashed(&self) -> bool {
        self.lifecycle.is_crashed()
    }

    pub fn audio_buffer(&self) -> &Arc<AudioSlab> {
        &self.audio_buffer
    }

    // --- RT fire-and-forget ---

    pub fn set_parameter_rt(&self, param_id: u32, value: f32) -> bool {
        !self.lifecycle.is_crashed()
            && self
                .channels
                .push_command(Command::SetParameter { param_id, value })
    }

    pub fn set_automation_state_rt(&self, mode: crate::protocol::AutomationMode) -> bool {
        !self.lifecycle.is_crashed()
            && self
                .channels
                .push_command(Command::SetAutomationState { mode })
    }

    pub fn set_sample_rate_rt(&self, rate: f64) -> bool {
        // Publish before queueing, so both the audio thread's wait budget and
        // the bridge thread's reply timeout track the new period from here on.
        self.channels.set_sample_rate(rate);
        !self.lifecycle.is_crashed() && self.channels.push_command(Command::SetSampleRate { rate })
    }

    pub fn reset_rt(&self) -> bool {
        !self.lifecycle.is_crashed() && self.channels.push_command(Command::Reset)
    }

    // --- RT submit (fire-and-forget) ---

    /// Hand block `seq` to the bridge thread and return **immediately**.
    ///
    /// RT-safe in the strongest sense available: lock-free, allocation-free, and
    /// it does not wait at all. That is the point. The previous version blocked
    /// here for up to half the block period, which was individually defensible
    /// but summed across nodes — fundsp runs them serially in one callback, so
    /// three stalled plugins spent 3 × 667 µs against a 1333 µs deadline and
    /// overran it. Not waiting makes a stalled plugin cost zero, however many
    /// there are.
    ///
    /// The caller collects block `seq`'s *output* on a later call, gated on the
    /// slab's sequence number rather than on a reply (see
    /// `Batcher::collectable`). Returning `true` means only "the bridge accepted
    /// this block", never "the output is ready".
    ///
    /// # What still comes back through the queue
    ///
    /// Only the plugin's MIDI-out. The audio never travels this way — it is
    /// written into, and read back out of, the shared [`AudioSlab`]. Everything
    /// pending is drained into `midi_out` (cleared first); the caller-owned
    /// buffer reaches steady-state capacity so the `append` is alloc-free.
    ///
    /// Note the resulting skew: MIDI-out drained here belongs to a block that
    /// finished earlier, so its events' frame offsets are relative to *that*
    /// block. The caller shifts them; see `PluginClient::drain_midi_out`.
    #[allow(clippy::too_many_arguments)]
    pub fn submit(
        &self,
        seq: u64,
        num_samples: usize,
        midi_events: MidiEventVec,
        param_changes: ParameterChanges,
        note_expression: NoteExpressionChanges,
        harmony: HarmonyInputs,
        transport: TransportInfo,
        midi_out: &mut MidiEventVec,
    ) -> bool {
        midi_out.clear();
        if self.lifecycle.is_crashed() {
            return false;
        }

        // Drain whatever the bridge has finished since the last block. These
        // carry only the plugin's MIDI-out — the audio itself never travels
        // through this queue, and whether the *audio* is there is settled by the
        // slab's sequence numbers, not by a reply arriving.
        //
        // Draining rather than taking one: at ring depth 2 there is at most one
        // block in flight, but a reply for an abandoned block can still be
        // sitting here, and leaving it would put the queue one behind forever.
        while let Some(resp) = self.channels.pop_audio_response() {
            if let AudioResponse::AudioProcessed {
                midi_out: mut events,
                ..
            } = resp
            {
                midi_out.append(&mut events);
            }
        }

        let mut payload = self.payloads.acquire();
        payload.seq = seq;
        payload.num_samples = num_samples;
        payload.midi_events = midi_events;
        payload.param_changes = param_changes;
        payload.note_expression = note_expression;
        payload.chords = harmony.chords;
        payload.scales = harmony.scales;
        payload.expr_texts = harmony.expr_texts;
        payload.expr_ints = harmony.expr_ints;
        payload.transport = transport;

        // Publish the block number *before* queueing it, so the bridge thread
        // can never dequeue a command that looks newer than what the host admits
        // to having submitted. The reverse order would leave a window where a
        // block is judged against a stale `newest` and dropped as if it were two
        // blocks old.
        self.channels.note_submitted(seq);
        self.channels.push_command(Command::Process(payload))
    }

    // --- Main-thread sync request+response ---

    pub fn save_state(&self) -> Option<Vec<u8>> {
        if self.lifecycle.is_crashed() {
            return None;
        }
        let (ask_resp, reply) = ask::<Option<Vec<u8>>>();
        if !self.channels.push_command(Command::SaveState { reply }) {
            return None;
        }
        ask_resp.recv_timeout(STATE_TIMEOUT).ok().flatten()
    }

    pub fn load_state(&self, data: &[u8]) -> bool {
        if self.lifecycle.is_crashed() {
            return false;
        }
        let (ask_resp, reply) = ask::<bool>();
        if !self.channels.push_command(Command::LoadState {
            data: data.to_vec(),
            reply,
        }) {
            return false;
        }
        ask_resp.recv_timeout(STATE_TIMEOUT).unwrap_or(false)
    }

    pub fn parameters(&self) -> Option<Vec<ParameterInfo>> {
        if self.lifecycle.is_crashed() {
            return None;
        }
        let (ask_resp, reply) = ask::<Option<Vec<ParameterInfo>>>();
        if !self
            .channels
            .push_command(Command::GetParameterList { reply })
        {
            return None;
        }
        ask_resp.recv_timeout(PARAM_TIMEOUT).ok().flatten()
    }

    pub fn parameter(&self, param_id: u32) -> Option<f32> {
        if self.lifecycle.is_crashed() {
            return None;
        }
        let (ask_resp, reply) = ask::<Option<f32>>();
        if !self
            .channels
            .push_command(Command::GetParameter { param_id, reply })
        {
            return None;
        }
        ask_resp.recv_timeout(PARAM_TIMEOUT).ok().flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The negotiated rate is stored once and read live.
    ///
    /// This is what remains of a whole module of wait-budget tests, deleted with
    /// the synchronous path: the audio thread no longer waits, so it no longer
    /// sizes anything from the period. The *bridge* thread still does — its
    /// reply timeout in `dispatch` reads exactly this cell — so the storage has
    /// to keep working, and the test goes through the real `Channels` rather
    /// than a local atomic so it would fail if that plumbing were disconnected.
    #[test]
    fn the_sample_rate_is_stored_and_updated_live() {
        let channels = Channels::new(48_000.0);
        assert_eq!(channels.sample_rate(), 48_000.0);

        channels.set_sample_rate(192_000.0);
        assert_eq!(channels.sample_rate(), 192_000.0);
    }
}
