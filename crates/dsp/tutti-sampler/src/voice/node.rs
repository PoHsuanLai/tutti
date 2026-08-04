//! [`VoiceNode`] — a single [`Voice`] as its own graph node.
//!
//! Zero inputs, N outputs, and — via [`VoiceNode::with_commands`] — a command
//! channel for the one control `AudioUnit::set` cannot carry. For resynth,
//! preview and a single timeline clip, where a whole pool would be ceremony. It shares [`VoiceSlot`] with the pool, so the
//! per-voice read is the same code in both — a fix in one is a fix in both.

use std::sync::Arc;

use crate::stretch;
use crate::{nonempty, MAX_SAMPLER_CHANNELS};

use super::command::{VoiceCommand, VoiceNodeHandle, COMMAND_CAPACITY};
use super::slot::{stretch_wanted, VoiceSlot};
use super::types::{SlotId, Voice};
use crossbeam_channel::{bounded, Receiver};
use tutti_core::transport::BeatCursor;
use tutti_core::{
    AudioUnit, BufferMut, BufferRef, ChannelLayout, SampleRate, SignalFrame, Timeline,
};

// ---------------------------------------------------------------------------
// VoiceNode — a standalone single-`Voice` graph node (0 inputs, 2 outputs).
//
// The mixer (`VoicePool`) holds a *list* of voices and sums them; a
// `VoiceNode` holds exactly ONE and plays it — a degenerate single-voice
// mixdown. Its `tick`/`process` are the SAME per-voice read the mixer does for
// one slot, shared through [`VoiceSlot::tick_frame`] / [`VoiceSlot::process_into`]
// (no duplication, no per-sample dyn).
//
// This is the standalone-graph-node case: `dawai-spectral`'s resynth adds a bare
// voice node to its net, and `tutti-export`'s region render downcasts these
// nodes to rebind their transport offline. Both rely on a `Voice` being an
// `AudioUnit` in its own right — not only reachable through the mixer — so this
// node keeps that path alive (guarded by a test).
// ---------------------------------------------------------------------------

pub struct VoiceNode {
    pub(crate) slot: VoiceSlot,
    /// Output width — see [`VoicePool`]'s field of the same name.
    pub(crate) channels: ChannelLayout,
    /// Detects transport discontinuities so buffered audio can be flushed on a
    /// seek — the standalone twin of [`VoicePool`]'s cursor, and needed for the
    /// same reason: a stretch filter's FIFOs keep draining pre-jump material
    /// until something clears them.
    ///
    /// `None` when the voice has no clock to watch (free-running / unplaced).
    pub(crate) cursor: Option<BeatCursor>,
    /// Commands from the control thread, drained at the top of every block.
    ///
    /// **A dead channel when the node was built without one**, not an `Option`:
    /// every constructor that predates this one keeps its signature and gets a
    /// `bounded(0)` receiver, so the drain is one `try_recv` that immediately
    /// answers `Empty` rather than a branch on every block. `VoicePool` makes the
    /// same choice for [`detached`](super::pool::VoicePool::detached).
    pub(crate) rx: Receiver<VoiceCommand>,
}

impl VoiceNode {
    /// Wrap a single [`Voice`] as a standalone **stereo** graph node. Builds the
    /// resident stretch processor once (like a mixer slot), off any hot path.
    pub fn new(voice: Voice) -> Self {
        Self::with_channels(voice, ChannelLayout::STEREO)
    }

    /// Wrap a single [`Voice`] as a `channels`-wide graph node.
    ///
    /// Builds the stretch filter here when the voice asks for one. This is a
    /// control-thread constructor, so the allocation is free; the audio-thread
    /// drain in [`VoicePool`] cannot do the same and takes a pre-built filter
    /// from the sender instead.
    ///
    /// It did not used to build one — `VoiceSlot::with_channels` always sets
    /// `stretch: None`, and the doc here claimed a filter was built "once, like a
    /// mixer slot" while nothing ever built it. A standalone stretched voice
    /// therefore read DRY forever: no stretch, no pitch shift, and no error. Both
    /// doc comments pointed at a `materialize_stretch` that does not exist.
    pub fn with_channels(voice: Voice, channels: impl Into<ChannelLayout>) -> Self {
        let channels = nonempty(channels.into());
        let sample_rate = SampleRate::SR_44K1;
        let stretch = stretch_wanted(&voice.play).then(|| {
            let unit = stretch::Unit::with_channels(sample_rate, channels);
            unit.set_stretch_factor(voice.play.stretch);
            unit.set_pitch_cents(voice.play.pitch);
            unit
        });
        let mut slot = VoiceSlot::with_channels(SlotId(0), voice, sample_rate, channels);
        slot.stretch = stretch;
        let cursor = slot
            .voice
            .source
            .timeline()
            .map(|t| BeatCursor::new(t, sample_rate));
        Self {
            slot,
            channels,
            cursor,
            // Channel-less: this constructor and its callers predate the command
            // channel and have no handle to give out. See
            // [`with_commands`](Self::with_commands).
            rx: bounded(0).1,
        }
    }

    /// As [`with_channels`](Self::with_channels), plus a command channel.
    ///
    /// Returns the node **and** its control-thread handle, the same
    /// `(unit, handle)` shape [`VoicePool::with_transport`] uses — because the
    /// receiver has to live inside the unit (the audio thread drains it) while
    /// the sender has to live outside it (the control thread fills it), and a
    /// constructor is the only place both ends exist at once.
    ///
    /// # What the channel is for, and what it is not
    ///
    /// One control: **placement**. `AudioUnit::set` already carries every scalar
    /// a voice exposes, and [`VoiceNode::set`] is where those belong. A
    /// placement cannot go there — `Setting` is one `f32` wide and a window is a
    /// `Beat` (f64) plus an optional duration — so it takes the tier the crate
    /// reserves for multi-field state, which is this queue.
    ///
    /// A node built with [`with_channels`] instead keeps working exactly as
    /// before; it simply has no way to be told to move.
    pub fn with_commands(
        voice: Voice,
        channels: impl Into<ChannelLayout>,
    ) -> (Self, VoiceNodeHandle) {
        let (tx, rx) = bounded(COMMAND_CAPACITY);
        let mut node = Self::with_channels(voice, channels);
        node.rx = rx;
        (node, VoiceNodeHandle { tx })
    }

    /// Apply every queued command. Runs at the top of each block.
    ///
    /// Allocation-free by construction: the only command a node accepts carries
    /// two `Copy` scalars, and applying it is a field write plus a re-arm of the
    /// source's own gate. The pool's drain has to be more careful because
    /// `AddVoice` moves a whole `Voice` — which is why its handle builds the
    /// stretch filter on the *control* thread before sending.
    ///
    /// Commands a node has no meaning for are ignored rather than rejected: the
    /// wire format is shared with the pool, and a host that reaches for
    /// `AddVoice` on a single-voice node is asking for something that does not
    /// exist. Silently is the right register here — the alternative is a panic
    /// in an audio callback.
    #[inline]
    fn drain_commands(&mut self) {
        while let Ok(cmd) = self.rx.try_recv() {
            if let VoiceCommand::UpdatePlacement {
                start_beat,
                duration_beats,
                ..
            } = cmd
            {
                // Both halves, for the reason `set` gives for gain: `Playback` is
                // the intent record a rebind reads back, the source owns the gate
                // the DSP consults. Except placement has no `Playback` field —
                // it was deleted precisely because keeping two copies in sync by
                // hand is how a rebind reaches the wrong one. So there is exactly
                // one write, and `voice/types.rs` records why.
                self.slot
                    .voice
                    .source
                    .apply_placement(start_beat, duration_beats);
            }
        }
    }

    /// Flush every voice's buffered audio when the playhead jumps.
    ///
    /// The standalone counterpart of [`VoicePool::flush_on_seek`]; see there for
    /// why detection has to be stateful and per-observer.
    #[inline]
    fn flush_on_seek(&mut self, block_size: usize) {
        let Some(cursor) = &self.cursor else { return };
        let Some((_, sync)) = cursor.advance(block_size) else {
            return;
        };
        if sync.is_discontinuous() {
            self.slot.flush_playhead_state();
        }
    }

    /// Output width — this node's `outputs()`.
    pub fn channels(&self) -> ChannelLayout {
        self.channels
    }

    /// The wrapped voice (immutable view).
    pub fn voice(&self) -> &Voice {
        &self.slot.voice
    }

    /// The wrapped voice (mutable view) — used by the offline render to rebind
    /// the transport via [`Voice::replace_transport`].
    pub fn voice_mut(&mut self) -> &mut Voice {
        &mut self.slot.voice
    }

    /// Rebind the transport clock behind the wrapped voice's placement,
    /// preserving start / duration. Convenience delegate to
    /// [`Voice::replace_transport`] so the offline region render can rebind a
    /// standalone voice node without reaching through `voice_mut`.
    pub fn replace_transport(&mut self, transport: Arc<dyn Timeline>) {
        self.slot.voice.replace_transport(transport);
    }
}

// Hand-rolled: wraps a non-`Debug` `VoiceSlot`. Print the wrapped `Voice`
// (which is `Debug`) and leave the resident stretch DSP out.
impl std::fmt::Debug for VoiceNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceNode")
            .field("voice", &self.slot.voice)
            .finish_non_exhaustive()
    }
}

impl From<Voice> for VoiceNode {
    fn from(voice: Voice) -> Self {
        Self::new(voice)
    }
}

impl Clone for VoiceNode {
    fn clone(&self) -> Self {
        Self {
            slot: VoiceSlot {
                id: self.slot.id,
                voice: self.slot.voice.clone(),
                stretch: self.slot.stretch.clone(),
                channels: self.slot.channels,
                sample_rate: self.slot.sample_rate,
            },
            channels: self.channels,
            // Shares the underlying `last_beat`, as `BeatCursor::clone` does for
            // the pool: fundsp deep-clones every node on `Net::commit`, and a
            // fresh cursor would read its first block as a discontinuity and
            // flush the filter on every graph edit.
            cursor: self.cursor.clone(),
            // **The same `Receiver`, not a fresh one.** `AudioUnit: DynClone`, so
            // `Net::commit` clones this node, and `migrate` installs the clone
            // over the backend's unit whenever the vertex is marked *changed* —
            // which `set_sample_rate`, `reset`, `isolate` and `rebind_offline`
            // all do, and the first of those on every device-rate change. A
            // clone that minted its own channel would then be handed to the
            // audio thread already deaf, and every command the live handle sent
            // after that would vanish — no error, no diagnostic, exactly the
            // silent class of failure this line of work exists to close.
            //
            // (An *unchanged* vertex keeps the backend's own unit, so a bare
            // commit does not exercise this. That is why
            // `a_command_reaches_a_node_across_a_commit` sets a sample rate:
            // without it the test passed with this line sabotaged.)
            //
            // Sharing is safe because crossbeam delivers each message to exactly
            // one receiver and the graph ticks one instance at a time, so there
            // is no double-drain. `VoicePool::clone` reaches the same conclusion
            // for the same reason and states it at length.
            //
            // The offline render is the case where "exactly one receiver" bites
            // rather than helps — see [`AudioUnit::isolate`].
            rx: self.rx.clone(),
        }
    }
}

impl AudioUnit for VoiceNode {
    fn inputs(&self) -> usize {
        0
    }

    fn outputs(&self) -> usize {
        // Boundary: `AudioUnit::outputs` is a fixed fundsp trait signature.
        self.channels.count() as usize
    }

    fn reset(&mut self) {
        // One definition shared with the per-block seek check, so a seek can
        // never flush less than a reset does — see `VoiceSlot::flush_playhead_state`.
        self.slot.flush_playhead_state();
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.slot
            .voice
            .source
            .as_audio_unit_mut()
            .set_sample_rate(sample_rate);
        self.slot.sample_rate = sample_rate;
        if let Some(unit) = &mut self.slot.stretch {
            unit.set_sample_rate(sample_rate);
        }
        // The cursor derives its forward-jump slack from the rate, so a stale one
        // makes the threshold wrong — mirrors `VoicePool::set_sample_rate`.
        if let Some(cursor) = &mut self.cursor {
            cursor.set_sample_rate(sample_rate.get());
        }
    }

    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        // **Before the early return below.** A zero-width call still has to
        // accept commands, or a caller that happens to pass an empty frame
        // silently swallows the user's edit and the queue backs up.
        self.drain_commands();
        // Same single-voice read the mixer runs per slot, straight into the
        // caller's frame.
        // Stride derived once, above the read — never inside a loop.
        let n = (self.channels.count() as usize).min(output.len());
        if n == 0 {
            return;
        }
        self.flush_on_seek(1);
        self.slot.tick_frame_into(&mut output[..n]);
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        self.drain_commands();
        // Stride derived once per block, above the loops.
        let n = (self.channels.count() as usize)
            .min(output.channels())
            .min(MAX_SAMPLER_CHANNELS);
        for c in 0..n {
            for i in 0..size {
                output.set_f32(c, i, 0.0);
            }
        }
        self.flush_on_seek(size.max(1));
        self.slot.process_into(size, n, output);
    }

    audio_unit_boilerplate!(id = crate::node_id::VOICE_NODE_ID);

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        // Width must track `outputs()` or fundsp mis-plans this node's latency.
        // Boundary: `SignalFrame::new` is a fundsp signature.
        SignalFrame::new(self.channels.count() as usize)
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }

    /// Size the resident stretch filter's block scratch — see
    /// [`VoicePool::allocate`], which this mirrors for the single-voice node.
    fn allocate(&mut self) {
        if let Some(s) = self.slot.stretch.as_mut() {
            s.allocate();
        }
    }

    /// Sever every input this clone shares with the live graph.
    ///
    /// **Required, not an optimization.** The offline region render clones the
    /// live net and ticks it on a worker pool *while the audio thread plays the
    /// original* — the one place two generations run genuinely concurrently. A
    /// clone that still shares state with the live node is a data race, not
    /// merely an interleave.
    ///
    /// [`VoicePool::isolate`] gets this for free by clearing its voices, which
    /// drops their stretch filters with them. `VoiceNode` keeps its single slot,
    /// so it has to sever explicitly — and until it did, it inherited the
    /// `AudioUnit` no-op default and shipped the render a filter still pointing
    /// at the live [`stretch::Unit`]'s shared vocoder bank.
    ///
    /// Latent rather than firing only because the sole producer of these nodes
    /// (`dawai-spectral`'s resynth) builds them at unity, where
    /// `stretch_wanted` leaves the slot's filter `None`. A resynth voice with
    /// any non-unity stretch or pitch arms it, with no change in this crate.
    fn isolate(&mut self) {
        if let Some(s) = self.slot.stretch.as_mut() {
            s.isolate();
        }
        // The wrapped voice too: a disk-backed one shares the live ring and
        // control cell through `Clone`, and a voice inside a slot is not a graph
        // vertex, so the net-wide walk never reaches it.
        self.slot.voice.isolate();
        // A fresh cursor: the clone must not inherit the live playhead's
        // last-seen beat, or its first offline block reads as a discontinuity.
        self.cursor = None;
        // **And the command channel.** `Clone` deliberately shares the receiver
        // (see there), which is right for the frontend↔backend swap and wrong
        // for a render: crossbeam delivers each message to exactly one receiver,
        // so a worker draining this channel would *steal* the user's edits from
        // the audio thread. The live voice would then miss a placement move with
        // nothing logged anywhere.
        //
        // `VoicePool` cannot fix this here — its render path replaces each node
        // with a channel-less `detached` one in a Prepare step, because by the
        // time `isolate` runs the pool has already been cloned with live voices
        // in it. A `VoiceNode` has no such staging step and nothing to empty, so
        // severing in place is both sufficient and the simpler half of the same
        // rule. `a_render_clone_steals_no_commands` pins it.
        self.rx = bounded(0).1;
    }

    /// Rebind the wrapped voice's placement (and its source's own read clock) to
    /// the render's transport.
    ///
    /// A bare voice node keeps its cloned content — the clone is already
    /// independent — so this is purely the re-point. Without it the voice reads
    /// the live playhead, which the offline driver never advances, and renders
    /// silence.
    fn rebind_offline(&mut self, ctx: &dyn core::any::Any) {
        // Delegated rather than unwrapped-then-`replace_transport`: a disk voice
        // needs the whole context, not just a clock, and `Voice::rebind_offline`
        // is what knows which arm it is.
        self.slot.voice.rebind_offline(ctx);
    }
}
