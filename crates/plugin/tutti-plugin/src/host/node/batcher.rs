//! [`Batcher`] — the pipeline between the plugin node and the plugin-server
//! bridge: every chunk it ships is exactly [`chunk`](Batcher::chunk) frames,
//! and the output is exactly one chunk late.
//!
//! [`process`](Batcher::process) takes a call's input into a FIFO and writes
//! the call's output from a ring. When the FIFO fills, the chunk is staged
//! into the shared-memory slab and submitted; when the ring's first frame is
//! next needed, the chunk submitted last is collected into it.
//!
//! # Pipelined, never waiting
//!
//! The batcher **submits chunk N and plays chunk N−1's output**, never waiting
//! for a reply. This replaced a synchronous version that spun on the audio
//! thread, where each node's wait was individually reasonable — half its own
//! block period — but the budgets *summed*: the graph runs nodes serially in
//! one callback, so three stalled plugins spent 3 × 667 µs against a 1333 µs
//! deadline. Parallelising the graph would not have helped; plugins in series
//! are a dependency chain. The defect was the waiting.
//!
//! Not waiting makes a stalled plugin cost zero, however many there are and
//! whatever the graph's shape. The price is one chunk of latency per
//! out-of-process plugin — *declared to PDC* (the node's `Shape::latency`,
//! through `PluginControls::declared_latency`) and so compensated rather than
//! heard. This is what JACK, PipeWire, AUv3 and every DAW that hosts plugins
//! out of process do.
//!
//! # A FIFO, so every chunk is whole
//!
//! The calls the node is handed need not line up with chunks: the engine
//! renders a device callback in 64-frame passes while a `Legacy`-flagged node
//! is in the graph, and an export may render 100-frame blocks. Shipping each
//! call as it came (a 36-frame submission, then a 64-frame one collecting it)
//! dropped or zero-padded frames wherever consecutive lengths differed. So a
//! call's input only ever fills the FIFO, a submission is always one whole
//! chunk, and the output is read from the ring at the same position the input
//! is written: output frame `t` is the plugin's output for input frame
//! `t - chunk`, for any cut of the calls.
//!
//! # One device callback per chunk
//!
//! The chunk is settled by [`prepare`](Batcher::prepare): the host's device
//! quantum (`Prepare::quantum`) when it knows one, else the graph's
//! `MaxBlock`, never past the slab's [`MAX_CHUNK`]. Live, that makes a chunk
//! complete on a callback's last frame and its output first needed on the
//! next callback's first — so the server has a whole device period to answer,
//! and the plugin's latency is one device block, as a DAW hosting plugins out
//! of process has it.
//!
//! Doc 013 had decided the opposite first (decision 8: a fixed 64-frame
//! pipeline, for the lower latency), and reversed it on measurement: with a
//! 64-frame chunk inside a 480-frame callback, every chunk but the first is
//! collected microseconds after it was submitted, and 186 of 200 blocks
//! rendered silent (441: 198; 1024: 187); with the callback as the chunk, 0
//! of 200 (`tests/clap_live.rs`). A chunk that is not the callback — a host
//! that does not know its quantum, a device calling back with more than
//! `MAX_CHUNK` frames, or one whose callbacks vary — still delays exactly
//! `chunk` frames, but may collect chunks the server had no time to answer.
//! An offline fork waits for every chunk, so it is exact whatever its chunk.
//!
//! Whether N−1's output is really there is decided by the slab's per-slot
//! sequence numbers, not by a reply arriving: a mismatch yields silence. See
//! `util::transport::shm::header`.
//!
//! # `f32` in the graph, the plugin's format on the wire
//!
//! The graph hands every node planar `f32` (doc 013, owner decision 2). The
//! wire carries whatever the plugin negotiated at load, so a plugin that
//! processes in double is converted here, in the wire scratch, and nowhere
//! else: the `f64` stays inside the node.

use super::fork::ForkWatch;
use crate::error::Result;
use crate::host::ipc_client::PluginBridge;
use crate::host::node::BlockPayload;
use crate::protocol::{MidiEvent, MidiEventVec, SampleFormat};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tutti_core::Samples;
use tutti_graph::Prepare;

/// The largest chunk that crosses the process edge: the largest device
/// callback a live plugin keeps in step with, and so the ceiling on the
/// pipeline's declared latency (85 ms at 48 kHz).
///
/// The shared-memory slab is sized to this at launch (`subprocess::launch`,
/// the other consumer of this constant; or to the host's smaller
/// `max_buffer_size`), before the node is prepared — so [`Batcher::prepare`]
/// can only narrow the chunk, never widen it, and the slab and the batcher
/// agree on the per-chunk ceiling by construction. A device calling back
/// with more frames than this gets a chunk of this many, which no longer
/// lines up with its callbacks (module docs).
pub(crate) const MAX_CHUNK: usize = 4096;

/// Single-chunk wire scratch in the plugin-negotiated format. Allocated at
/// [`Batcher::new`] for the chunk ceiling, because the wire format is known
/// up-front and no chunk is ever longer.
enum WireStorage {
    F32 { input: Vec<f32>, output: Vec<f32> },
    F64 { input: Vec<f64>, output: Vec<f64> },
}

impl WireStorage {
    fn new(format: SampleFormat, ceiling: usize) -> Self {
        match format {
            SampleFormat::Float32 => Self::F32 {
                input: vec![0.0; ceiling],
                output: vec![0.0; ceiling],
            },
            SampleFormat::Float64 => Self::F64 {
                input: vec![0.0; ceiling],
                output: vec![0.0; ceiling],
            },
        }
    }

    /// Stage `samples` as channel `ch` of chunk `seq`, converting to the
    /// wire's format. Staging only — the caller publishes once, after the
    /// last channel.
    fn stage(&mut self, bridge: &PluginBridge, seq: u64, ch: usize, samples: &[f32]) -> Result<()> {
        let n = samples.len();
        match self {
            Self::F32 { input, .. } => {
                let wire = &mut input[..n];
                wire.copy_from_slice(samples);
                bridge.audio_buffer().write_input(seq, ch, wire)
            }
            Self::F64 { input, .. } => {
                let wire = &mut input[..n];
                for (d, &s) in wire.iter_mut().zip(samples) {
                    *d = f64::from(s);
                }
                bridge.audio_buffer().write_input(seq, ch, wire)
            }
        }
    }

    /// Copy channel `ch` of chunk `seq`'s output into `out`, converting from
    /// the wire's format, and zero-pad past what the slab holds.
    ///
    /// Only call this for a `seq` the slab has confirmed (see
    /// [`Batcher::collectable`]): the copy count proves nothing about who
    /// wrote the bytes.
    fn recv(&mut self, bridge: &PluginBridge, seq: u64, ch: usize, out: &mut [f32]) {
        let size = out.len();
        let n = match self {
            Self::F32 { output, .. } => {
                let wire = &mut output[..size];
                let n = bridge
                    .audio_buffer()
                    .read_output_into(seq, ch, wire)
                    .unwrap_or(0);
                out[..n].copy_from_slice(&wire[..n]);
                n
            }
            Self::F64 { output, .. } => {
                let wire = &mut output[..size];
                let n = bridge
                    .audio_buffer()
                    .read_output_into(seq, ch, wire)
                    .unwrap_or(0);
                for (o, &v) in out[..n].iter_mut().zip(&wire[..n]) {
                    // The narrowing is the point: the graph is `f32`.
                    *o = v as f32;
                }
                n
            }
        };
        out[n..].fill(0.0);
    }
}

/// What the plugin node gives the batcher per chunk: the chunk's payload, and
/// where its MIDI-out goes.
///
/// A trait rather than arguments because a chunk and a call are not the same
/// span (module docs, "A FIFO, so every chunk is whole"): the node is told
/// where each chunk **begins** in the call that begins it, and is asked for
/// its payload when the chunk is **submitted**, possibly a call later.
pub(super) trait Chunks {
    /// A chunk of `chunk` frames begins at frame `at` of this call (its first
    /// frame is the next input frame the batcher takes).
    fn begin(&mut self, at: usize, chunk: usize);
    /// This call's frames `from..from + n` go into the current chunk at its
    /// frame `at`.
    fn take(&mut self, from: usize, n: usize, at: usize);
    /// The payload of the chunk being submitted now, `frames` long: what
    /// [`begin`](Self::begin) gathered for it.
    fn payload(&mut self, frames: usize) -> BlockPayload;
    /// The plugin's MIDI-out for the chunk the ring now holds, at that
    /// chunk's frames, sorted: handed over once, as its output starts to
    /// play.
    fn midi_out(&mut self, events: &MidiEventVec);
    /// One of those events, at frame `frame` of this call: where the ring
    /// plays the chunk frame it was emitted at, so it keeps its place against
    /// the plugin's audio. Called in frame order.
    fn emit(&mut self, frame: usize, event: MidiEvent);
}

/// Sort `events` by frame offset, stably, in place: an insertion sort, for
/// lists that are short and nearly sorted. Allocation-free.
pub(super) fn sort_by_offset(events: &mut MidiEventVec) {
    for i in 1..events.len() {
        let mut j = i;
        while j > 0 && events[j - 1].frame_offset > events[j].frame_offset {
            events.swap(j - 1, j);
            j -= 1;
        }
    }
}

/// The pipeline between the plugin node and the plugin-server bridge: an
/// input FIFO that ships a chunk when full, and an output ring the previous
/// chunk's output is read from.
pub(crate) struct Batcher {
    wire: WireStorage,

    /// The slab's per-chunk size: the most [`chunk`](Self::chunk) can be.
    ceiling: usize,
    /// The chunk [`prepare`](Self::prepare) settled on: the frames every
    /// submission carries, and so the pipeline's latency.
    chunk: usize,

    /// Input frames not shipped yet, one row per input port (main plus
    /// sidechain/aux buses, bus-ordered: row `ch` is the slab's input-region
    /// channel `ch`, so a graph edge into the node's port 2 feeds the plugin's
    /// sidechain bus), `ceiling` long.
    fifo: Vec<Vec<f32>>,
    /// The output of the chunk collected last, one row per output port.
    ring: Vec<Vec<f32>>,
    /// Frames of the current chunk taken so far (`0..chunk`): where the next
    /// input frame goes, and which ring frame the next output reads.
    pos: usize,

    /// The sequence number the next submitted chunk will carry. Starts at 1
    /// because 0 means "nothing published" in a freshly zeroed slab.
    next_seq: u64,
    /// The chunk submitted and not collected yet, or `None` when nothing is
    /// in flight (start-up, after a reset, after a failed submission).
    expect_seq: Option<u64>,
    /// `Some` only on an **offline fork** (`host::node::fork`): before
    /// collecting a chunk, wait up to this long for the server to publish it.
    /// See [`await_output`](Self::await_output).
    offline_wait: Option<OfflineWait>,
    /// The plugin's MIDI-out for the chunk the ring holds, at that chunk's
    /// frames, sorted (`collect`). Inline, so the drain never allocates.
    reply: MidiEventVec,
}

/// An offline fork's wait: its [`ForkWatch`] holds the per-block budget and
/// the latches.
struct OfflineWait {
    watch: Arc<ForkWatch>,
}

impl Batcher {
    /// A batcher for a plugin with these widths and wire format, over a slab
    /// whose chunks hold `ceiling` frames. Control thread: allocates the wire
    /// scratch, the FIFO and the ring. The chunk starts at the ceiling;
    /// [`prepare`](Self::prepare) narrows it to the graph's `MaxBlock`.
    pub(super) fn new(inputs: usize, outputs: usize, format: SampleFormat, ceiling: usize) -> Self {
        Self {
            wire: WireStorage::new(format, ceiling),
            ceiling,
            chunk: ceiling,
            fifo: vec![vec![0.0; ceiling]; inputs],
            ring: vec![vec![0.0; ceiling]; outputs],
            pos: 0,
            next_seq: 1,
            expect_seq: None,
            offline_wait: None,
            reply: MidiEventVec::new(),
        }
    }

    /// Settle the chunk for `p`: the host's device quantum when it knows one
    /// (live: one chunk per device callback, so the server has a whole
    /// device period to answer), else the graph's `MaxBlock` (an offline
    /// render, or a host that does not know its callback), never past the
    /// slab's ceiling (module docs, "One device callback per chunk"). Starts
    /// the pipeline over, as a re-prepare starts the node over.
    pub(super) fn prepare(&mut self, p: &Prepare) {
        let want = p.quantum().map_or(p.max_block().get(), Samples::get);
        self.chunk = self.ceiling.min(want).max(1);
        self.reset();
    }

    /// The frames every submission carries.
    pub(super) fn chunk(&self) -> usize {
        self.chunk
    }

    /// [`chunk`](Self::chunk) as the latency it adds: exactly one chunk,
    /// whatever the blocks (module docs).
    pub(super) fn pipeline_latency(&self) -> Samples {
        Samples(self.chunk)
    }

    /// Make every collect wait for its chunk, up to `budget` per chunk: the
    /// offline mode of a forked instance.
    ///
    /// The pipeline never waits because it runs on the audio thread, where a
    /// wait is a dropout (module docs). An offline fork runs on a render
    /// worker with no deadline, as fast as the plugin answers — so there the
    /// same rule is the defect: a render loop outpaces the subprocess, every
    /// chunk it has not published yet reads as silence, and the export is
    /// mostly silent with no error. Waiting keeps the pipelined shape and so
    /// the declared one-chunk latency, and makes the output a function of the
    /// input rather than of scheduling.
    ///
    /// The budget and the latches are the fork's [`ForkWatch`]: it records a
    /// miss or a dead server there, and its health probe reports them.
    pub(super) fn set_offline_wait(&mut self, watch: Arc<ForkWatch>) {
        self.offline_wait = Some(OfflineWait { watch });
    }

    /// With [`set_offline_wait`](Self::set_offline_wait): block until the
    /// chunk this call collects is published, the bridge crashes, or the
    /// budget runs out. A no-op otherwise, and when nothing is in flight.
    ///
    /// **The first miss is the last wait.** A chunk still missing at the
    /// budget is collected as silence, as the live path would, and the wait
    /// latches `gave_up`: from then on every chunk is collected at once
    /// (silence, unless the server catches up), so a hung server costs one
    /// budget per render, not one per chunk — an hour-long export through a
    /// wedged plugin would otherwise take days to report its failure. The
    /// fork's health probe turns the latch into `ForkFaultKind::TimedOut`,
    /// which the renderer reports.
    ///
    /// **A dead server ends the wait at once**, and reads as `Crashed`. The
    /// bridge alone would not notice in time: it learns of a dead peer only
    /// when it next reads the socket, which it does for a command, and this
    /// wait sends none. So the wait also asks the process itself
    /// ([`ForkWatch::server_died`]), every [`PROCESS_POLL`].
    fn await_output(&mut self, bridge: &PluginBridge) {
        const PROCESS_POLL: Duration = Duration::from_millis(5);
        let (Some(wait), Some(seq)) = (&self.offline_wait, self.expect_seq) else {
            return;
        };
        let watch = &wait.watch;
        if watch.stopped_waiting() {
            return;
        }
        let deadline = Instant::now() + watch.budget();
        let mut next_poll = Instant::now() + PROCESS_POLL;
        while !bridge.is_crashed() && !bridge.audio_buffer().has_output(seq) {
            let now = Instant::now();
            if now >= next_poll {
                if watch.server_died() {
                    return;
                }
                next_poll = now + PROCESS_POLL;
            }
            if now >= deadline {
                watch.give_up();
                tracing::warn!(
                    seq,
                    budget = ?watch.budget(),
                    "offline plugin fork: block not published in time; \
                     rendering silence from here on"
                );
                return;
            }
            // Short, not a spin: this is a worker thread, and the server needs
            // the core more than this loop does.
            std::thread::sleep(Duration::from_micros(50));
        }
        // Latch what the bridge saw into the watch: the health probe holds the
        // bridge weakly, and a render that ends before the next process poll
        // drops it (with the executor) before anyone asks, which read as a
        // healthy fork.
        if bridge.is_crashed() {
            watch.latch_crash(bridge.crash_cause());
            return;
        }
        // The server publishes a chunk's audio, then sends its reply: wait
        // for the reply too, or an export's MIDI-out would depend on how the
        // two raced. Within the same budget; a reply that never comes costs
        // its MIDI, never the audio (no `give_up`).
        while !bridge.take_replies(seq, &mut self.reply) {
            if bridge.is_crashed() || Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_micros(50));
        }
    }

    /// Drop the chunk in flight, the frames taken towards the next one and
    /// the output not yet played, and start collecting fresh.
    ///
    /// **`next_seq` is deliberately not reset.** This is the load-bearing
    /// subtlety of the whole pipeline: if the count restarted at 1, a chunk
    /// submitted before a seek could be published after it and accepted as the
    /// new chunk 1 — the seek would replay a fragment of pre-seek audio. Keeping
    /// the sequence monotonic makes a late publish structurally unmatchable.
    /// (A `u64` at ~750 chunks/s takes on the order of 780,000 years to wrap.)
    ///
    /// The server's own `Reset` is a no-op for this purpose, so clearing
    /// `expect_seq` here is the *entire* mechanism by which a seek stops old
    /// audio from arriving.
    pub(super) fn reset(&mut self) {
        self.expect_seq = None;
        self.pos = 0;
        self.reply.clear();
        for row in self.fifo.iter_mut().chain(self.ring.iter_mut()) {
            row.fill(0.0);
        }
    }

    /// Whether chunk `expect_seq`'s output is really available to read.
    ///
    /// Two independent conditions, and the crash check is first on purpose. A
    /// crashed bridge never publishes, so the sequence would mismatch anyway and
    /// the audio would come out silent either way — but relying on that makes
    /// correct behaviour a *coincidence* of the sequence numbering rather than a
    /// decision. One relaxed atomic load, already on this path.
    fn collectable(&self, bridge: &PluginBridge) -> Option<u64> {
        if bridge.is_crashed() {
            return None;
        }
        let seq = self.expect_seq?;
        bridge.audio_buffer().has_output(seq).then_some(seq)
    }

    /// Fill the ring with the output of the chunk in flight: the one submitted
    /// last, whose input the ring's frames were played against a chunk ago.
    /// Silence when nothing is collectable (start-up, a reset, a crashed
    /// bridge, a chunk the server never answered).
    ///
    /// Called when the ring's first frame is needed and not before, so a chunk
    /// submitted at the end of one call has until the next call's output to
    /// be answered — the time the pipeline exists to give the plugin.
    ///
    /// The chunk's MIDI-out comes with it: its reply (and any earlier one that
    /// missed its own collection, at frame 0) is drained into `reply`, handed
    /// to `host` once and then emitted as the ring plays it (`process`).
    fn collect(&mut self, bridge: &PluginBridge, host: &mut impl Chunks) {
        self.reply.clear();
        self.await_output(bridge);
        let chunk = self.chunk;
        match self.collectable(bridge) {
            Some(seq) => {
                for (ch, row) in self.ring.iter_mut().enumerate() {
                    self.wire.recv(bridge, seq, ch, &mut row[..chunk]);
                }
            }
            None => {
                for row in &mut self.ring {
                    row[..chunk].fill(0.0);
                }
            }
        }
        bridge.take_replies(self.expect_seq.unwrap_or(0), &mut self.reply);
        let last = u32::try_from(chunk.saturating_sub(1)).unwrap_or(u32::MAX);
        for e in self.reply.iter_mut() {
            e.frame_offset = e.frame_offset.min(last);
        }
        sort_by_offset(&mut self.reply);
        host.midi_out(&self.reply);
        self.expect_seq = None;
    }

    /// Ship the full FIFO as the next chunk: stage every input port, publish
    /// the input slot **exactly once, after the last channel** (a per-channel
    /// publish would let the server observe the slot as valid while later
    /// channels are still being copied, and half of this chunk spliced onto
    /// half of the previous one sounds almost right — far worse than
    /// silence), and hand the chunk and its payload to the bridge.
    fn submit(&mut self, bridge: &PluginBridge, host: &mut impl Chunks) {
        let chunk = self.chunk;
        let seq = self.next_seq;
        let mut staged = true;
        for (ch, row) in self.fifo.iter().enumerate() {
            if self.wire.stage(bridge, seq, ch, &row[..chunk]).is_err() {
                staged = false;
                break;
            }
        }
        if !staged {
            // The chunk will never be collectable; its output plays as silence.
            self.expect_seq = None;
            return;
        }
        bridge.audio_buffer().publish_input(seq);
        let p = host.payload(chunk);
        let submitted = bridge.submit(
            seq,
            chunk,
            p.midi,
            p.params,
            p.note_expression,
            p.harmony,
            p.transport,
        );
        if submitted {
            self.next_seq += 1;
            self.expect_seq = Some(seq);
        } else {
            self.expect_seq = None;
        }
    }

    /// Take `frames` frames of `input` (one slice per input port) and write
    /// `frames` frames of `output` (one slice per output port): output frame
    /// `t` is the plugin's output for input frame `t - chunk`, exactly, however
    /// the calls are cut.
    ///
    /// An input port past `input.len()` is taken as silence; an output port
    /// past `output.len()` is not written.
    pub(super) fn process(
        &mut self,
        bridge: &PluginBridge,
        frames: usize,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        host: &mut impl Chunks,
    ) {
        let chunk = self.chunk;
        let mut i = 0;
        while i < frames {
            if self.pos == 0 {
                // A new chunk: its output-side frames need the chunk before it.
                self.collect(bridge, host);
                host.begin(i, chunk);
            }
            let n = (chunk - self.pos).min(frames - i);
            let (at, to) = (self.pos, self.pos + n);
            host.take(i, n, at);
            for (row, out) in self.ring.iter().zip(output.iter_mut()) {
                out[i..i + n].copy_from_slice(&row[at..to]);
            }
            let first = self
                .reply
                .partition_point(|e| (e.frame_offset as usize) < at);
            for e in &self.reply[first..] {
                let o = e.frame_offset as usize;
                if o >= to {
                    break;
                }
                host.emit(i + o - at, *e);
            }
            for (ch, row) in self.fifo.iter_mut().enumerate() {
                match input.get(ch) {
                    Some(inp) => row[at..to].copy_from_slice(&inp[i..i + n]),
                    None => row[at..to].fill(0.0),
                }
            }
            self.pos = to;
            i += n;
            if self.pos == chunk {
                self.submit(bridge, host);
                self.pos = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::SampleRate;

    /// A fresh batcher has nothing in flight, so its first chunk must be silence
    /// rather than whatever the output slot happens to contain.
    #[test]
    fn a_fresh_batcher_expects_nothing() {
        let b = Batcher::new(2, 2, SampleFormat::Float32, MAX_CHUNK);
        assert_eq!(b.expect_seq, None);
        assert_eq!(b.next_seq, 1, "0 means 'never published' in the slab");
    }

    /// `reset` (a seek) drops the in-flight chunk but must NOT rewind the
    /// sequence. Rewinding would let a chunk submitted before the seek be
    /// published after it and accepted as the new chunk 1, replaying a
    /// fragment of pre-seek audio.
    #[test]
    fn reset_drops_the_in_flight_block_without_rewinding() {
        let mut b = Batcher::new(2, 2, SampleFormat::Float32, MAX_CHUNK);
        b.next_seq = 100;
        b.expect_seq = Some(99);

        b.reset();
        assert_eq!(b.expect_seq, None, "the pre-seek block is abandoned");
        assert_eq!(
            b.next_seq, 100,
            "the sequence is monotonic across a seek — this is what makes a \
             late pre-seek publish structurally unmatchable"
        );
    }

    /// The *consequence* of that monotonicity: no sequence issued after a seek
    /// can equal one issued before it, so a late pre-seek publish cannot be
    /// mistaken for post-seek audio however long it takes to arrive.
    ///
    /// `has_output` compares sequences for equality, so this disjointness is what
    /// actually stops the replay. The test above pins the two fields; this pins
    /// what they buy.
    ///
    /// Asserted here rather than end-to-end, deliberately. Driving a real seek
    /// through the mock server means suspending it mid-block so the held reply
    /// lands after the reset — but the mock is single-threaded, so a suspended
    /// server also stops draining the 128-slot command queue. Every post-seek
    /// submit is then rejected and the pipeline goes silent for a reason that has
    /// nothing to do with `reset`. Measured, not assumed: all 8 post-seek submits
    /// failed. A real subprocess keeps reading its socket however slow its DSP
    /// is, so that failure is an artifact of the harness and the end-to-end test
    /// would have been pinning the artifact.
    #[test]
    fn no_post_seek_sequence_can_collide_with_a_pre_seek_one() {
        let mut b = Batcher::new(2, 2, SampleFormat::Float32, MAX_CHUNK);

        let mut pre_seek = Vec::new();
        for _ in 0..5 {
            pre_seek.push(b.next_seq);
            b.next_seq += 1;
        }
        // One of them is still in flight when the seek happens.
        b.expect_seq = Some(*pre_seek.last().unwrap());

        b.reset();

        let mut post_seek = Vec::new();
        for _ in 0..5 {
            post_seek.push(b.next_seq);
            b.next_seq += 1;
        }

        for issued in &post_seek {
            assert!(
                !pre_seek.contains(issued),
                "sequence {issued} was issued both before and after the seek: a \
                 pre-seek block publishing late would satisfy `has_output` for a \
                 post-seek block and replay its audio"
            );
        }
        assert!(
            post_seek[0] > *pre_seek.last().unwrap(),
            "post-seek sequences must continue past the pre-seek ones, not \
             restart alongside them"
        );
    }

    /// The pipeline's chunk — and so the latency it declares — comes from
    /// `prepare`: the host's device quantum when it has one (one chunk per
    /// callback: doc 013 reversed decision 8, a live plugin now keeps in step
    /// with the device rather than a 64-frame grid), else the graph's
    /// `MaxBlock`, capped by the slab's ceiling either way.
    ///
    /// Mutation: `self.chunk = self.ceiling.min(64)` in `prepare` (the
    /// reversed 64-frame pipeline) → the 480-frame quantum ships 64 → fails.
    /// Mutation: ignore `quantum()` → 1024 instead of 480 → fails. Mutation:
    /// drop the `ceiling.min` → the small slab's chunk is 1024 → fails.
    #[test]
    fn the_pipeline_chunk_is_the_device_quantum_else_the_max_block() {
        let prepare = |max: usize| Prepare::new(SampleRate(48_000.0), Samples(max));
        let mut b = Batcher::new(2, 2, SampleFormat::Float32, MAX_CHUNK);
        b.prepare(&prepare(1024).with_quantum(Samples(480)));
        assert_eq!(b.pipeline_latency(), Samples(480));
        b.prepare(&prepare(64).with_quantum(Samples(2048)));
        assert_eq!(
            b.pipeline_latency(),
            Samples(2048),
            "the quantum, not MaxBlock"
        );
        b.prepare(&prepare(1024));
        assert_eq!(b.pipeline_latency(), Samples(1024), "no quantum: MaxBlock");
        b.prepare(&prepare(32));
        assert_eq!(b.pipeline_latency(), Samples(32));

        let mut small_slab = Batcher::new(2, 2, SampleFormat::Float32, 16);
        small_slab.prepare(&prepare(1024));
        assert_eq!(small_slab.pipeline_latency(), Samples(16));
    }

    /// A re-prepare starts the pipeline over: the chunk in flight was
    /// submitted under the old chunk size and must not be collected into the
    /// new one — without advancing the sequence, as for a seek.
    ///
    /// Mutation: drop the `reset()` from `prepare` → `expect_seq` survives →
    /// fails.
    #[test]
    fn prepare_drops_the_chunk_in_flight() {
        let mut b = Batcher::new(2, 2, SampleFormat::Float32, MAX_CHUNK);
        b.next_seq = 7;
        b.expect_seq = Some(6);
        b.prepare(&Prepare::new(SampleRate(48_000.0), Samples(128)));
        assert_eq!(b.expect_seq, None);
        assert_eq!(b.next_seq, 7);
    }
}
