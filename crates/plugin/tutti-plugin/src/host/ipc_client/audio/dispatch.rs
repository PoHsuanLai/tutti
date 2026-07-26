//! Per-command IPC choreography for the bridge thread.

use super::channels::Channels;
use super::messages::{AudioResponse, BridgeEvent, Command, ResyncKind};
use super::payload_pool::PayloadPool;
use crate::error::Result;
use crate::protocol::{BridgeMessage, HostMessage, IpcMidiEvent, MidiEvent, ProcessAudioData};
use crate::util::transport::control::{self as ipc, ControlStream};
use std::time::Duration;

/// How many block periods the bridge thread will wait for a `ProcessAudio`
/// reply before abandoning the block.
///
/// This has to be bounded by the *block period*, not an absolute duration. The
/// audio thread gives up on a block after a fraction of one period (see
/// `wait_budget_for`); if the bridge thread waits far longer it stays blocked in
/// `recv_reply` and — because `pump` is a single loop — dequeues no further
/// commands while it waits. The audio thread meanwhile keeps pushing one command
/// per block, so a single slow reply used to overflow the 128-slot command queue
/// and produce a sustained run of silence long after the server recovered. With
/// a 500 ms constant against a 667 µs budget (64 frames @ 48 kHz) that was a
/// ~750x mismatch and roughly 750 queued blocks.
///
/// A small multiple rather than the audio thread's own budget: the bridge is
/// allowed to still be waiting when the audio thread has already given up (its
/// reply then lands for a later block, or is discarded by `answers`), which
/// absorbs ordinary jitter without stalling the queue. Beyond a few periods the
/// reply is worthless anyway — the block it answers is long gone.
const PROCESS_TIMEOUT_PERIODS: u32 = 4;

/// Floor for the `ProcessAudio` reply timeout. At very short periods the
/// computed timeout drops under the scheduler's own wake latency (~0.7-1 ms
/// measured on this machine), and a bridge thread that times out before the
/// server could realistically have been scheduled would abandon every block on
/// a busy machine. Unlike the audio thread — which genuinely cannot spend 1 ms
/// at 64 frames / 192 kHz — the bridge thread is *not* on a deadline, so a floor
/// is safe here and merely stops it giving up prematurely.
const MIN_PROCESS_TIMEOUT: Duration = Duration::from_millis(2);

/// Ceiling, for the degenerate case of a very large block: past this the reply
/// cannot help any live block and continuing to wait only starves the queue.
const MAX_PROCESS_TIMEOUT: Duration = Duration::from_millis(50);

const PARAM_TIMEOUT: Duration = Duration::from_secs(5);
const STATE_TIMEOUT: Duration = Duration::from_secs(10);

/// The bridge thread's reply timeout for one block: [`PROCESS_TIMEOUT_PERIODS`]
/// of that block's own period, clamped to
/// [`MIN_PROCESS_TIMEOUT`]..=[`MAX_PROCESS_TIMEOUT`].
///
/// `num_samples`/`rate` stay raw here for the same reason as in
/// `wait_budget_for`: both come off the IPC wire, where the unit mandate stops.
fn process_timeout(num_samples: usize, rate: f64) -> Duration {
    let rate = if rate.is_finite() && rate > 0.0 {
        rate
    } else {
        super::FALLBACK_SAMPLE_RATE
    };
    let period = Duration::from_secs_f64(num_samples as f64 / rate);
    (period * PROCESS_TIMEOUT_PERIODS).clamp(MIN_PROCESS_TIMEOUT, MAX_PROCESS_TIMEOUT)
}

pub(super) fn handle(
    cmd: Command,
    stream: &mut ControlStream,
    channels: &Channels,
    payloads: &PayloadPool,
) -> Result<()> {
    match cmd {
        Command::Process(mut payload) => {
            let sent_id = payload.buffer_id;
            let num_samples = payload.num_samples;
            let msg = HostMessage::ProcessAudio(Box::new(ProcessAudioData {
                buffer_id: payload.buffer_id,
                num_samples: payload.num_samples,
                midi_events: payload.midi_events.iter().map(IpcMidiEvent::from).collect(),
                param_changes: core::mem::take(&mut payload.param_changes),
                note_expression: core::mem::take(&mut payload.note_expression),
                chords: core::mem::take(&mut payload.chords),
                scales: core::mem::take(&mut payload.scales),
                expr_texts: core::mem::take(&mut payload.expr_texts),
                expr_ints: core::mem::take(&mut payload.expr_ints),
                transport: core::mem::take(&mut payload.transport),
            }));

            payloads.recycle(payload);

            ipc::send(stream, &msg)?;

            let timeout = process_timeout(num_samples, channels.sample_rate());
            match recv_reply(stream, channels, timeout)? {
                BridgeMessage::AudioProcessed {
                    buffer_id,
                    midi_out,
                    ..
                } => {
                    // Convert IpcMidiEvent → MidiEvent HERE, on the bridge
                    // thread (off-RT). The RT thread only drains the built
                    // SmallVec — no per-event conversion, no heap traffic on
                    // the audio thread.
                    let midi_out = midi_out.iter().map(|e| MidiEvent::from(*e)).collect();
                    // The server echoes the id it was sent. If it ever failed to
                    // (a peer that predates the echo would send 0), the waiting
                    // audio thread times out into silence rather than reading a
                    // slab region nobody wrote — that is the whole point of the
                    // echo, so pass it through verbatim without repairing it.
                    channels.push_audio_response(AudioResponse::AudioProcessed {
                        buffer_id,
                        midi_out,
                    });
                }
                BridgeMessage::Error { .. } => {
                    // Attributable to this request: the server answered it.
                    channels.push_audio_response(AudioResponse::Error {
                        buffer_id: Some(sent_id),
                    });
                }
                _ => {}
            }
        }
        Command::SetParameter { param_id, value } => {
            ipc::send(stream, &HostMessage::SetParameter { param_id, value })?;
        }
        Command::SetAutomationState { mode } => {
            ipc::send(stream, &HostMessage::SetAutomationState { mode })?;
        }
        Command::SetSampleRate { rate } => {
            ipc::send(stream, &HostMessage::SetSampleRate { rate })?;
        }
        Command::Reset => {
            ipc::send(stream, &HostMessage::Reset)?;
        }
        Command::Shutdown => {
            ipc::send(stream, &HostMessage::Shutdown)?;
        }
        Command::SaveState { reply } => {
            ipc::send(stream, &HostMessage::SaveState)?;
            let value = match recv_reply(stream, channels, STATE_TIMEOUT)? {
                BridgeMessage::StateData { data } => Some(data),
                _ => None,
            };
            reply.send(value);
        }
        Command::LoadState { data, reply } => {
            ipc::send(stream, &HostMessage::LoadState { data })?;
            reply.send(true);
        }
        Command::GetParameterList { reply } => {
            ipc::send(stream, &HostMessage::GetParameterList)?;
            let value = match recv_reply(stream, channels, PARAM_TIMEOUT)? {
                BridgeMessage::ParameterList { parameters } => Some(parameters),
                _ => None,
            };
            reply.send(value);
        }
        Command::GetParameter { param_id, reply } => {
            ipc::send(stream, &HostMessage::GetParameter { param_id })?;
            let value = match recv_reply(stream, channels, PARAM_TIMEOUT)? {
                BridgeMessage::ParameterValue { value } => value,
                _ => None,
            };
            reply.send(value);
        }
    }
    Ok(())
}

/// Drain unsolicited events, returning the next reply-type message.
fn recv_reply(
    stream: &mut ControlStream,
    channels: &Channels,
    timeout: Duration,
) -> Result<BridgeMessage> {
    loop {
        match ipc::recv_within(stream, timeout)? {
            BridgeMessage::LatencyChanged { samples } => {
                channels.push_unsolicited(BridgeEvent::LatencyChanged { samples });
            }
            BridgeMessage::ParameterChanged { index, value } => {
                channels.push_unsolicited(BridgeEvent::ParameterChanged { index, value });
            }
            BridgeMessage::PluginParamValuesChanged => {
                channels.push_unsolicited(BridgeEvent::Resync(ResyncKind::ParamValues));
            }
            BridgeMessage::PluginParamTitlesChanged => {
                channels.push_unsolicited(BridgeEvent::Resync(ResyncKind::ParamTitles));
            }
            BridgeMessage::PluginIoChanged => {
                channels.push_unsolicited(BridgeEvent::Resync(ResyncKind::Io));
            }
            BridgeMessage::PluginReloaded => {
                channels.push_unsolicited(BridgeEvent::Resync(ResyncKind::Reloaded));
            }
            msg => return Ok(msg),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::ipc_client::audio::wait_budget_for;

    /// Every production block size / rate combination the engine can present.
    /// `BATCH_SIZE` is 64 today, but the timeout must hold if that changes.
    const RATES: [f64; 4] = [44_100.0, 48_000.0, 96_000.0, 192_000.0];
    const BLOCKS: [usize; 4] = [64, 128, 256, 512];

    fn period(num_samples: usize, rate: f64) -> Duration {
        Duration::from_secs_f64(num_samples as f64 / rate)
    }

    /// The regression this fix exists for: a fixed 500 ms timeout sat ~750x
    /// above the audio thread's 667 us budget at 64 frames / 48 kHz. While the
    /// bridge thread blocks in `recv_reply` it dequeues nothing, so the audio
    /// thread's one-command-per-block kept filling the 128-slot queue and one
    /// slow reply became a sustained run of silence. Bound it to a few periods.
    #[test]
    fn timeout_is_a_small_multiple_of_the_block_period() {
        for rate in RATES {
            for n in BLOCKS {
                let t = process_timeout(n, rate);
                let p = period(n, rate);
                let limit = p * PROCESS_TIMEOUT_PERIODS;
                assert!(
                    t <= limit.max(MIN_PROCESS_TIMEOUT),
                    "n={n} rate={rate}: timeout {t:?} exceeds {PROCESS_TIMEOUT_PERIODS} periods ({limit:?})"
                );
                assert!(
                    t <= MAX_PROCESS_TIMEOUT,
                    "n={n} rate={rate}: timeout {t:?} over the ceiling"
                );
            }
        }
    }

    /// The bridge must not be the first to give up: if it timed out before the
    /// audio thread did, the reply would be abandoned while a caller was still
    /// waiting for it, turning a recoverable late block into a guaranteed
    /// silent one.
    #[test]
    fn timeout_outlives_the_audio_threads_wait_budget() {
        for rate in RATES {
            for n in BLOCKS {
                let t = process_timeout(n, rate);
                let budget = wait_budget_for(n, rate);
                assert!(
                    t > budget,
                    "n={n} rate={rate}: bridge timeout {t:?} <= audio budget {budget:?}"
                );
            }
        }
    }

    /// A garbage rate off the wire must not produce a nonsense timeout.
    #[test]
    fn non_finite_rate_falls_back() {
        for bad in [f64::NAN, f64::INFINITY, 0.0, -48_000.0] {
            let t = process_timeout(64, bad);
            assert!(t >= MIN_PROCESS_TIMEOUT && t <= MAX_PROCESS_TIMEOUT, "rate={bad}: {t:?}");
        }
    }
}
