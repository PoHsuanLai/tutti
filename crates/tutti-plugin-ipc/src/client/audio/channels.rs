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

use super::messages::{AudioResponse, Command};
use crossbeam::queue::ArrayQueue;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tutti_rt::Doorbell;
use tutti_types::SampleRate;

/// How many commands the audio thread can queue before `push_command` fails.
/// This also bounds how many `Process` payloads can be outstanding at once,
/// which is why [`super::payload_pool::PayloadPool`] sizes itself from it.
pub(super) const COMMAND_QUEUE_SIZE: usize = 128;
const RESPONSE_QUEUE_SIZE: usize = 128;

#[derive(Clone)]
pub(super) struct Channels {
    commands: Arc<ArrayQueue<Command>>,
    audio_responses: Arc<ArrayQueue<AudioResponse>>,
    /// The newest block sequence the audio thread has submitted. Owned here
    /// rather than on the batcher because the *bridge* thread is what reads it,
    /// to decide whether a dequeued block is still worth sending.
    newest_seq: Arc<AtomicU64>,
    /// The bridge thread's doorbell: the thread sleeps on it when the queue
    /// is empty, and the audio thread rings it after a push. The ring is
    /// wait-free (no lock, no allocation) and cannot be missed
    /// ([`Doorbell`]'s protocol), so the bridge needs no polling timeout.
    bell: Arc<Doorbell>,
    /// The negotiated sample rate, as `f64::to_bits`. Lives here rather than
    /// only on `AudioBridge` because *both* threads size a timeout from the
    /// block period — the bridge thread its reply timeout, and the staleness
    /// bound its notion of how far behind a block may be. One shared source
    /// keeps the two from drifting apart: two independently-derived periods let
    /// a constant sit orders of magnitude off the real block period, which
    /// starves the command queue.
    sample_rate_bits: Arc<AtomicU64>,
    /// How many owed replies the drain has taken back off the socket, ever.
    ///
    /// Pure instrumentation, and the only externally visible evidence that the
    /// drain ran at all. Everything else it does is invisible by construction:
    /// the audio it recovers is already stale and rejected by the slab's
    /// sequence check, and the MIDI it forwards is indistinguishable from MIDI
    /// that arrived on time. Without this counter a test can only observe the
    /// drain's *absence* statistically — as a backlog that grows over a long
    /// enough run — which is a load-dependent assertion of exactly the kind this
    /// work exists to remove.
    settled: Arc<AtomicU64>,
    /// How many blocks the bridge thread abandoned on a missed budget, ever.
    /// The other half of [`settled`](Self::settled): every abandoned reply is
    /// owed back, so a test can bound what the drain took by what was owed
    /// instead of by how many budgets a loaded machine happened to miss.
    abandoned: Arc<AtomicU64>,
    /// How many replies the bridge thread has taken from the plugin, ever,
    /// on time or owed. With [`abandoned`](Self::abandoned) the hang
    /// verdict's input (`PluginHandle::block_counts`): a hang is abandoned
    /// blocks growing while this does not. Written by the bridge thread only;
    /// a relaxed read is enough, since a host that reads it is polling.
    replies: Arc<AtomicU64>,
    /// How many unsolicited parameter events the bridge thread dropped
    /// because more arrived during one command than its inbox holds, ever.
    /// Each drop also raises a `Resync(ParamValues)`, so the host re-reads.
    dropped_param_events: Arc<AtomicU64>,
    /// MIDI events the bridge thread did not send, ever: a stale block's,
    /// and the rest of a SysEx that lost packets with it. Shared with the
    /// plugin node's controls, which add it to the node's own count.
    dropped_midi: Arc<AtomicU64>,
}

impl Channels {
    pub(super) fn new(sample_rate: SampleRate) -> Self {
        Self {
            commands: Arc::new(ArrayQueue::new(COMMAND_QUEUE_SIZE)),
            audio_responses: Arc::new(ArrayQueue::new(RESPONSE_QUEUE_SIZE)),
            newest_seq: Arc::new(AtomicU64::new(0)),
            bell: Arc::new(Doorbell::new()),
            sample_rate_bits: Arc::new(AtomicU64::new(sample_rate.get().to_bits())),
            settled: Arc::new(AtomicU64::new(0)),
            abandoned: Arc::new(AtomicU64::new(0)),
            replies: Arc::new(AtomicU64::new(0)),
            dropped_param_events: Arc::new(AtomicU64::new(0)),
            dropped_midi: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The current sample rate. `Relaxed` suffices: it only sizes a timeout, so
    /// reading a value one block stale is harmless.
    pub(super) fn sample_rate(&self) -> SampleRate {
        SampleRate(f64::from_bits(
            self.sample_rate_bits.load(Ordering::Relaxed),
        ))
    }

    pub(super) fn set_sample_rate(&self, rate: SampleRate) {
        self.sample_rate_bits
            .store(rate.get().to_bits(), Ordering::Relaxed);
    }

    /// Park the bridge thread until a command is queued or `stop` says to
    /// leave, whichever is already true or comes first. May return with
    /// neither (a spurious wake-up): the caller rechecks. `stop` reads what
    /// whoever ends the loop publishes before calling [`wake`](Self::wake).
    pub(super) fn wait_for_command(&self, stop: impl FnOnce() -> bool) {
        self.bell.wait(|| !self.commands.is_empty() || stop());
    }

    /// Wake the bridge thread to re-check its stop condition (shutdown).
    /// Publish the condition first. Real-time safe, like the ring in
    /// [`push_command`](Self::push_command).
    pub(super) fn wake(&self) {
        self.bell.ring();
    }

    /// Record that the audio thread has submitted block `seq`. Called from the
    /// RT path, so it is a single relaxed store and nothing else.
    pub(super) fn note_submitted(&self, seq: u64) {
        self.newest_seq.store(seq, Ordering::Relaxed);
    }

    /// The newest block the audio thread has submitted.
    ///
    /// The bridge thread uses this to tell how far behind a command it just
    /// dequeued is, without draining the queue to look. `Relaxed` suffices: it
    /// only decides whether to skip work that is already provably useless, so
    /// reading a value one block stale costs at most one extra dead block.
    pub(super) fn newest_submitted(&self) -> u64 {
        self.newest_seq.load(Ordering::Relaxed)
    }

    /// Record that the drain took `n` owed replies off the socket. Called by the
    /// bridge thread only. `Release`, paired with the `Acquire` in
    /// [`settled`](Self::settled): a reader that sees a settle also sees the
    /// [`note_abandoned`](Self::note_abandoned) that owed it, so a test may
    /// assert `settled <= abandoned` by reading `settled` first.
    pub(super) fn note_settled(&self, n: u32) {
        if n > 0 {
            self.settled.fetch_add(n as u64, Ordering::Release);
            self.replies.fetch_add(u64::from(n), Ordering::Relaxed);
        }
    }

    /// How many owed replies the drain has taken, over the life of the bridge.
    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn settled(&self) -> u64 {
        self.settled.load(Ordering::Acquire)
    }

    /// Record that a block's reply missed its budget and is now owed. Called
    /// by the bridge thread only, before any settle of that reply; `Relaxed`
    /// because the `Release` on [`note_settled`](Self::note_settled) orders it.
    pub(super) fn note_abandoned(&self) {
        self.abandoned.fetch_add(1, Ordering::Relaxed);
    }

    /// Record that the plugin answered (a reply for a block, on time or
    /// owed): it is not hung. Bridge thread only.
    pub(super) fn note_replied(&self) {
        self.replies.fetch_add(1, Ordering::Relaxed);
    }

    /// Blocks abandoned and replies taken, over the life of the bridge. Two
    /// relaxed reads: a reply landing between them shows up on the next call,
    /// which is all a poller needs.
    pub(super) fn block_counts(&self) -> (u64, u64) {
        (
            self.abandoned.load(Ordering::Relaxed),
            self.replies.load(Ordering::Relaxed),
        )
    }

    /// How many blocks' replies were abandoned, over the life of the bridge.
    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn abandoned(&self) -> u64 {
        self.abandoned.load(Ordering::Relaxed)
    }

    /// Record `n` dropped unsolicited parameter events. Bridge thread only;
    /// `Relaxed`: a count nothing branches on.
    pub(super) fn note_dropped_param_events(&self, n: u64) {
        if n > 0 {
            self.dropped_param_events.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// Record `n` MIDI events not sent. Bridge thread only; `Relaxed`: a
    /// count nothing branches on.
    pub(super) fn note_dropped_midi(&self, n: u64) {
        if n > 0 {
            self.dropped_midi.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// The cell [`note_dropped_midi`](Self::note_dropped_midi) counts into.
    pub(super) fn dropped_midi_cell(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.dropped_midi)
    }

    /// How many unsolicited parameter events were dropped, over the life of
    /// the bridge.
    pub(super) fn dropped_param_events(&self) -> u64 {
        self.dropped_param_events.load(Ordering::Relaxed)
    }

    /// Pushes, then rings the bridge thread's doorbell, so the socket
    /// round-trip starts immediately rather than after a poll interval. The
    /// audio thread never waits on the reply, but the ring still matters: it
    /// is what keeps a block's reply arriving in time to be collected on the
    /// *next* block rather than the one after.
    ///
    /// The ring is wait-free and allocation-free (`Doorbell::ring`: a fence
    /// and a load, plus a futex wake when the bridge is parked), and it
    /// cannot be lost, so nothing backs it with a timeout. A push the queue
    /// refuses rings nothing: the bridge is, or is about to be, draining.
    pub(super) fn push_command(&self, cmd: Command) -> bool {
        self.try_push_command(cmd).is_ok()
    }

    /// [`push_command`](Self::push_command), handing a refused command back
    /// rather than dropping it, so the audio thread can return what it holds
    /// to its pools instead of freeing it.
    pub(super) fn try_push_command(&self, cmd: Command) -> Result<(), Command> {
        self.commands.push(cmd)?;
        self.bell.ring();
        Ok(())
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channels() -> Channels {
        Channels::new(SampleRate(48_000.0))
    }

    /// Abandoned blocks and replies are separate monotonic totals: a poller
    /// who sees the first grow while the second does not has a hang.
    ///
    /// Mutations (each run, each failed): `note_abandoned` not counting;
    /// `note_replied` a no-op; `note_settled` not counting its replies.
    #[test]
    fn blocks_abandoned_and_replies_are_counted_apart() {
        let c = channels();
        assert_eq!(c.block_counts(), (0, 0));
        for _ in 0..3 {
            c.note_abandoned();
        }
        assert_eq!(c.block_counts(), (3, 0));
        c.note_replied();
        assert_eq!(c.block_counts(), (3, 1), "an on-time reply");
        c.note_settled(2);
        assert_eq!(c.block_counts(), (3, 3), "owed replies taken back");
        c.note_settled(0);
        assert_eq!(c.block_counts(), (3, 3), "settling nothing is no reply");
    }

    /// A bridge thread as `pump` runs one: sleep for a command, take it,
    /// report it, until `stop`. Returns the handle once it is parked.
    fn bridge(
        c: &Channels,
        stop: &Arc<std::sync::atomic::AtomicBool>,
    ) -> (
        std::thread::JoinHandle<()>,
        std::sync::mpsc::Receiver<Command>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        let (c, stop) = (c.clone(), Arc::clone(stop));
        let t = std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                match c.pop_command() {
                    Some(cmd) => tx.send(cmd).unwrap(),
                    None => c.wait_for_command(|| stop.load(Ordering::Acquire)),
                }
            }
        });
        (t, rx)
    }

    const PATIENCE: std::time::Duration = std::time::Duration::from_secs(20);

    /// **A push wakes a parked bridge, and the audio thread's half
    /// allocates nothing.** The ring runs inside `assert_no_alloc` with the
    /// bridge parked, so the futex-wake path is the one gated, not just the
    /// fence and load.
    ///
    /// Mutations (run): the `self.bell.ring()` removed from
    /// `try_push_command` -> the bridge sleeps on, the receive times out and
    /// fails; the ring replaced by a `Box::new(0)` -> the gate aborts.
    #[test]
    fn a_push_wakes_a_parked_bridge_without_allocating() {
        let c = channels();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (t, rx) = bridge(&c, &stop);
        while !c.bell.is_asleep() {
            std::thread::yield_now();
        }
        assert_no_alloc::assert_no_alloc(|| assert!(c.push_command(Command::Shutdown)));
        let got = rx.recv_timeout(PATIENCE).expect("the push woke the bridge");
        assert!(matches!(got, Command::Shutdown));
        stop.store(true, Ordering::Release);
        c.wake();
        t.join().unwrap();
    }

    /// **No push is left sitting in the queue**: many hand-offs, each
    /// answered before the next, race the bridge's park against the ring.
    /// A lost wake-up leaves one unanswered and the receive times out (there
    /// is no backstop timeout to hide it).
    ///
    /// Mutation (run): the ring removed from `try_push_command` -> the first
    /// hand-off that finds the bridge parked never completes; fails.
    #[test]
    fn no_pushed_command_waits_on_a_missed_wake_up() {
        let c = channels();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (t, rx) = bridge(&c, &stop);
        for i in 0..20_000u32 {
            // Alternate a spin so the push lands both before and after the park.
            for _ in 0..(i % 64) {
                std::hint::spin_loop();
            }
            assert!(c.push_command(Command::Shutdown));
            rx.recv_timeout(PATIENCE)
                .unwrap_or_else(|_| panic!("hand-off {i} was never woken"));
        }
        stop.store(true, Ordering::Release);
        c.wake();
        t.join().unwrap();
    }

    /// **`wake` ends a wait on a stop flag with no command queued**, which
    /// is how `shutdown` reaches a parked bridge when its queue was full.
    ///
    /// Mutation (run): `wake` a no-op -> the join hangs (nextest times out).
    #[test]
    fn wake_ends_a_wait_with_nothing_queued() {
        let c = channels();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (t, _rx) = bridge(&c, &stop);
        while !c.bell.is_asleep() {
            std::thread::yield_now();
        }
        stop.store(true, Ordering::Release);
        c.wake();
        t.join().unwrap();
    }
}
