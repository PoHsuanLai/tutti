//! Per-command IPC choreography for the bridge thread.

use super::channels::Channels;
use super::lifecycle::Lifecycle;
use super::messages::{AudioResponse, Command, Unsolicited};
use super::payload_pool::PayloadPool;
use super::progress::StateProgress;
use crate::protocol::{
    BridgeMessage, HostMessage, ParamId, ParameterChanges, ParameterPoint, ParameterQueue,
    ProcessAudioData,
};
use crate::transport::control::{self as ipc, ControlStream, PartialFrame};
use crate::transport::shm::RING_SLOTS;
use crate::transport::state_chunk::{self, ChunkError, Reassembler};
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tutti_plugin_types::MidiIntake;
use tutti_plugin_types::{BridgeError, BridgeResult, ResyncKind, StateError};
use tutti_types::SampleRate;

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

/// How long the bridge waits for a plugin to re-activate at a new rate.
/// Longer than [`PARAM_TIMEOUT`]: it tears down and rebuilds the plugin's
/// processing, which a large instrument can take seconds to do.
pub(super) const RATE_TIMEOUT: Duration = Duration::from_secs(10);

/// The slice [`RATE_TIMEOUT`] is waited in, between checks that the bridge is
/// still running.
const RATE_SLICE: Duration = Duration::from_millis(100);

/// How long [`next_reply`] will wait to finish a reply that has *already
/// started* arriving when its own budget runs out.
///
/// Larger than any request's budget, and the split is the point: the decision
/// to *wait* for a reply is the request's, and is short for a block, but once
/// bytes of a frame are moving the reply is arriving and finishing it costs
/// little. (The cursor would resume a half-read frame correctly anyway; this
/// only lets a reply that is already landing count for its own request.)
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
fn process_timeout(num_samples: usize, rate: SampleRate) -> Duration {
    let rate = rate.get();
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

/// Replies the server still owes for requests this thread stopped waiting on,
/// plus the cursor of any frame a timeout left half-read.
///
/// **The server answers strictly in order**, one request at a time, so every
/// frame owed for an earlier request arrives before the answer to the next
/// one. Taking exactly the owed frames off the stream first ([`Owed::take`],
/// which every read goes through, [`next_reply`]) therefore keeps each answer
/// paired with its own request, whatever kind of request gave up. Leaving them
/// there is what paired a block with its predecessor's reply, and a query with
/// a stale `AudioProcessed` (doc 027 D5).
///
/// Owned by `pump` and passed in, rather than living in [`Channels`]: only the
/// bridge thread ever reads or writes it, so an `Arc<AtomicU32>` would advertise
/// a sharing that does not exist. The [`PartialFrame`] must have exactly this
/// scope for a different reason — it describes a position in *this* connection's
/// stream, so it lives and dies with the stream.
#[derive(Default)]
pub(super) struct Owed {
    /// What is still owed, oldest first: the order the frames arrive in.
    late: VecDeque<Late>,
    /// Where the last read stopped, so a timeout resumes rather than restarts.
    partial: PartialFrame,
    /// The parameter changes of blocks skipped as stale, as each parameter's
    /// last value, on frame 0: sent on the first frame of the next block that
    /// goes. A plain list, cleared (its capacity kept) once merged.
    carried: Vec<(ParamId, ParameterPoint)>,
    /// The MIDI of the blocks that go, filtered: once a stale block is
    /// skipped, the rest of every SysEx open in it is not sent, so the
    /// server's reassembler never completes one with a gap.
    intake: MidiIntake,
}

impl Owed {
    /// Keep a skipped block's parameter changes: each parameter's last value.
    ///
    /// The plugin node sends a parameter only when its value changes, so a
    /// change dropped with its block would never be sent again: the plugin
    /// would hold the value before it until the host's value moved once more.
    /// Bridge thread: may allocate.
    fn carry(&mut self, changes: &ParameterChanges) {
        for queue in &changes.queues {
            let Some(last) = queue.points.last() else {
                continue;
            };
            let point = ParameterPoint {
                sample_offset: 0,
                value: last.value,
            };
            match self.carried.iter_mut().find(|(a, _)| *a == queue.param_id) {
                Some((_, kept)) => *kept = point,
                None => self.carried.push((queue.param_id, point)),
            }
        }
    }

    /// Put what [`carry`](Self::carry) kept into the block about to be sent,
    /// on its first frame: a parameter the block moves itself keeps its own
    /// points, preceded by the carried value when its first point is later
    /// than frame 0.
    ///
    /// Never grows the block's buffer: it is built
    /// `ParameterChanges::reserved_bridged`, which keeps a point of every
    /// queue (and of every pool block) for the carried value, so the value
    /// takes a real slot (`ParameterChanges::prepend_carried`, counted in the
    /// buffer's `points_overflowed` when it cannot: a buffer built some other
    /// way, full, or one with every queue open). A parameter the block does
    /// not move opens one of the buffer's spare queues. Only the empty
    /// placeholder of a block that had no changes (no queue room at all)
    /// gets queues built here (bridge thread: may allocate; it is dropped on
    /// return).
    fn merge_carried(&mut self, changes: &mut ParameterChanges) {
        let placeholder = changes.capacity() == 0;
        for &(param_id, carried) in &self.carried {
            let i = match changes.queues.iter().position(|q| q.param_id == param_id) {
                Some(i)
                    if changes.queues[i]
                        .points
                        .first()
                        .is_some_and(|p| p.sample_offset > 0) =>
                {
                    i
                }
                Some(_) => continue,
                None if placeholder => {
                    changes
                        .queues
                        .push(ParameterQueue::from_points(param_id, vec![carried]));
                    continue;
                }
                None if changes.open_queue(param_id) => changes.queues.len() - 1,
                // No spare queue: past the queues, so dropped and counted.
                // Here the carried value is the parameter's latest, and it
                // is lost. Unreachable from the delivery: every buffer holds
                // its slot count (the parameters + `SPARE_QUEUES`), and each
                // id a block holds or the bridge carries is one of those
                // slots. No `debug_assert`: a buffer built smaller reaches
                // it, and the test below builds one.
                None => changes.queues.len(),
            };
            let _ = changes.prepend_carried(i, carried);
        }
        self.carried.clear();
    }

    /// Note that a request's reply was abandoned before it arrived.
    fn owe(&mut self, late: Late) {
        self.late.push_back(late);
    }

    /// Whether `msg` is a frame owed for an earlier request, taking it if so:
    /// `None` when it was owed (and has been dealt with), `Some(msg)` when it
    /// is the answer the caller is waiting for.
    ///
    /// Each owed item knows what answers it (and the server's `Error` answers
    /// anything). A frame of another kind means that answer is not coming
    /// (a peer that dropped it): the item is let go and the frame is weighed
    /// against the next one, so a lost answer cannot swallow every reply
    /// after it.
    ///
    /// A block's owed reply is **forwarded, not discarded**. Its audio is
    /// moot (the slab slot it names has been recycled, and the sequence check
    /// rejects it) but its MIDI-out is the plugin's real output and the socket
    /// is its only path. Dropping it silently loses notes from any block whose
    /// reply ran late. Only a block's counts as settled
    /// ([`Channels::note_settled`]): the counters are about blocks.
    fn take(&mut self, msg: BridgeMessage, channels: &Channels) -> Option<BridgeMessage> {
        while let Some(&late) = self.late.front() {
            let error = matches!(msg, BridgeMessage::Error { .. });
            match late {
                Late::Block => {
                    if let BridgeMessage::AudioProcessed { seq, midi_out, .. } = msg {
                        channels
                            .push_audio_response(AudioResponse::AudioProcessed { seq, midi_out });
                        channels.note_settled(1);
                        self.late.pop_front();
                        return None;
                    }
                    if error {
                        // The block fell back to silence when its budget
                        // expired; nothing to forward.
                        channels.note_settled(1);
                        self.late.pop_front();
                        return None;
                    }
                }
                Late::Answer(answers) if error || answers(&msg) => {
                    self.late.pop_front();
                    return None;
                }
                Late::Answer(_) => {}
                // A state keeps arriving until its `last` chunk; an `Error`
                // instead of the chunks ends it.
                Late::StateRest => match msg {
                    BridgeMessage::StateChunk { last: false, .. } => return None,
                    BridgeMessage::StateChunk { last: true, .. } | BridgeMessage::Error { .. } => {
                        self.late.pop_front();
                        return None;
                    }
                    _ => {}
                },
            }
            self.late.pop_front();
        }
        Some(msg)
    }
}

/// What a request the bridge stopped waiting for still has coming.
#[derive(Clone, Copy, Debug)]
pub(super) enum Late {
    /// A block's `AudioProcessed` (or `Error`): its MIDI is forwarded.
    Block,
    /// A control request's one reply, of the kind this accepts (or `Error`).
    Answer(fn(&BridgeMessage) -> bool),
    /// The rest of a `SaveState` answer: chunks up to `last`, or one `Error`.
    StateRest,
}

/// The next reply to the request just sent, within `budget`, after the frames
/// owed for earlier requests: `Ok(None)` when it did not come in time.
///
/// **One deadline for the whole read.** The owed frames and the unsolicited
/// events read on the way are inside it (doc 027 D7); a chatty plugin or a
/// backlog does not extend it.
///
/// A timeout on a frame boundary is not an error: the stream is synchronised
/// and the session healthy, so the caller answers its own caller and the
/// bridge goes on. A budget that expires **part-way through a frame** gets
/// [`DRAIN_FINISH`] to complete *that frame*, and the read ends there (with
/// the frame, when it is the answer): extending the whole read instead let a
/// chatty peer start a new frame at the extended deadline, and a live peer was
/// declared unable to frame. A frame still unfinished after `DRAIN_FINISH` is
/// a peer that cannot frame (a few hundred bytes over a local socket do not
/// take that long from a live one), and that timeout propagates with the
/// transport's errors (EOF, a frame over the cap), which `pump` crashes on.
fn next_reply(
    stream: &mut ControlStream,
    inbox: &mut Unsolicited,
    owed: &mut Owed,
    channels: &Channels,
    budget: Duration,
) -> BridgeResult<Option<BridgeMessage>> {
    let deadline = Instant::now() + budget;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match recv_reply(stream, inbox, left, &mut owed.partial) {
            Ok(msg) => {
                if let Some(msg) = owed.take(msg, channels) {
                    return Ok(Some(msg));
                }
            }
            Err(BridgeError::Timeout { partial: true, .. }) => {
                let frame = ipc::recv_resumable(stream, DRAIN_FINISH, &mut owed.partial)?;
                let answer = absorb(inbox, frame).and_then(|msg| owed.take(msg, channels));
                return Ok(answer);
            }
            Err(BridgeError::Timeout { partial: false, .. }) => return Ok(None),
            Err(e) => return Err(e),
        }
    }
}

/// [`next_reply`], owing the reply as `late` when it does not come in time,
/// so it is taken off the stream when it does (doc 027 D5, D6).
fn answer(
    stream: &mut ControlStream,
    inbox: &mut Unsolicited,
    owed: &mut Owed,
    channels: &Channels,
    budget: Duration,
    late: Late,
) -> BridgeResult<Option<BridgeMessage>> {
    let reply = next_reply(stream, inbox, owed, channels, budget)?;
    if reply.is_none() {
        owed.owe(late);
    }
    Ok(reply)
}

pub(super) fn handle(
    cmd: Command,
    stream: &mut ControlStream,
    channels: &Channels,
    payloads: &PayloadPool,
    owed: &mut Owed,
    inbox: &mut Unsolicited,
    lifecycle: &Lifecycle,
) -> BridgeResult<()> {
    match cmd {
        Command::Process(mut payload) => {
            let seq = payload.seq;
            let num_samples = payload.num_samples;

            // Skip a block whose slab slot the host has already recycled: no
            // reply to it can be consumed (see `MAX_BEHIND`). Recycle the
            // payload so the pool doesn't leak, and push nothing — the audio
            // thread stopped expecting this block two blocks ago.
            if is_stale(seq, channels.newest_submitted()) {
                // Its MIDI is lost, counted, and so is the rest of every
                // SysEx open in it: the server's reassembler would otherwise
                // complete it from the next block's packets, with these
                // missing. (No wire change: a gap is never sent.)
                for e in &payload.midi_events {
                    owed.intake.admit(e);
                }
                owed.intake.lost();
                channels.note_dropped_midi(payload.midi_events.len() as u64);
                owed.carry(&payload.param_changes);
                payloads.return_params(core::mem::take(&mut payload.param_changes));
                payloads.recycle(payload);
                return Ok(());
            }

            if !owed.carried.is_empty() {
                owed.merge_carried(&mut payload.param_changes);
            }
            let before = payload.midi_events.len();
            payload.midi_events.retain(|e| owed.intake.admit(e));
            channels.note_dropped_midi((before - payload.midi_events.len()) as u64);
            let msg = HostMessage::ProcessAudio(ProcessAudioData {
                seq: payload.seq,
                num_samples: payload.num_samples,
                midi_events: payload.midi_events.clone(),
                param_changes: core::mem::take(&mut payload.param_changes),
                note_expression: core::mem::take(&mut payload.note_expression),
                chords: core::mem::take(&mut payload.chords),
                scales: core::mem::take(&mut payload.scales),
                expr_texts: core::mem::take(&mut payload.expr_texts),
                expr_ints: core::mem::take(&mut payload.expr_ints),
                transport: core::mem::take(&mut payload.transport),
                transport_changes: core::mem::take(&mut payload.transport_changes),
            });

            payloads.recycle(payload);

            let timeout = process_timeout(num_samples, channels.sample_rate());
            let sent = ipc::send(stream, &msg);
            // The parameter-change buffer goes back to the audio thread's free
            // list once its contents are on the wire (sent or not: a failed
            // send crashes the bridge below, and the buffer is no use to it).
            if let HostMessage::ProcessAudio(data) = msg {
                payloads.return_params(data.param_changes);
            }
            sent?;

            // Replies owed from earlier blocks come first and are forwarded
            // (`Owed::take`); then this block's own, within its budget.
            match answer(stream, inbox, owed, channels, timeout, Late::Block)? {
                // **A missed block budget costs the block, not the connection.**
                //
                // [`process_timeout`] is a few block periods with a 2 ms floor,
                // and it exists to stop the bridge thread blocking while the
                // audio thread keeps queueing. When the timeout was an error
                // out of `handle`, `pump` treated it as connection-level and
                // `crash()`ed: one reply that lost a 2 ms race against the
                // scheduler latched a permanent crash for a server still
                // running and publishing correct audio. Whether a working
                // plugin was declared dead was a property of the machine.
                //
                // The reply is late, not absent: `answer` owes it, so it is
                // taken off the stream before a later request reads its own.
                // The host needs no notification to fall back to silence (the
                // server has not published, so the slab's sequence check fails
                // for this block on its own); the response keeps the
                // abandonment visible and stops the queue running a block
                // behind.
                None => {
                    channels.note_abandoned();
                    channels.push_audio_response(AudioResponse::Error { seq: Some(seq) });
                }
                Some(reply) => {
                    channels.note_replied();
                    match reply {
                        // The wire adapter already decoded the list on the
                        // bridge thread (off-RT) and refused one past the cap;
                        // the RT thread only drains it. Whether the *audio* is
                        // there is settled by the slab's sequence numbers,
                        // which the server published before sending this; the
                        // echoed `seq` is for diagnostics and ordering.
                        BridgeMessage::AudioProcessed { seq, midi_out, .. } => {
                            channels.push_audio_response(AudioResponse::AudioProcessed {
                                seq,
                                midi_out,
                            });
                        }
                        // Attributable to this request: the server answered it
                        // and never published, so the block is silence; the
                        // variant keeps the failure visible.
                        BridgeMessage::Error { .. } => {
                            channels.push_audio_response(AudioResponse::Error { seq: Some(seq) });
                        }
                        _ => {}
                    }
                }
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
            // Wait for the answer: the plugin's re-activation is the slowest
            // thing the bridge asks of it, and what it says decides whether
            // the next block may be sent.
            //
            // Replies owed from abandoned requests can still be ahead of the
            // answer on the stream; `next_reply` takes them first.
            //
            // Waited in slices, so a shutdown (dropping the node joins this
            // thread) ends the wait within one slice rather than after
            // `RATE_TIMEOUT`. A slice that expires part-way through a frame
            // resumes it (`owed.partial`), so slicing never unframes the stream.
            let budget = lifecycle.rate_timeout();
            let deadline = std::time::Instant::now() + budget;
            loop {
                let reply = match next_reply(stream, inbox, owed, channels, RATE_SLICE)? {
                    None => {
                        if !lifecycle.is_running() {
                            return Ok(());
                        }
                        if std::time::Instant::now() >= deadline {
                            // No answer is a refusal, not a dead connection: the
                            // plugin may still be re-activating. Its late reply
                            // is taken off the stream when it arrives
                            // (`recv_reply`) and settles the state.
                            inbox.expect_late_rate_reply();
                            apply_rate_reply(
                                lifecycle,
                                inbox,
                                rate,
                                Some(format!(
                                    "the plugin did not answer the rate change within {budget:?}"
                                )),
                            );
                            break;
                        }
                        continue;
                    }
                    Some(reply) => reply,
                };
                match reply {
                    // A refusal is not a failure of the connection: the plugin
                    // stays loaded, the server renders silence for it, and the
                    // next rate change retries it.
                    BridgeMessage::SampleRateSet { rate, error } => {
                        apply_rate_reply(lifecycle, inbox, rate, error);
                        break;
                    }
                    other => {
                        return Err(BridgeError::UnexpectedMessage {
                            expected: "SampleRateSet",
                            got: format!("{other:?}"),
                        });
                    }
                }
            }
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
            reply.send(recv_state(stream, inbox, owed, channels, &progress)?);
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
            // The answer is owed, and taken off the stream when it comes.
            let budget = progress.deadline();
            let value = match answer(
                stream,
                inbox,
                owed,
                channels,
                budget,
                Late::Answer(|m| matches!(m, BridgeMessage::StateLoaded { .. })),
            )? {
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
            let value = match ask(stream, inbox, owed, channels, |m| {
                matches!(m, BridgeMessage::ParameterList { .. })
            })? {
                Some(BridgeMessage::ParameterList { parameters }) => Some(parameters),
                _ => None,
            };
            reply.send(value);
        }
        Command::GetPresetList { reply } => {
            ipc::send(stream, &HostMessage::GetPresetList)?;
            let value = match ask(stream, inbox, owed, channels, |m| {
                matches!(m, BridgeMessage::PresetList { .. })
            })? {
                Some(BridgeMessage::PresetList { presets }) => Some(presets),
                _ => None,
            };
            reply.send(value);
        }
        Command::LoadPreset { id, reply } => {
            ipc::send(stream, &HostMessage::LoadPreset { id })?;
            let ok = matches!(
                ask(stream, inbox, owed, channels, |m| matches!(
                    m,
                    BridgeMessage::PresetLoaded { .. }
                ))?,
                Some(BridgeMessage::PresetLoaded { ok: true })
            );
            reply.send(ok);
        }
        Command::GetCurrentPreset { reply } => {
            ipc::send(stream, &HostMessage::GetCurrentPreset)?;
            let value = match ask(stream, inbox, owed, channels, |m| {
                matches!(m, BridgeMessage::CurrentPreset { .. })
            })? {
                Some(BridgeMessage::CurrentPreset { id }) => id,
                _ => None,
            };
            reply.send(value);
        }
        Command::GetParameter { param_id, reply } => {
            ipc::send(stream, &HostMessage::GetParameter { param_id })?;
            let value = match ask(stream, inbox, owed, channels, |m| {
                matches!(m, BridgeMessage::ParameterValue { .. })
            })? {
                Some(BridgeMessage::ParameterValue { value }) => value,
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
            let text = match ask(stream, inbox, owed, channels, |m| {
                matches!(m, BridgeMessage::ParameterText { .. })
            })? {
                Some(BridgeMessage::ParameterText { text }) => text,
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
            let value = match ask(stream, inbox, owed, channels, |m| {
                matches!(m, BridgeMessage::ParameterValueFromText { .. })
            })? {
                Some(BridgeMessage::ParameterValueFromText { value }) => value,
                _ => None,
            };
            reply.send(value);
        }
    }
    Ok(())
}

/// A query's reply, within [`PARAM_TIMEOUT`]: `None` when it does not come
/// in time, which the query answers as "cannot be asked" (and the reply is
/// owed). A timeout here was a `?` that crashed a healthy bridge (doc 027 D6).
fn ask(
    stream: &mut ControlStream,
    inbox: &mut Unsolicited,
    owed: &mut Owed,
    channels: &Channels,
    answers: fn(&BridgeMessage) -> bool,
) -> BridgeResult<Option<BridgeMessage>> {
    answer(
        stream,
        inbox,
        owed,
        channels,
        PARAM_TIMEOUT,
        Late::Answer(answers),
    )
}

/// Collect a chunked `StateChunk` sequence into one state.
///
/// Returns the caller's `BridgeResult` rather than propagating, because none of the
/// ways this fails is a connection failure: an over-limit state, a misordered
/// sequence and a stalled one are all *this plugin's* problem, and crashing the
/// bridge for any of them would take down a session whose socket is fine
/// ([`StateError::Stalled`] promises the caller a healthy session to retry
/// against). Only a genuine transport error (`?`) still propagates, and that
/// is the one case where the stream really is unusable.
///
/// **Whenever this stops before the answer has ended, the rest is owed**
/// ([`Late::StateRest`]): the server sends the whole sequence whatever this end
/// made of it, and a later request must not read its chunks as its answer.
///
/// Each chunk gets the full progress deadline, not a share of it: the budget is
/// per reply, and a plugin serialising a large state can legitimately pause
/// between chunks. The overall wait is still bounded, because a sequence that
/// stops arriving fails on the next chunk's own deadline. The deadline comes
/// from `progress`, so it is the same figure the waiting caller uses, and
/// `progress` is advanced per chunk so the *caller's* wait is bounded the same
/// way: this bounds the bridge thread's read, that the main thread's block on
/// the reply, and a fix to only one leaves the other as the ceiling.
fn recv_state(
    stream: &mut ControlStream,
    inbox: &mut Unsolicited,
    owed: &mut Owed,
    channels: &Channels,
    progress: &StateProgress,
) -> BridgeResult<std::result::Result<Vec<u8>, StateError>> {
    let mut acc = Reassembler::default();
    loop {
        let Some(msg) = next_reply(stream, inbox, owed, channels, progress.deadline())? else {
            owed.owe(Late::StateRest);
            return Ok(Err(progress.stalled()));
        };
        let BridgeMessage::StateChunk { seq, last, bytes } = msg else {
            // A reply of another type is the whole answer (the server's
            // `Error` when the plugin cannot save); the state is not coming.
            return Ok(Err(StateError::Rejected(format!(
                "unexpected reply while reading plugin state: {msg:?}"
            ))));
        };
        progress.advance(bytes.len());
        let refused = match acc.push(seq, last, &bytes) {
            Ok(true) => return Ok(Ok(acc.into_inner())),
            Ok(false) => continue,
            Err(ChunkError::TooLarge { bytes, limit }) => StateError::TooLarge { bytes, limit },
            Err(ChunkError::OutOfOrder { got, expected }) => StateError::Rejected(format!(
                "plugin sent state chunk {got} where {expected} was expected"
            )),
            Err(ChunkError::Empty { seq }) => StateError::Rejected(format!(
                "plugin sent an empty state chunk {seq} before the last"
            )),
        };
        if !last {
            owed.owe(Late::StateRest);
        }
        return Ok(Err(refused));
    }
}

/// Record the plugin's answer to a rate change: the refusal while it refuses,
/// cleared (and heard, if there was one) when it takes the rate.
pub(super) fn apply_rate_reply(
    lifecycle: &Lifecycle,
    inbox: &mut Unsolicited,
    rate: SampleRate,
    error: Option<String>,
) {
    match error {
        Some(reason) => {
            lifecycle.set_rate_refusal(Some((rate, reason.clone())));
            inbox.rate_refusal(Some((rate, reason)));
        }
        None => {
            if lifecycle.rate_refusal().is_some() {
                lifecycle.set_rate_refusal(None);
                inbox.rate_refusal(None);
            }
        }
    }
}

/// Drain unsolicited events into `inbox`, returning the next reply-type message
/// within `timeout`.
///
/// The timeout is one deadline for the whole call: each read gets what is left
/// of it. Restarting it per read let a plugin that keeps sending events (a GUI
/// drag, a meter) hold the bridge past every budget (doc 027 D7).
fn recv_reply(
    stream: &mut ControlStream,
    inbox: &mut Unsolicited,
    timeout: Duration,
    partial: &mut PartialFrame,
) -> BridgeResult<BridgeMessage> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if let Some(reply) = absorb(inbox, ipc::recv_resumable(stream, left, partial)?) {
            return Ok(reply);
        }
    }
}

/// Put an unsolicited event into `inbox`; anything else is a reply, returned.
fn absorb(inbox: &mut Unsolicited, msg: BridgeMessage) -> Option<BridgeMessage> {
    match msg {
        BridgeMessage::LatencyChanged { samples } => inbox.latency(samples),
        BridgeMessage::TailChanged { tail } => inbox.tail(tail),
        BridgeMessage::ParameterChanged { index, value } => inbox.param(index, value),
        BridgeMessage::PluginParamValuesChanged => inbox.resync(ResyncKind::ParamValues),
        BridgeMessage::PluginParamTitlesChanged => inbox.resync(ResyncKind::ParamTitles),
        BridgeMessage::PluginIoChanged => inbox.resync(ResyncKind::Io),
        BridgeMessage::PluginReloaded => inbox.resync(ResyncKind::Reloaded),
        // The answer to a rate change the bridge stopped waiting for: it
        // belongs to no request now, so it is held for the thread to apply,
        // and never read as another request's reply.
        BridgeMessage::SampleRateSet { rate, error } => {
            if !inbox.late_rate_reply(rate, error.clone()) {
                return Some(BridgeMessage::SampleRateSet { rate, error });
            }
        }
        msg => return Some(msg),
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ParamId;

    fn processed(seq: u64) -> BridgeMessage {
        BridgeMessage::AudioProcessed {
            latency_us: 0,
            seq,
            midi_out: Default::default(),
        }
    }

    fn chunk(seq: u32, last: bool) -> BridgeMessage {
        BridgeMessage::StateChunk {
            seq,
            last,
            bytes: vec![1; 4],
        }
    }

    fn is_value(m: &BridgeMessage) -> bool {
        matches!(m, BridgeMessage::ParameterValue { .. })
    }

    /// **What is owed is taken first, in order, and the next request's answer
    /// is its own** (doc 027 D5). An abandoned block's reply is forwarded and
    /// counted as settled; a query's late reply and the rest of a stalled
    /// state are taken without answering anything; the frame after them is
    /// the answer.
    ///
    /// Mutations (run), each fails: `take` returning `Some(msg)` at once (the
    /// pre-fix pairing: every request read the oldest owed frame as its
    /// answer); `StateRest` ending on any chunk (the second chunk is read as
    /// the answer); a block's reply not forwarded (no MIDI response).
    #[test]
    fn owed_frames_are_taken_in_order_before_the_answer() {
        let channels = Channels::new(SampleRate(48_000.0));
        let mut owed = Owed::default();
        owed.owe(Late::Block);
        owed.owe(Late::Answer(is_value));
        owed.owe(Late::StateRest);

        assert!(owed.take(processed(3), &channels).is_none());
        assert!(matches!(
            channels.pop_audio_response(),
            Some(AudioResponse::AudioProcessed { seq: 3, .. })
        ));
        assert_eq!(channels.settled(), 1);
        let value = BridgeMessage::ParameterValue {
            value: Some(crate::protocol::Normalized::of(0.25)),
        };
        assert!(owed.take(value, &channels).is_none());
        assert!(owed.take(chunk(0, false), &channels).is_none());
        assert!(owed.take(chunk(1, true), &channels).is_none());
        assert!(owed.late.is_empty());

        let list = BridgeMessage::ParameterList {
            parameters: Vec::new(),
        };
        assert!(matches!(
            owed.take(list, &channels),
            Some(BridgeMessage::ParameterList { .. })
        ));
        assert_eq!(channels.settled(), 1, "only a block counts as settled");
    }

    /// **An owed answer that is not coming does not swallow the replies after
    /// it.** A frame of another kind lets the owed item go and is weighed
    /// against the next; the server's `Error` answers any request.
    ///
    /// Mutations (run), each fails: `Late::Answer` taking any frame (the list
    /// is swallowed as the late value); `StateRest` taking any frame (the same);
    /// `Error` not answering an owed query (it is returned as the answer).
    #[test]
    fn a_frame_of_another_kind_lets_an_owed_answer_go() {
        let channels = Channels::new(SampleRate(48_000.0));
        let list = || BridgeMessage::ParameterList {
            parameters: Vec::new(),
        };

        let mut owed = Owed::default();
        owed.owe(Late::Answer(is_value));
        owed.owe(Late::StateRest);
        assert!(matches!(
            owed.take(list(), &channels),
            Some(BridgeMessage::ParameterList { .. })
        ));
        assert!(owed.late.is_empty());

        owed.owe(Late::Answer(is_value));
        let error = BridgeMessage::Error {
            message: "refused".into(),
        };
        assert!(owed.take(error, &channels).is_none());
        assert!(owed.late.is_empty());
    }

    /// **A carried value goes into a reserved queue of the block's buffer**,
    /// so the buffer returns to the pool with its point room intact. A
    /// queue pushed past the spares (the carry's own, with the inline room)
    /// later took a reserved queue's place on `clear` and left the buffer
    /// with a 10-point queue among its reserved ones.
    ///
    /// Mutation (run): `merge_carried` pushes the carried queue
    /// (`changes.queues.push(kept)`) instead of opening a spare → the
    /// merged queue has the room of its one point → fails.
    #[test]
    fn a_carried_value_fills_a_reserved_queue() {
        let mut owed = Owed::default();
        let mut skipped = ParameterChanges::reserved(1, 1);
        skipped.add_change(ParamId::new(7), 3, 0.25);
        owed.carry(&skipped);

        let mut block = ParameterChanges::reserved(4, 40);
        assert!(block.open_queue(ParamId::new(1)));
        if let Some(q) = block.queues.last_mut() {
            q.add_point(5, 0.5);
        }
        owed.merge_carried(&mut block);

        let carried = block.queue(ParamId::new(7)).expect("carried");
        assert_eq!(carried.points.len(), 1);
        assert_eq!(carried.points[0].sample_offset, 0);
        assert!((carried.points[0].value.get() - 0.25).abs() < 1e-12);
        assert!(
            carried.points.capacity() >= 40,
            "the carried queue is a reserved one: {}",
            carried.points.capacity()
        );
        block.clear();
        assert_eq!(block.point_storage(), 4 * 41, "the reserved storage");
    }

    /// **A reserved buffer with no spare queue drops a carried value,
    /// counted, rather than growing; the empty placeholder takes every one.**
    /// A one-queue buffer with its queue open for another parameter: the
    /// carried value is dropped inside the no-alloc gate and counted, and the
    /// buffer still has one queue. The placeholder of a block with no
    /// changes (`ParameterChanges::new`) gets a queue for each of two
    /// carried values, though the first gives it queue room.
    ///
    /// Mutations (run), each fails: the no-spare case pushing a queue (as
    /// it did) → the gate trips on the allocation; the placeholder decided
    /// per value (`changes.capacity() == 0` inside the loop) → the second
    /// value opens a queue with no point room and is dropped.
    #[test]
    fn a_carried_value_past_the_queues_is_dropped_but_the_placeholder_takes_all() {
        let mut skipped = ParameterChanges::reserved(2, 1);
        skipped.add_change(ParamId::new(7), 3, 0.25);
        skipped.add_change(ParamId::new(8), 3, 0.5);

        let mut owed = Owed::default();
        owed.carry(&skipped);
        let mut block = ParameterChanges::reserved_bridged(1, 40);
        assert!(block.add_change(ParamId::new(1), 5, 0.5));
        assert_no_alloc::assert_no_alloc(|| owed.merge_carried(&mut block));
        assert_eq!(block.queues.len(), 1);
        assert_eq!(block.points_overflowed(), 2);

        let mut owed = Owed::default();
        owed.carry(&skipped);
        let mut placeholder = ParameterChanges::new();
        owed.merge_carried(&mut placeholder);
        assert_eq!(placeholder.queues.len(), 2);
        assert_eq!(placeholder.points_overflowed(), 0);
        for (id, value) in [(7, 0.25), (8, 0.5)] {
            let q = placeholder.queue(ParamId::new(id as u32)).expect("queued");
            assert_eq!(q.points.len(), 1);
            assert_eq!(
                (q.points[0].sample_offset, q.points[0].value.get()),
                (0, value)
            );
        }
    }

    /// **The carried values reach a chunk at the pool budget whole, in real
    /// slots, without allocating or growing it.** 2 000 parameters at a
    /// 512-frame block, the block's buffer built `reserved_bridged` as the
    /// buffer pool builds it. The block moves all but the last ten
    /// parameters, every one at its floor (first point after frame 0) and
    /// the last `POOLED_QUEUES` of them at their full room; a skipped block
    /// left a value owed for every parameter. Inside the no-alloc gate,
    /// `merge_carried` puts each on frame 0 (the ten unmoved ones in spare
    /// queues): nothing is counted, every queue gained exactly its carried
    /// point ahead of its own, and the buffer's storage is what it was.
    ///
    /// Mutation (run): `ParameterChanges::reserved_bridged` without its
    /// carry slot (`carry_slot: 0`) → `make_room` lets the full queues fill
    /// the kept point, so their carried values have no slot and no block
    /// left and are dropped, counted → fails.
    #[test]
    fn carried_values_take_real_slots_at_the_pool_budget_without_allocating() {
        const PARAMS: usize = 2_000;
        const UNMOVED: usize = 10;
        let room = crate::protocol::ParameterQueue::chunk_room(512);
        let moved = PARAMS - UNMOVED;
        let full = moved - ParameterChanges::POOLED_QUEUES..moved;
        let addr = |i: usize| ParamId::new((i as i32) as u32);

        let mut skipped = ParameterChanges::reserved(PARAMS, 1);
        for i in 0..PARAMS {
            assert!(skipped.add_change(addr(i), 7, 0.25));
        }
        let mut owed = Owed::default();
        owed.carry(&skipped);

        // Filled as the node's delivery fills it: a full queue takes points
        // while `make_room` gives it room (the producers' room, `room`).
        let mut block = ParameterChanges::reserved_bridged(PARAMS, room);
        for i in 0..moved {
            assert!(block.open_queue(addr(i)));
            if full.contains(&i) {
                let mut k = 0;
                while block.make_room(i) {
                    assert!(block.push_point(i, 1 + k, 0.5));
                    k += 1;
                }
            } else {
                for k in 0..ParameterChanges::FLOOR_POINTS {
                    assert!(block.push_point(i, 1 + k as i32, 0.5));
                }
            }
        }
        let before: Vec<usize> = block.queues.iter().map(|q| q.points.len()).collect();
        let storage = block.point_storage();

        assert_no_alloc::assert_no_alloc(|| owed.merge_carried(&mut block));

        assert_eq!(block.points_overflowed(), 0);
        assert_eq!(block.point_storage(), storage, "nothing grew");
        assert_eq!(block.queues.len(), PARAMS);
        for (i, q) in block.queues.iter().enumerate() {
            let own = before.get(i).copied().unwrap_or(0);
            assert_eq!(q.points.len(), own + 1, "queue {i}");
            assert_eq!(q.points[0].sample_offset, 0, "queue {i}");
            assert!((q.points[0].value.get() - 0.25).abs() < 1e-12);
        }
    }

    /// Every production block size / rate combination the engine can present.
    /// A chunk is one device callback (`MAX_CHUNK` at most), so the timeout
    /// must hold at every size.
    const RATES: [SampleRate; 4] = [
        SampleRate(44_100.0),
        SampleRate(48_000.0),
        SampleRate(96_000.0),
        SampleRate(192_000.0),
    ];
    const BLOCKS: [usize; 4] = [64, 128, 256, 512];

    fn period(num_samples: usize, rate: SampleRate) -> Duration {
        Duration::from_secs_f64(num_samples as f64 / rate.get())
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
            let t = process_timeout(64, SampleRate(bad));
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
    /// Asserting specific numbers (`MAX_BEHIND == RING_SLOTS`, `survivors ==
    /// [8, 9, 10]`) would pin an off-by-one as intended behaviour: at depth 2,
    /// `slot_for(8) == slot_for(10)`. Asserting the property makes the tests
    /// independent of the ring depth, so raising `RING_SLOTS` cannot silently
    /// introduce it.
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
