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

pub use messages::{BridgeEvent, PluginInvalidation, PluginRefresh, ResyncClass, ResyncKind};
pub use thread::BridgeThread;

const STATE_TIMEOUT: Duration = Duration::from_secs(10);
const PARAM_TIMEOUT: Duration = Duration::from_secs(5);

/// Fraction of one block period that [`AudioBridge::process`] will spend
/// waiting for that block's reply before giving up and letting the caller emit
/// silence.
///
/// The budget is derived from the block period rather than fixed, because the
/// period is what the audio thread actually has and it spans two orders of
/// magnitude across host configurations (333 us for 64 samples at 192 kHz,
/// 10.7 ms for 512 at 48 kHz). A single constant is either over the whole
/// period at small block sizes or pointlessly generous at large ones.
///
/// 1/2 leaves the audio thread the other half of its period to finish the
/// block. Overshooting costs an audio-thread overrun (a dropout across the
/// WHOLE graph); undershooting costs one block of silence from this plugin
/// alone — so the split is deliberately conservative.
///
/// This fraction is the ONLY thing that sizes the budget. There is deliberately
/// no absolute floor or ceiling: an absolute bound cannot be correct for a
/// quantity defined relative to the block period. The previous 1 ms floor
/// exceeded the period at every block of 64 frames at 48 kHz and above (and 128
/// frames at 96 kHz and above), so on exactly the small-buffer configurations
/// this mechanism exists to protect it let the audio thread block for up to
/// three times its own deadline — a guaranteed dropout. The invariant is pinned
/// by the `budget_is_always_under_the_block_period` test.
///
/// No budget survives a genuinely saturated machine: measured under heavy CPU
/// contention this round-trip was seen taking 6 ms, well past any value that
/// respects a 1.33 ms period. That is the timeout working as intended — the
/// subprocess really cannot answer in time, and one block of silence is the
/// correct outcome. Likewise at very short periods the budget can fall under
/// the scheduler's own wake latency (~0.7-1 ms measured here), so a contended
/// bridge thread will miss it. That is not a bug to paper over with a floor:
/// at 64 frames / 192 kHz the audio thread genuinely does not have 1 ms to
/// give, and one block of silence beats an overrun of the whole graph.
const PROCESS_WAIT_FRACTION: u32 = 2;

/// Fallback sample rate for the budget calculation when the bridge has not been
/// told the real one yet. Only ever affects how long a stalled block waits.
pub(super) const FALLBACK_SAMPLE_RATE: f64 = 48_000.0;

/// The wait budget for one block: [`PROCESS_WAIT_FRACTION`] of the block's own
/// period, and nothing else. Because it is a strict fraction of the period, it
/// is strictly less than the period for every configuration — which is the
/// whole point, since the period is the audio thread's deadline.
///
/// `num_samples` and `rate` stay raw `usize`/`f64` rather than becoming
/// `Samples`/`SampleRate` newtypes: both arrive from the IPC/C-ABI side of this
/// module (`num_samples` is the wire field the server is handed, the rate is
/// what was sent over `SetSampleRate`), and the unit mandate stops at that
/// boundary.
///
/// A non-finite or non-positive rate falls back to [`FALLBACK_SAMPLE_RATE`]
/// rather than producing a nonsense duration.
fn wait_budget_for(num_samples: usize, rate: f64) -> Duration {
    let rate = if rate.is_finite() && rate > 0.0 {
        rate
    } else {
        FALLBACK_SAMPLE_RATE
    };
    let period = Duration::from_secs_f64(num_samples as f64 / rate);
    period / PROCESS_WAIT_FRACTION
}

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

    /// How long this block may wait for its reply: a strict fraction
    /// ([`PROCESS_WAIT_FRACTION`]) of its own period, so it can never exceed the
    /// deadline it exists to protect. Pure arithmetic — no allocation, no
    /// locking, safe on the audio thread.
    fn wait_budget(&self, num_samples: usize) -> Duration {
        wait_budget_for(num_samples, self.channels.sample_rate())
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

    // --- RT request+response ---

    /// Submit one block and wait, for a strictly bounded time, for the reply
    /// that belongs to **that** block.
    ///
    /// RT-safe: lock-free, allocation-free, and never blocks longer than this
    /// block's [`wait_budget`](Self::wait_budget) — half its own period. It
    /// does NOT wait indefinitely: a wedged or slow plugin subprocess returns
    /// `false` and the caller emits silence rather than the audio thread
    /// overrunning its deadline.
    ///
    /// # Why the wait has to match on `buffer_id`
    ///
    /// The audio itself never travels through this queue — it is written into
    /// (and read back out of) the shared [`AudioSlab`], which has no
    /// generation counter and whose `read_channel_into` reports success
    /// whether or not the server ever wrote the region. So "a reply arrived"
    /// is the only evidence that the slab holds this block's *output*. Taking
    /// any reply off the queue is not enough: taking the previous block's
    /// reply would mean reading the slab after this block's input write had
    /// already overwritten it — the host reading its own input back at unity
    /// gain, i.e. a silent bypass (single-bus layouts collapse to
    /// `output_base == 0`, so input and output share one region in place).
    /// Matching the echoed `buffer_id` is what rules that out.
    ///
    /// Stale replies (a block whose budget already expired, answered late) are
    /// discarded as they are encountered, so the queue cannot accumulate a
    /// permanent one-block skew.
    ///
    /// The plugin's MIDI-out for the block is drained into `midi_out` (cleared
    /// first); the caller-owned buffer reaches steady-state capacity so the
    /// `append` is alloc-free. Returns `false` on crash, timeout, or failure —
    /// in every one of those cases the caller must emit silence, never read the
    /// slab.
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
        let buffer_id = self.channels.next_buffer_id();
        payload.buffer_id = buffer_id;
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

        match self.await_response(buffer_id, self.wait_budget(num_samples)) {
            Some(AudioResponse::AudioProcessed {
                midi_out: mut events,
                ..
            }) => {
                midi_out.append(&mut events);
                true
            }
            // Timed out, or the block errored: silence, never a slab read.
            _ => false,
        }
    }

    /// Spin-wait up to `budget` for the response answering `buffer_id`,
    /// discarding any other response it meets on the way (those answer blocks
    /// that already gave up).
    ///
    /// Deliberately a spin with `yield_now`, not a park: the audio thread has
    /// no way to be sure it will be unparked (the bridge thread may be dead),
    /// and it must return within its budget regardless. `Instant::now` and
    /// `yield_now` are both syscall-light and allocation-free.
    fn await_response(&self, buffer_id: u32, budget: Duration) -> Option<AudioResponse> {
        let deadline = std::time::Instant::now() + budget;
        loop {
            while let Some(resp) = self.channels.pop_audio_response() {
                if resp.answers(buffer_id) {
                    return Some(resp);
                }
                // Not ours: a late reply to a block that already timed out.
                // Dropping it here is what keeps the queue from settling into a
                // permanent one-block skew.
            }
            if self.lifecycle.is_crashed() || std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::yield_now();
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

#[cfg(test)]
mod tests {
    use super::*;

    const RATES: [f64; 4] = [44_100.0, 48_000.0, 96_000.0, 192_000.0];
    const BLOCKS: [usize; 3] = [64, 128, 256];

    fn period(num_samples: usize, rate: f64) -> Duration {
        Duration::from_secs_f64(num_samples as f64 / rate)
    }

    /// THE invariant: the budget bounds how long the audio thread may block, so
    /// it must always be under the deadline it is bounding. A fixed 1 ms floor
    /// used to break this at 64 frames from 48 kHz up and at 128 frames from
    /// 96 kHz up — the exact small-buffer configurations the wait exists for.
    ///
    /// The matrix is the test: checking one configuration is what let the
    /// regression through.
    #[test]
    fn budget_is_always_under_the_block_period() {
        for rate in RATES {
            for n in BLOCKS {
                let budget = wait_budget_for(n, rate);
                let period = period(n, rate);
                assert!(
                    budget < period,
                    "budget {budget:?} >= period {period:?} at rate {rate} / {n} frames"
                );
            }
        }
    }

    /// Stronger than the invariant above, and the reason it holds: the budget is
    /// exactly the declared fraction of the period at every configuration — no
    /// clamp ever displaces it.
    #[test]
    fn budget_is_the_declared_fraction_of_the_period_everywhere() {
        for rate in RATES {
            for n in BLOCKS {
                let budget = wait_budget_for(n, rate);
                let expected = period(n, rate) / PROCESS_WAIT_FRACTION;
                assert_eq!(
                    budget, expected,
                    "budget was clamped away from period/{PROCESS_WAIT_FRACTION} \
                     at rate {rate} / {n} frames"
                );
            }
        }
    }

    /// The shortest period the engine supports is 64 frames at 192 kHz = 333 us;
    /// the old 1 ms floor was 3x that. Pin the worst case numerically so a future
    /// absolute bound cannot be reintroduced without this failing.
    #[test]
    fn shortest_supported_period_gets_a_sub_period_budget() {
        let budget = wait_budget_for(64, 192_000.0);
        assert!(
            budget < Duration::from_micros(333),
            "budget {budget:?} must be under the 333 us period at 192 kHz / 64 frames"
        );
        assert_eq!(budget, Duration::from_secs_f64(64.0 / 192_000.0) / 2);
    }

    /// A rate the bridge has not been told yet (or a garbage one over IPC) must
    /// still produce a sane, period-relative budget rather than a zero, infinite,
    /// or NaN duration.
    #[test]
    fn non_finite_or_zero_rate_falls_back() {
        let expected = wait_budget_for(128, FALLBACK_SAMPLE_RATE);
        for bad in [0.0, -48_000.0, f64::NAN, f64::INFINITY] {
            assert_eq!(
                wait_budget_for(128, bad),
                expected,
                "rate {bad} should have fallen back to {FALLBACK_SAMPLE_RATE}"
            );
        }
    }

    /// The budget follows the rate the bridge was last told about, which is what
    /// makes it track the real period after a `SetSampleRate`.
    ///
    /// Goes through the real `Channels` storage rather than a local atomic: the
    /// rate now lives there precisely so the bridge thread's reply timeout reads
    /// the same value, and a test that hand-rolls its own atomic would still pass
    /// if that plumbing were disconnected.
    #[test]
    fn budget_tracks_the_stored_sample_rate() {
        let channels = Channels::new(48_000.0);
        assert_eq!(
            wait_budget_for(64, channels.sample_rate()),
            wait_budget_for(64, 48_000.0)
        );

        channels.set_sample_rate(192_000.0);
        let after = wait_budget_for(64, channels.sample_rate());
        assert_eq!(after, wait_budget_for(64, 192_000.0));
        assert!(after < wait_budget_for(64, 48_000.0));
    }
}
