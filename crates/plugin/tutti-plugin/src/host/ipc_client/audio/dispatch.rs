//! Per-command IPC choreography for the bridge thread.

use super::channels::Channels;
use super::messages::{AudioResponse, BridgeEvent, Command, ResyncKind};
use super::payload_pool::PayloadPool;
use crate::error::Result;
use crate::protocol::{BridgeMessage, HostMessage, IpcMidiEvent, MidiEvent, ProcessAudioData};
use crate::util::transport::control::{self as ipc, ControlStream};
use crate::util::transport::shm::RING_SLOTS;
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

/// Fallback sample rate when the bridge has not been told the real one yet.
/// Only ever affects how long the bridge thread waits for a reply before
/// abandoning a block.
const FALLBACK_SAMPLE_RATE: f64 = 48_000.0;

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

/// How many blocks behind the newest submitted block a queued `Process` may be
/// and still be worth sending.
///
/// **This is the ring depth, not an independent tunable.** A reply is unusable
/// once its slab slot has been overwritten — that is, once the host is more than
/// [`RING_SLOTS`] blocks past it — so the two numbers express one constraint.
/// Raising this without raising the ring would keep work whose destination has
/// already been recycled; lowering it would drop work that was still usable.
/// Deriving it is what stops a later edit from splitting them: two constants
/// that should have been one is precisely how the 750x reply-timeout mismatch
/// happened.
///
/// It answers a *different* question from [`process_timeout`], and the two must
/// not be conflated either. The timeout asks "is this plugin still responding?";
/// this asks "is this reply still wanted?". Collapsing them — by capping the
/// timeout low enough to force a catch-up — would answer the second by breaking
/// the first, abandoning slow-but-working plugins on every block.
///
/// Dropping makes the bridge catch up in one step instead of grinding through a
/// backlog of dead work. The honest cost: a stateful plugin (a delay line, a
/// reverb tail) whose input blocks are skipped has its internal state diverge
/// from a continuous signal, so it glitches on recovery rather than cleanly
/// silencing. That is unavoidable in any design that does not stall the audio
/// thread, and it only happens when the plugin is already failing to keep up.
const MAX_BEHIND: u64 = RING_SLOTS as u64;

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
        FALLBACK_SAMPLE_RATE
    };
    let period = Duration::from_secs_f64(num_samples as f64 / rate);
    (period * PROCESS_TIMEOUT_PERIODS).clamp(MIN_PROCESS_TIMEOUT, MAX_PROCESS_TIMEOUT)
}

/// Whether block `seq` is far enough behind the newest submitted block that its
/// reply is provably unwanted — its slab slot has been recycled.
///
/// `newest` is the highest sequence the audio thread has submitted. Saturating
/// rather than plain subtraction only to be total: `newest < seq` cannot happen
/// (the bridge cannot dequeue a block that was never pushed), and treating that
/// impossible case as "not stale" is the safe direction — it sends a block
/// rather than silently dropping a live one.
///
/// No wrap handling, unlike the `u32` `buffer_id` this replaced: a `u64`
/// sequence at ~750 blocks/s takes on the order of 780,000 years to exhaust.
fn is_stale(seq: u64, newest: u64) -> bool {
    newest.saturating_sub(seq) > MAX_BEHIND
}

pub(super) fn handle(
    cmd: Command,
    stream: &mut ControlStream,
    channels: &Channels,
    payloads: &PayloadPool,
) -> Result<()> {
    match cmd {
        Command::Process(mut payload) => {
            let seq = payload.seq;
            let num_samples = payload.num_samples;

            // Skip a block whose slab slot the host has already recycled: no
            // reply to it can be consumed (see `MAX_BEHIND`). Recycle the
            // payload so the pool doesn't leak, and push nothing — the audio
            // thread stopped expecting this block two blocks ago.
            if is_stale(seq, channels.newest_submitted()) {
                payloads.recycle(payload);
                return Ok(());
            }

            let msg = HostMessage::ProcessAudio(Box::new(ProcessAudioData {
                seq: payload.seq,
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
                BridgeMessage::AudioProcessed { seq, midi_out, .. } => {
                    // Convert IpcMidiEvent → MidiEvent HERE, on the bridge
                    // thread (off-RT). The RT thread only drains the built
                    // SmallVec — no per-event conversion, no heap traffic on
                    // the audio thread.
                    let midi_out = midi_out.iter().map(|e| MidiEvent::from(*e)).collect();
                    // The reply now carries MIDI only; whether the *audio* is
                    // there is settled by the slab's sequence numbers, which the
                    // server published before sending this. The echoed `seq` is
                    // kept for diagnostics and ordering, not as evidence.
                    channels.push_audio_response(AudioResponse::AudioProcessed { seq, midi_out });
                }
                BridgeMessage::Error { .. } => {
                    // Attributable to this request: the server answered it. The
                    // host needs no notification to fall back to silence — the
                    // server never published, so the sequence check fails — but
                    // the variant keeps the failure visible.
                    channels.push_audio_response(AudioResponse::Error { seq: Some(seq) });
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

    /// The bridge's patience must outlast the ring, or it would abandon blocks
    /// whose slots are still live — declaring a plugin unresponsive while its
    /// reply was still wanted.
    ///
    /// This replaces a test that compared the timeout against the audio thread's
    /// own wait budget. That comparison died with the budget: the audio thread
    /// no longer waits at all, so there is nothing to outlast on that side. The
    /// ring depth is what bounds usefulness now.
    #[test]
    fn timeout_keeps_the_bridge_within_the_ring_depth() {
        for rate in RATES {
            for n in BLOCKS {
                let t = process_timeout(n, rate);
                let ring_lifetime = period(n, rate) * MAX_BEHIND as u32;
                assert!(
                    t >= ring_lifetime.min(MAX_PROCESS_TIMEOUT),
                    "n={n} rate={rate}: timeout {t:?} gives up before the slot \
                     is recycled ({ring_lifetime:?})"
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

    /// The block just submitted is never stale — dropping it would turn every
    /// block into silence, the opposite of the fix.
    #[test]
    fn the_newest_block_is_never_stale() {
        assert!(!is_stale(10, 10));
    }

    /// Anything still inside the ring is sent: its slot has not been recycled,
    /// so its output can still be collected.
    #[test]
    fn blocks_still_inside_the_ring_are_sent() {
        for behind in 0..=MAX_BEHIND {
            assert!(
                !is_stale(10 - behind, 10),
                "{behind} block(s) behind is still within the ring"
            );
        }
    }

    /// Past the ring depth the slot has been overwritten, so no reply can land
    /// anywhere useful.
    #[test]
    fn blocks_past_the_ring_are_dropped() {
        assert!(is_stale(10 - MAX_BEHIND - 1, 10));
        assert!(is_stale(0, 10));
        assert!(is_stale(0, 1_000));
    }

    /// A backlog collapses to the live blocks in one pass rather than being
    /// ground through. This is the whole point: the old behaviour sent all ten,
    /// each a socket round-trip, while the audio thread kept adding more.
    #[test]
    fn a_backlog_collapses_to_the_live_blocks() {
        let newest = 10u64;
        let survivors: Vec<u64> = (0..=newest).filter(|&s| !is_stale(s, newest)).collect();
        assert_eq!(survivors, vec![8, 9, 10]);
    }

    /// `MAX_BEHIND` and the ring depth are one constraint, not two tunables.
    /// Asserted so a later edit cannot raise one without the other — which would
    /// either keep work whose slot was recycled or drop work still usable.
    #[test]
    fn max_behind_tracks_the_ring_depth() {
        assert_eq!(MAX_BEHIND, RING_SLOTS as u64);
    }

    /// `newest < seq` cannot occur — the bridge cannot dequeue a block that was
    /// never pushed — but the comparison must be total, and "send it" is the
    /// safe direction if it somehow did.
    #[test]
    fn an_impossible_future_block_is_not_dropped() {
        assert!(!is_stale(20, 10));
    }
}
