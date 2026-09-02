//! Per-command IPC choreography for the bridge thread.

use super::channels::Channels;
use super::messages::{AudioResponse, BridgeEvent, Command, ResyncKind};
use super::payload_pool::PayloadPool;
use super::progress::StateProgress;
use crate::error::{BridgeError, Result, StateError};
use crate::protocol::{BridgeMessage, HostMessage, IpcMidiEvent, MidiEvent, ProcessAudioData};
use crate::util::transport::control::{self as ipc, ControlStream};
use crate::util::transport::shm::RING_SLOTS;
use crate::util::transport::state_chunk::{self, ChunkError, Reassembler};
use std::time::Duration;

/// How many block periods the bridge thread will wait for a `ProcessAudio`
/// reply before abandoning the block.
///
/// Bounded by the *block period*, not an absolute duration. A bridge thread
/// blocked in `recv_reply` dequeues no further commands — `pump` is a single
/// loop — while the audio thread keeps pushing one command per block, so a single
/// slow reply overflows the 128-slot queue and produces sustained silence long
/// after the server recovers. A fixed 500 ms against a 667 µs block (64 frames
/// @ 48 kHz) is a ~750x mismatch, some 750 queued blocks.
///
/// A few periods rather than exactly one: the bridge may still be waiting after
/// the audio thread has moved on, and that reply is simply unmatched by the
/// slab's sequence check. Beyond a few periods it is worthless anyway.
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

/// How long [`Owed::settle`] waits to find out whether an owed reply has
/// *started* arriving.
///
/// Short on purpose. The drain is opportunistic — it collects frames already in
/// the socket buffer — and a frame that has not begun to land is left owed for a
/// later block rather than waited on, because blocking here is the queue
/// starvation the per-block timeout exists to prevent.
const DRAIN_PROBE: Duration = Duration::from_millis(1);

/// How long [`Owed::settle`] will wait to finish a reply that has *already
/// started* arriving.
///
/// A separate, far larger budget from [`DRAIN_PROBE`], and the split is the
/// point. A single small budget is not a smaller version of this — it is a
/// different outcome: expiring part-way through a frame leaves the stream
/// unframed (see [`BridgeError::Timeout::partial`]), and the only recovery from
/// that is the crash the drain exists to avoid. So the decision to *start*
/// reading is cheap and the commitment to *finish* is generous, because once
/// bytes are moving the only safe move is to consume the whole frame.
///
/// A few hundred bytes over a local socket cannot plausibly take this long
/// without the peer being dead, which the read reports as EOF rather than as a
/// timeout.
const DRAIN_FINISH: Duration = Duration::from_millis(500);

/// How many blocks behind the newest submitted block a queued `Process` may be
/// and still be worth sending.
///
/// **This is the ring depth, not an independent tunable**, but note the `- 1`:
/// a block exactly [`RING_SLOTS`] behind occupies the *same slot* as the newest
/// one, because `slot_for` is `seq % RING_SLOTS`. So the last still-live block
/// is `RING_SLOTS - 1` behind, not `RING_SLOTS`.
///
/// This was `RING_SLOTS`, and being one too lax was not merely wasteful. The
/// server writes and publishes its output *unconditionally* — its `has_input`
/// check gates only the read — so a block admitted here at `RING_SLOTS` behind
/// would write into the very slot the host is reading for the newest block and
/// then stamp that slot's `output_seq` with its own sequence, either tearing
/// the block being read or destroying the evidence for it. Deriving the
/// constant is right; deriving it with the wrong offset still shipped the bug
/// the derivation was meant to prevent.
///
/// It answers a *different* question from [`process_timeout`]: that asks "is this
/// plugin still responding?", this asks "is this reply still wanted?". Capping the
/// timeout low enough to force a catch-up would answer the second by breaking the
/// first, abandoning slow-but-working plugins on every block.
///
/// Cost of dropping: a stateful plugin (delay line, reverb tail) that skips input
/// blocks has its state diverge from a continuous signal, so it glitches on
/// recovery rather than cleanly silencing. Unavoidable without stalling the audio
/// thread, and only when the plugin is already failing to keep up.
const MAX_BEHIND: u64 = RING_SLOTS as u64 - 1;

/// The bridge thread's reply timeout for one block: [`PROCESS_TIMEOUT_PERIODS`]
/// of that block's own period, clamped to
/// [`MIN_PROCESS_TIMEOUT`]..=[`MAX_PROCESS_TIMEOUT`].
///
/// `num_samples`/`rate` stay raw: both come off the IPC wire, where the unit
/// mandate stops.
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

/// Replies the server still owes for blocks this thread stopped waiting on.
///
/// Owned by `pump` and passed in, rather than living in [`Channels`]: only the
/// bridge thread ever reads or writes it, so an `Arc<AtomicU32>` would advertise
/// a sharing that does not exist. See [`Owed::settle`] for what it is for.
#[derive(Default)]
pub(super) struct Owed(u32);

impl Owed {
    /// Note that a block's reply was abandoned before it arrived.
    fn note(&mut self) {
        self.0 = self.0.saturating_add(1);
    }

    /// Take one owed reply, in two stages: probe briefly to see whether a frame
    /// has begun arriving, and if it has, commit to finishing it.
    ///
    /// The stages exist because the two questions have different right answers.
    /// "Has anything arrived?" must be answered cheaply — waiting on it starves
    /// the command queue. "Will this frame finish?" must be answered patiently —
    /// giving up half-way leaves the stream unframed, and the only recovery is
    /// the crash this whole path exists to avoid.
    ///
    /// A `partial: true` timeout out of the probe is therefore not a failure to
    /// report but a fact to act on: bytes are moving, so it re-reads with
    /// [`DRAIN_FINISH`]. Only a probe that consumed *nothing* means "not here
    /// yet", and that is the one this returns as still-owed.
    fn take_one(stream: &mut ControlStream, channels: &Channels) -> Result<BridgeMessage> {
        match recv_reply(stream, channels, DRAIN_PROBE) {
            Err(BridgeError::Timeout { partial: true, .. }) => {
                recv_reply(stream, channels, DRAIN_FINISH)
            }
            other => other,
        }
    }

    /// Consume the replies owed from abandoned blocks, so the frame this
    /// command reads is its *own*.
    ///
    /// Abandoning a block on a timeout does not cancel anything: the server is
    /// mid-`process` and will write that frame onto this stream whenever it
    /// finishes. Leaving it there desynchronises the pairing — the next command
    /// would read the previous block's answer, and every later one would be a
    /// block behind, permanently. The frames themselves are still well-formed,
    /// so this is a matter of *pairing*, not of stream corruption.
    ///
    /// **Every owed frame that has already arrived is taken, not one per block.**
    /// Draining a single frame per call cannot catch up with a server that is
    /// missing its budget on every block: one frame is owed per block and one is
    /// cleared per block, so a backlog that forms never shrinks and the pairing
    /// distance grows with the run. Measured before this loop was made greedy: a
    /// reply arriving 15 blocks late over a 40-block run. Since the frames being
    /// taken here are already buffered, taking all of them costs no waiting.
    ///
    /// It waits on none that have not started. [`Owed::take_one`] probes with
    /// [`DRAIN_PROBE`] and returns at once when nothing is there, so a backlog
    /// never costs the bridge thread a full block budget on top of the one it
    /// already pays — which is the queue starvation the timeout exists to
    /// prevent. Whatever is still owed stays owed, and a later block takes it.
    ///
    /// A drained reply is **forwarded, not discarded**. Its audio is already
    /// moot — the slab slot it names has been recycled, and the sequence check
    /// rejects it — but its MIDI-out is the plugin's real output and the socket
    /// is the only path it has. Dropping it silently loses notes from any block
    /// whose reply ran late, which is a hard defect to attribute later: the
    /// audio is fine and a few events are simply missing.
    fn settle(&mut self, stream: &mut ControlStream, channels: &Channels) {
        while self.0 > 0 {
            self.0 -= 1;
            match Self::take_one(stream, channels) {
                Ok(BridgeMessage::AudioProcessed { seq, midi_out, .. }) => {
                    let midi_out = midi_out.iter().map(|e| MidiEvent::from(*e)).collect();
                    channels.push_audio_response(AudioResponse::AudioProcessed { seq, midi_out });
                }
                // An error the server attributed to a block already abandoned.
                // Nothing to forward: the caller fell back to silence for it
                // when the budget expired.
                Ok(_) => {}
                // Still not here, and nothing of it was read. Stop draining;
                // the stream is on a frame boundary and a later block may find
                // it.
                Err(BridgeError::Timeout { partial: false, .. }) => {
                    self.0 += 1;
                    return;
                }
                // Part of a frame was consumed, so the stream is no longer
                // framed. Stop owing anything — there is nothing to resume to —
                // and let the desync surface as the decode error it is on the
                // next read, which `pump` turns into a crash.
                Err(BridgeError::Timeout { partial: true, .. }) => {
                    self.0 = 0;
                    return;
                }
                // The peer is gone or the stream is unreadable. Say nothing here
                // — the send below will fail and `pump` will crash on it, with
                // the error that actually describes the failure.
                Err(_) => return,
            }
        }
    }
}

pub(super) fn handle(
    cmd: Command,
    stream: &mut ControlStream,
    channels: &Channels,
    payloads: &PayloadPool,
    owed: &mut Owed,
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

            let timeout = process_timeout(num_samples, channels.sample_rate());
            // Collect any reply an earlier block gave up on, so the frame read
            // below is this block's own rather than its predecessor's.
            owed.settle(stream, channels);

            ipc::send(stream, &msg)?;

            // The block's own budget, then the same two-stage rule the drain
            // uses: a budget that expired mid-frame means bytes are moving, and
            // the only safe move then is to finish the frame rather than leave
            // the stream unframed.
            let first = recv_reply(stream, channels, timeout);
            let first = match first {
                Err(BridgeError::Timeout { partial: true, .. }) => {
                    recv_reply(stream, channels, DRAIN_FINISH)
                }
                other => other,
            };
            let reply = match first {
                Ok(msg) => msg,
                // **A missed block budget costs the block, not the connection.**
                //
                // [`process_timeout`] is a few block periods with a 2 ms floor,
                // and it exists to stop the bridge thread blocking while the
                // audio thread keeps queueing — "abandoning the block", as its
                // doc says. But the error travelled out through `?` into `pump`,
                // which treats every `handle` error as connection-level: it
                // `crash()`es, drains the queue with errors and returns, ending
                // the thread. So one reply that lost a 2 ms race against the
                // scheduler latched a permanent crash, and from then on
                // `Batcher::collectable` short-circuits on `is_crashed()` and
                // every later block is silence — for a server still running,
                // still reading the socket, and still publishing correct audio.
                //
                // That is a wall-clock deadline gating correctness on a thread
                // with no scheduling priority, so on a loaded machine whether a
                // working plugin is declared dead is a property of the machine.
                // It is what made the pipeline suites fail intermittently under
                // parallel load and pass in isolation.
                //
                // Only `Timeout` is caught. Every other error — EOF, a frame
                // over `MAX_FRAME_BYTES`, a decode failure — means the peer is
                // gone or the stream is desynchronised, and those must still
                // reach `pump` as a crash.
                // A partial timeout has already been given `DRAIN_FINISH` to
                // complete above, so anything still timing out here left the
                // stream on a frame boundary and is safe to resume from.
                Err(BridgeError::Timeout { .. }) => {
                    // The reply is late, not absent: it will arrive on this
                    // stream, and reading the *next* command's reply would
                    // otherwise consume it and pair every later block with its
                    // predecessor's answer. Owing it here is what keeps the
                    // stream synchronised without waiting for it now.
                    owed.note();
                    // The host needs no notification to fall back to silence —
                    // the server has not published, so the slab's sequence check
                    // fails for this block on its own. The response keeps the
                    // abandonment visible and stops the queue running a block
                    // behind.
                    channels.push_audio_response(AudioResponse::Error { seq: Some(seq) });
                    return Ok(());
                }
                Err(e) => return Err(e),
            };
            match reply {
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
        Command::SetRenderMode { mode } => {
            ipc::send(stream, &HostMessage::SetRenderMode { mode })?;
        }
        Command::Reset => {
            ipc::send(stream, &HostMessage::Reset)?;
        }
        Command::Shutdown => {
            ipc::send(stream, &HostMessage::Shutdown)?;
        }
        Command::SaveState { progress, reply } => {
            ipc::send(stream, &HostMessage::SaveState)?;
            reply.send(recv_state(stream, channels, &progress)?);
        }
        Command::LoadState {
            data,
            progress,
            reply,
        } => {
            // Refuse over-limit state *here*, before a byte is written, and
            // answer the caller rather than propagating.
            //
            // Propagating would be wrong twice over: `?` returns before
            // `reply.send`, so the `Reply` drops and the caller waits out
            // the progress deadline to learn a length comparison the host made
            // instantly; and `pump` treats every `handle` error as
            // connection-level, so one oversized preset would `crash()` a
            // healthy session. Nothing was sent, so the stream is still
            // synchronised and there is nothing to crash about. Contrast
            // `recv_by`, where an over-cap length genuinely *has*
            // desynchronised the stream.
            if data.len() > crate::protocol::MAX_STATE_BYTES {
                reply.send(Err(StateError::TooLarge {
                    bytes: data.len(),
                    limit: crate::protocol::MAX_STATE_BYTES,
                }));
                return Ok(());
            }
            for (seq, last, bytes) in state_chunk::split(&data) {
                let n = bytes.len();
                ipc::send(
                    stream,
                    &HostMessage::LoadStateChunk {
                        seq,
                        last,
                        bytes: bytes.to_vec(),
                    },
                )?;
                // Report the write before waiting on the acknowledgement. A
                // socket that accepts chunks is a transfer that is moving, and
                // it is the only progress this direction has to show: the
                // server answers once, on `last`, so a caller watching for a
                // reply alone cannot distinguish a slow write from a stall.
                progress.advance(n);
            }
            // Wait for the answer, as `SaveState` above does. Answering the
            // caller straight after the write would report that the request had
            // been *sent*, never whether the plugin accepted it.
            //
            // A timeout here is **not** connection-level, for the same reason
            // the over-limit branch above is not: the socket is synchronised and
            // the session is healthy — the plugin simply has not answered yet.
            // Propagating would `crash()` it, and the caller would receive
            // `Stalled` (which says "the session is fine, retrying is
            // reasonable") against a session that had just been torn down.
            let value = match recv_state_reply(stream, channels, &progress)? {
                Some(BridgeMessage::StateLoaded { error }) => {
                    error.map(StateError::Rejected).map_or(Ok(()), Err)
                }
                Some(other) => Err(StateError::Rejected(format!(
                    "unexpected reply to LoadState: {other:?}"
                ))),
                None => Err(progress.stalled()),
            };
            reply.send(value);
        }
        Command::GetParameterList { reply } => {
            ipc::send(stream, &HostMessage::GetParameterList)?;
            let value = match recv_reply(stream, channels, PARAM_TIMEOUT)? {
                BridgeMessage::ParameterList { parameters } => Some(parameters),
                _ => None,
            };
            reply.send(value);
        }
        Command::GetPresetList { reply } => {
            ipc::send(stream, &HostMessage::GetPresetList)?;
            let value = match recv_reply(stream, channels, PARAM_TIMEOUT)? {
                BridgeMessage::PresetList { presets } => Some(presets),
                _ => None,
            };
            reply.send(value);
        }
        Command::LoadPreset { id, reply } => {
            ipc::send(stream, &HostMessage::LoadPreset { id })?;
            let ok = matches!(
                recv_reply(stream, channels, PARAM_TIMEOUT)?,
                BridgeMessage::PresetLoaded { ok: true }
            );
            reply.send(ok);
        }
        Command::GetCurrentPreset { reply } => {
            ipc::send(stream, &HostMessage::GetCurrentPreset)?;
            let value = match recv_reply(stream, channels, PARAM_TIMEOUT)? {
                BridgeMessage::CurrentPreset { id } => id,
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
        Command::GetParameterText {
            param_id,
            value,
            reply,
        } => {
            ipc::send(stream, &HostMessage::GetParameterText { param_id, value })?;
            let text = match recv_reply(stream, channels, PARAM_TIMEOUT)? {
                BridgeMessage::ParameterText { text } => text,
                _ => None,
            };
            reply.send(text);
        }
        Command::GetParameterValueFromText {
            param_id,
            text,
            reply,
        } => {
            ipc::send(
                stream,
                &HostMessage::GetParameterValueFromText { param_id, text },
            )?;
            let value = match recv_reply(stream, channels, PARAM_TIMEOUT)? {
                BridgeMessage::ParameterValueFromText { value } => value,
                _ => None,
            };
            reply.send(value);
        }
    }
    Ok(())
}

/// Collect a chunked `StateChunk` sequence into one state.
///
/// Returns the caller's `Result` rather than propagating, because none of the
/// ways this fails is a connection failure: an over-limit state and a
/// misordered sequence are both *this plugin's* problem, and crashing the
/// bridge for either would take down a session whose socket is fine. Only a
/// genuine transport error (`?`) still propagates, and that is the one case
/// where the stream really is unusable.
///
/// Each chunk gets the full progress deadline, not a share of it: the budget is
/// per reply, and a plugin serialising a large state can legitimately pause
/// between chunks. The overall wait is still bounded, because a sequence that
/// stops arriving fails on the next chunk's own deadline.
///
/// The deadline comes from `progress` rather than a constant, so it is the same
/// figure the waiting caller uses — one configuration, and a test that shortens
/// it shortens both halves of the production path.
///
/// `progress` is also advanced per chunk so the *caller's* wait is bounded the
/// same way. Both are needed and neither subsumes the other: this one bounds
/// the bridge thread's read, that one bounds the main thread's block on the
/// reply, and a fix to only one leaves the other as the effective ceiling.
fn recv_state(
    stream: &mut ControlStream,
    channels: &Channels,
    progress: &StateProgress,
) -> Result<std::result::Result<Vec<u8>, StateError>> {
    let mut acc = Reassembler::default();
    loop {
        let Some(msg) = recv_state_reply(stream, channels, progress)? else {
            // Stalled, not disconnected. Answering the caller keeps the session
            // alive; see `recv_state_reply`.
            return Ok(Err(progress.stalled()));
        };
        match msg {
            BridgeMessage::StateChunk { seq, last, bytes } => {
                progress.advance(bytes.len());
                match acc.push(seq, last, &bytes) {
                    Ok(true) => return Ok(Ok(acc.into_inner())),
                    Ok(false) => {}
                    Err(ChunkError::TooLarge { bytes, limit }) => {
                        return Ok(Err(StateError::TooLarge { bytes, limit }))
                    }
                    Err(ChunkError::OutOfOrder { got, expected }) => {
                        return Ok(Err(StateError::Rejected(format!(
                            "plugin sent state chunk {got} where {expected} was expected"
                        ))))
                    }
                }
            }
            // A reply of the wrong type mid-sequence is the server answering
            // something else entirely; the state is not coming.
            other => {
                return Ok(Err(StateError::Rejected(format!(
                    "unexpected reply while reading plugin state: {other:?}"
                ))))
            }
        }
    }
}

/// [`recv_reply`] on the transfer's progress deadline, with a timeout reported
/// as `Ok(None)` rather than as an error.
///
/// **This is the difference between a stalled transfer and a dead session.**
/// `pump` treats every `Err` out of `handle` as connection-level: it calls
/// `crash()`, latches a cause, fails every queued block and returns. That is
/// right for a broken socket and wrong for a slow plugin — the stream is
/// synchronised, nothing is malformed, and the peer has simply not spoken yet.
///
/// Reporting a stall as fatal was self-contradictory in a way a caller could
/// not defend against: [`StateError::Stalled`] tells the caller the session is
/// healthy and a retry is reasonable, while the crash it rode in on had already
/// destroyed the session it would retry against.
///
/// The precedent is the over-limit `LoadState` branch in [`handle`], which
/// answers its caller and returns `Ok(())` for exactly this reason. This does
/// the same for the timeout: `None` means "no reply within the deadline", the
/// caller is told, and the bridge thread goes back to the queue.
///
/// Only a *genuine* transport error still propagates — a closed socket, a
/// malformed frame — which is the one case where the stream really is unusable.
fn recv_state_reply(
    stream: &mut ControlStream,
    channels: &Channels,
    progress: &StateProgress,
) -> Result<Option<BridgeMessage>> {
    match recv_reply(stream, channels, progress.deadline()) {
        Ok(msg) => Ok(Some(msg)),
        // Matched on the variant, not on a string or an `io::ErrorKind`:
        // `recv_within` already normalises the socket's `TimedOut`/`WouldBlock`
        // into this, so this is the only shape a deadline expiry takes, and
        // nothing else produces it.
        Err(BridgeError::Timeout { .. }) => Ok(None),
        Err(e) => Err(e),
    }
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
            BridgeMessage::TailChanged { tail } => {
                channels.push_unsolicited(BridgeEvent::TailChanged { tail });
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

    /// A fixed 500 ms timeout sits ~750x above the audio thread's 667 us budget
    /// at 64 frames / 48 kHz. While the bridge thread blocks in `recv_reply` it
    /// dequeues nothing, so the audio thread's one-command-per-block fills the
    /// 128-slot queue and one slow reply becomes a sustained run of silence.
    /// The timeout is therefore bound to a few block periods.
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
    /// Bounded against the ring depth rather than the audio thread's wait
    /// budget: the audio thread never waits, so there is nothing to outlast on
    /// that side.
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
            assert!(
                t >= MIN_PROCESS_TIMEOUT && t <= MAX_PROCESS_TIMEOUT,
                "rate={bad}: {t:?}"
            );
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
    /// ground through. Sending all ten, each a socket round-trip, loses ground
    /// while the audio thread keeps adding more.
    #[test]
    fn a_backlog_collapses_to_the_live_blocks() {
        let newest = 10u64;
        let survivors: Vec<u64> = (0..=newest).filter(|&s| !is_stale(s, newest)).collect();
        assert_eq!(survivors, vec![9, 10]);
    }

    /// **The invariant, stated as a property rather than as a number.**
    ///
    /// No block this admits may share a ring slot with the newest one. If it
    /// did, the server would write that block's output into the slot the host
    /// is reading for `newest` and stamp the slot's sequence with its own —
    /// tearing the block being read, or destroying the evidence for it.
    ///
    /// The previous version of this test asserted `MAX_BEHIND == RING_SLOTS`
    /// and its sibling asserted `survivors == [8, 9, 10]`, which *pinned the
    /// off-by-one as intended behaviour*: at depth 2, `slot_for(8) ==
    /// slot_for(10)`. Asserting the numbers made the tests agree with the bug.
    /// Asserting the property makes them independent of the ring depth, so
    /// raising `RING_SLOTS` cannot silently reintroduce it.
    #[test]
    fn no_admitted_block_shares_a_slot_with_the_newest() {
        // Mirrors `shm::header::slot_for`, which is private to that module.
        // Duplicated deliberately: importing it would couple this test to the
        // slab's internals, and the mapping (`seq % RING_SLOTS`) is the *wire*
        // contract both sides implement, not an implementation detail.
        let slot_of = |seq: u64| seq % RING_SLOTS as u64;

        let newest = 64u64;
        for seq in 0..=newest {
            if is_stale(seq, newest) {
                continue;
            }
            assert!(
                seq == newest || slot_of(seq) != slot_of(newest),
                "block {seq} is {} behind and admitted, but shares slot {} with \
                 the newest block {newest} — the server would overwrite the slot \
                 the host is reading",
                newest - seq,
                slot_of(seq),
            );
        }
    }

    /// `newest < seq` cannot occur — the bridge cannot dequeue a block that was
    /// never pushed — but the comparison must be total, and "send it" is the
    /// safe direction if it somehow did.
    #[test]
    fn an_impossible_future_block_is_not_dropped() {
        assert!(!is_stale(20, 10));
    }
}
