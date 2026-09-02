//! RT-safe audio bridge between the host audio thread and the
//! plugin-server subprocess.
//!
//! Composition:
//! - [`Channels`] — the three lock-free queues (commands, audio responses,
//!   unsolicited events) plus the shared newest-sequence and sample-rate
//!   cells. Payload recycling lives in [`PayloadPool`]; the buffer-id counter
//!   is gone, the batcher owns the block sequence.
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
mod progress;
mod thread;

use crate::error::{Delivered, Result, StateError};
use crate::protocol::{
    ChordChanges, MidiEventVec, Normalized, NoteExpressionChanges, NoteExpressionIntChanges,
    NoteExpressionTextChanges, ParamAddress, ParameterChanges, ParameterInfo, Preset, PresetId,
    ScaleChanges, TransportInfo,
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
use progress::{StateProgress, PROGRESS_TIMEOUT};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub use messages::{BridgeEvent, PluginInvalidation, PluginRefresh, ResyncClass, ResyncKind};
pub use thread::BridgeThread;

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
    /// How long a state transfer may stand still before it is declared stalled.
    ///
    /// A field rather than a constant so a test can assert the stall path
    /// without spending [`PROGRESS_TIMEOUT`] of wall clock doing it. Production
    /// never sets it — [`AudioBridge::new`] installs the constant, and the
    /// override is `cfg(test)`-only, so there is no way to ship a bridge with a
    /// deadline nobody chose.
    state_progress_timeout: Duration,
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
            state_progress_timeout: PROGRESS_TIMEOUT,
        };
        Ok((bridge, thread))
    }

    /// Shorten the state-transfer progress deadline, for tests only.
    ///
    /// Kept out of non-test builds deliberately: the deadline is a liveness
    /// judgement the transport makes, not a knob a host should be able to turn
    /// down until healthy plugins start failing.
    #[cfg(test)]
    pub(crate) fn set_state_progress_timeout(&mut self, timeout: Duration) {
        self.state_progress_timeout = timeout;
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

    /// Why this bridge died, or `None` while it is alive.
    ///
    /// Latched at the detection site, so it answers for a crash that happened
    /// before any listener was installed — which the connect- and
    /// handshake-failure paths routinely do.
    pub fn crash_cause(&self) -> Option<String> {
        self.lifecycle.crash_cause()
    }

    /// How many replies the bridge thread has taken back off the socket after
    /// abandoning the blocks that asked for them.
    ///
    /// Instrumentation for the drain, which is otherwise unobservable: the audio
    /// it recovers is stale and rejected by the slab's sequence check, and the
    /// MIDI it forwards looks exactly like MIDI that arrived on time. A test
    /// that wants to pin the drain has to read this — the alternative is
    /// inferring it from how far replies lag, which is a function of machine
    /// load rather than of correctness.
    ///
    /// Monotonic over the life of the bridge, and `Relaxed`: nothing branches on
    /// it.
    ///
    /// `cfg(test)`: nothing in the shipping host reads this, and an
    /// always-compiled accessor with no caller reads as an API someone may
    /// depend on. The counter itself is unconditional — it is two atomic ops
    /// on a path that already blocks on a socket — so what the tests observe
    /// is the production code path, not a test-only variant of it.
    #[cfg(test)]
    pub fn settled_replies(&self) -> u64 {
        self.channels.settled()
    }

    pub fn audio_buffer(&self) -> &Arc<AudioSlab> {
        &self.audio_buffer
    }

    // --- RT fire-and-forget ---

    pub fn set_parameter_rt(&self, param_id: ParamAddress, value: f32) -> bool {
        !self.lifecycle.is_crashed()
            && self
                .channels
                .push_command(Command::SetParameter { param_id, value })
    }

    /// Queue an automation-state push, saying **why** if it did not go.
    ///
    /// The one member of this family whose answer a caller reads and turns into
    /// a user-visible message (`SubprocessBackend::set_automation_mode`). The
    /// other nine are `let _ =`'d or forwarded, so they keep the bool: an enum
    /// nobody matches on is ceremony, and this family is RT-adjacent.
    ///
    /// The two causes want opposite responses — a dead plugin will answer the
    /// same forever, a full queue may take the very next call — and a `bool`
    /// merged them into one "not delivered" that a caller could only report,
    /// never act on.
    pub fn set_automation_state_rt(&self, mode: crate::protocol::AutomationMode) -> Delivered {
        if self.lifecycle.is_crashed() {
            return Delivered::PluginDead;
        }
        if self
            .channels
            .push_command(Command::SetAutomationState { mode })
        {
            Delivered::Yes
        } else {
            Delivered::Dropped
        }
    }

    pub fn set_sample_rate_rt(&self, rate: f64) -> bool {
        // Publish before queueing, so both the staleness bound and
        // the bridge thread's reply timeout track the new period from here on.
        self.channels.set_sample_rate(rate);
        !self.lifecycle.is_crashed() && self.channels.push_command(Command::SetSampleRate { rate })
    }

    /// Queue a render-mode change for the plugin.
    ///
    /// Named `_rt` like its neighbours because it shares their bounded,
    /// non-blocking queue, not because the audio thread is the expected caller:
    /// a bounce sets this once from the control thread before it starts
    /// pulling. Riding the same queue is what keeps it ordered against the
    /// blocks around it — a mode that overtook an in-flight block would apply
    /// to audio the caller thought was already rendered.
    pub fn set_render_mode_rt(&self, mode: crate::protocol::RenderMode) -> bool {
        !self.lifecycle.is_crashed() && self.channels.push_command(Command::SetRenderMode { mode })
    }

    pub fn reset_rt(&self) -> bool {
        !self.lifecycle.is_crashed() && self.channels.push_command(Command::Reset)
    }

    // --- RT submit (fire-and-forget) ---

    /// Hand block `seq` to the bridge thread and return **immediately**.
    ///
    /// Lock-free, allocation-free, and it never waits — see the module doc on
    /// `Batcher` for why waiting here was the defect rather than a tuning problem.
    ///
    /// The caller collects block `seq`'s *output* on a later call, gated on the
    /// slab's sequence number rather than on a reply (see `Batcher::collectable`).
    /// Returning `true` means only "the bridge accepted this block", never "the
    /// output is ready".
    ///
    /// # What still comes back through the queue
    ///
    /// Only the plugin's MIDI-out; audio travels through the shared [`AudioSlab`]
    /// in both directions. Everything pending is drained into `midi_out` (cleared
    /// first); the caller-owned buffer reaches steady-state capacity so the
    /// `append` is alloc-free.
    ///
    /// Those events' frame offsets are relative to the *earlier* block that
    /// produced them. The caller shifts them; see `PluginClient::drain_midi_out`.
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

    pub fn save_state(&self) -> std::result::Result<Vec<u8>, StateError> {
        if self.lifecycle.is_crashed() {
            return Err(StateError::PluginCrashed);
        }
        let (ask_resp, reply) = ask::<std::result::Result<Vec<u8>, StateError>>();
        let progress = StateProgress::new(self.state_progress_timeout);
        if !self.channels.push_command(Command::SaveState {
            progress: progress.clone(),
            reply,
        }) {
            // The queue refused the command, which on this path means the
            // bridge thread is gone.
            return Err(StateError::PluginCrashed);
        }
        // As in `load_state`: a refusal to answer is not an empty state.
        // Collapsing it to `None`/`vec![]` is what let an unsaved preset look
        // like a plugin that had nothing to save.
        //
        // The wait ends on a stalled *transfer*, not on a total elapsed budget.
        // A fixed total here was the effective ceiling on state size — it
        // expired mid-stream on a large-but-legal state and reported it as a
        // plugin that never answered.
        progress.wait(ask_resp)
    }

    pub fn load_state(&self, data: &[u8]) -> std::result::Result<(), StateError> {
        if self.lifecycle.is_crashed() {
            return Err(StateError::PluginCrashed);
        }
        // Refuse here as well as in the dispatcher. Not redundant: this saves
        // copying a gigabyte into a command that is only going to be rejected,
        // and it answers on the calling thread rather than after a round trip
        // through the bridge.
        if data.len() > crate::protocol::MAX_STATE_BYTES {
            return Err(StateError::TooLarge {
                bytes: data.len(),
                limit: crate::protocol::MAX_STATE_BYTES,
            });
        }
        let (ask_resp, reply) = ask::<std::result::Result<(), StateError>>();
        let progress = StateProgress::new(self.state_progress_timeout);
        if !self.channels.push_command(Command::LoadState {
            data: data.to_vec(),
            progress: progress.clone(),
            reply,
        }) {
            // The queue refused the command, which on this path means the
            // bridge thread is gone — the plugin is unreachable either way.
            return Err(StateError::PluginCrashed);
        }
        // A refusal to answer is not an acceptance. Before this the fallback was
        // `false`, which a `()`-returning caller could not see.
        //
        // Same progress deadline as `save_state`, for the same reason: the write
        // direction is chunked too, so a total budget capped how large a state
        // could be *sent* just as it capped how large one could be received.
        progress.wait(ask_resp)
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

    /// The plugin's preset list, or `None` when the subprocess is gone.
    ///
    /// `None` and `Some(vec![])` are different answers: the first means the
    /// question could not be asked, the second that the plugin listed nothing.
    pub fn presets(&self) -> Option<Vec<Preset>> {
        if self.lifecycle.is_crashed() {
            return None;
        }
        let (ask_resp, reply) = ask::<Option<Vec<Preset>>>();
        if !self.channels.push_command(Command::GetPresetList { reply }) {
            return None;
        }
        ask_resp.recv_timeout(PARAM_TIMEOUT).ok().flatten()
    }

    /// Ask the plugin to load a preset. `false` when it refused, the format has
    /// no load path, or the subprocess is gone — all three mean the preset did
    /// not load, and a caller must leave its selection where it was.
    pub fn load_preset(&self, id: PresetId) -> bool {
        if self.lifecycle.is_crashed() {
            return false;
        }
        let (ask_resp, reply) = ask::<bool>();
        if !self
            .channels
            .push_command(Command::LoadPreset { id, reply })
        {
            return false;
        }
        ask_resp.recv_timeout(PARAM_TIMEOUT).unwrap_or(false)
    }

    /// Which preset the plugin considers current, if it will say.
    pub fn current_preset(&self) -> Option<PresetId> {
        if self.lifecycle.is_crashed() {
            return None;
        }
        let (ask_resp, reply) = ask::<Option<PresetId>>();
        if !self
            .channels
            .push_command(Command::GetCurrentPreset { reply })
        {
            return None;
        }
        ask_resp.recv_timeout(PARAM_TIMEOUT).ok().flatten()
    }

    pub fn parameter(&self, param_id: ParamAddress) -> Option<f32> {
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

    /// The plugin's display string for `value` on one parameter.
    ///
    /// `None` when the subprocess is gone, the request could not be queued, or
    /// the plugin declined — all three mean there is no text, and a caller
    /// renders the raw number.
    pub fn parameter_text(&self, param_id: ParamAddress, value: Normalized) -> Option<String> {
        if self.lifecycle.is_crashed() {
            return None;
        }
        let (ask_resp, reply) = ask::<Option<String>>();
        if !self.channels.push_command(Command::GetParameterText {
            param_id,
            value,
            reply,
        }) {
            return None;
        }
        ask_resp.recv_timeout(PARAM_TIMEOUT).ok().flatten()
    }

    /// The value the plugin parses `text` into.
    ///
    /// `None` when it cannot parse the string, on the same three failure paths
    /// as [`parameter_text`](Self::parameter_text). A caller must leave its
    /// field unchanged rather than substituting a fallback.
    pub fn parameter_value_from_text(
        &self,
        param_id: ParamAddress,
        text: &str,
    ) -> Option<Normalized> {
        if self.lifecycle.is_crashed() {
            return None;
        }
        let (ask_resp, reply) = ask::<Option<Normalized>>();
        if !self
            .channels
            .push_command(Command::GetParameterValueFromText {
                param_id,
                text: text.to_string(),
                reply,
            })
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
    /// The audio thread never waits, so it sizes nothing from the period. The
    /// *bridge* thread does — its reply timeout in `dispatch` reads exactly this
    /// cell — so the storage has to keep working. The test goes through the real
    /// `Channels` rather than a local atomic, so it fails if that plumbing is
    /// disconnected.
    #[test]
    fn the_sample_rate_is_stored_and_updated_live() {
        let channels = Channels::new(48_000.0);
        assert_eq!(channels.sample_rate(), 48_000.0);

        channels.set_sample_rate(192_000.0);
        assert_eq!(channels.sample_rate(), 192_000.0);
    }
}
