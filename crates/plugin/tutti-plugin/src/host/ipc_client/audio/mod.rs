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
/// [`AudioBridge::process`]'s signature manageable. All default to empty.
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

pub use messages::{
    BridgeEvent, PluginInvalidation, PluginRefresh, ResyncClass, ResyncKind,
};
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
    pub fn new(socket_path: PathBuf, audio_buffer: Arc<AudioSlab>) -> Result<(Self, BridgeThread)> {
        let channels = Channels::new();
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

    pub fn set_automation_state_rt(&self, state: i32) -> bool {
        !self.lifecycle.is_crashed()
            && self
                .channels
                .push_command(Command::SetAutomationState { state })
    }

    pub fn set_sample_rate_rt(&self, rate: f64) -> bool {
        !self.lifecycle.is_crashed() && self.channels.push_command(Command::SetSampleRate { rate })
    }

    pub fn reset_rt(&self) -> bool {
        !self.lifecycle.is_crashed() && self.channels.push_command(Command::Reset)
    }

    // --- RT request+response ---

    /// RT-safe, lock-free. Waits for the bridge thread's AudioResponse. The
    /// plugin's MIDI-out for the block is drained into `midi_out` (cleared
    /// first); the caller-owned buffer reaches steady-state capacity so the
    /// `append` is alloc-free. Returns `false` on crash / failure.
    #[allow(clippy::too_many_arguments)]
    pub fn process(
        &self,
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

        let mut payload = self.payloads.acquire();
        payload.buffer_id = self.channels.next_buffer_id();
        payload.num_samples = num_samples;
        payload.midi_events = midi_events;
        payload.param_changes = param_changes;
        payload.note_expression = note_expression;
        payload.chords = harmony.chords;
        payload.scales = harmony.scales;
        payload.expr_texts = harmony.expr_texts;
        payload.expr_ints = harmony.expr_ints;
        payload.transport = transport;

        if !self.channels.push_command(Command::Process(payload)) {
            return false;
        }
        match self.channels.pop_audio_response() {
            Some(AudioResponse::AudioProcessed {
                midi_out: mut events,
            }) => {
                midi_out.append(&mut events);
                true
            }
            _ => false,
        }
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
