//! [`VoiceNode`] — a single [`Voice`] as its own graph node.
//!
//! Zero inputs, N outputs, and — via [`VoiceNode::with_commands`] — a command
//! channel for the one control `AudioUnit::set` cannot carry. For resynth,
//! preview and a single timeline clip, where a whole pool would be ceremony. It
//! shares `PlaybackSlot` with [`VoicePool`](super::pool::VoicePool), so the
//! per-voice read is the same code in both — a fix in one is a fix in both.

use std::sync::Arc;

use crate::stretch;
use crate::{nonempty, MAX_SAMPLER_CHANNELS};

use super::command::{VoiceCommand, VoiceNodeHandle, COMMAND_CAPACITY};
use super::slot::{stretch_wanted, PlaybackSlot};
use super::types::{SlotId, Voice};
use crossbeam_channel::{bounded, Receiver};
use tutti_core::transport::BeatCursor;
use tutti_core::{
    AudioUnit, BufferMut, BufferRef, ChannelLayout, SampleRate, SignalFrame, Timeline,
};

/// One [`Voice`] as a graph node in its own right: zero inputs, `channels`
/// outputs.
///
/// Where [`VoicePool`](super::pool::VoicePool) holds a *list* of voices and sums
/// them, this holds exactly ONE and plays it — a degenerate single-voice
/// mixdown. Its `tick` / `process` are the same per-voice read the pool does for
/// one slot, shared through the slot itself, so there is no duplication and no
/// per-sample dynamic dispatch.
///
/// Two consumers depend on a voice being an `AudioUnit` on its own rather than
/// only reachable through the pool: `dawai-spectral`'s resynth adds a bare voice
/// node to its net, and `tutti-export`'s region render downcasts these nodes to
/// rebind their transport offline.
pub struct VoiceNode {
    /// The single voice plus its resident stretch filter and the per-sample
    /// read, shared with the pool's slots.
    pub(crate) slot: PlaybackSlot,
    /// Output width — this node's `outputs()`, fixed at construction. Declared
    /// rather than inferred from the voice, so a `Net` edge wired against
    /// `outputs()` cannot be re-arityed under a live graph.
    pub(crate) channels: ChannelLayout,
    /// Detects transport discontinuities so buffered audio can be flushed on a
    /// seek — the standalone twin of the pool's cursor, and needed for the same
    /// reason: a stretch filter's FIFOs keep draining pre-jump material until
    /// something clears them.
    ///
    /// `None` when the voice has no clock to watch (free-running / unplaced).
    pub(crate) cursor: Option<BeatCursor>,
    /// Commands from the control thread, drained at the top of every block.
    ///
    /// **A dead channel when the node was built without one**, not an `Option`:
    /// a node built through [`new`](VoiceNode::new) or
    /// [`with_channels`](VoiceNode::with_channels) gets a `bounded(0)` receiver,
    /// so the drain is one `try_recv` that immediately answers `Empty` rather
    /// than a branch on every block. `VoicePool` makes the same choice for
    /// [`detached`](super::pool::VoicePool::detached).
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
    /// **Builds the stretch filter here** when `voice.play` asks for one. This
    /// is a control-thread constructor, so the allocation is free; the
    /// audio-thread drain in [`VoicePool`](super::pool::VoicePool) cannot do the
    /// same and takes a pre-built filter from the sender instead.
    ///
    /// Skipping that step is how a standalone stretched voice reads DRY forever
    /// — no stretch, no pitch shift, and no error — because
    /// `PlaybackSlot::with_channels` always leaves the field `None` and nothing
    /// else on this path fills it in.
    pub fn with_channels(voice: Voice, channels: impl Into<ChannelLayout>) -> Self {
        let channels = nonempty(channels.into());
        let sample_rate = SampleRate::SR_44K1;
        let stretch = stretch_wanted(&voice.play).then(|| {
            let unit = stretch::Unit::with_channels(sample_rate, channels);
            unit.set_stretch_factor(voice.play.stretch);
            unit.set_pitch_cents(voice.play.pitch);
            unit
        });
        let mut slot = PlaybackSlot::with_channels(SlotId(0), voice, sample_rate, channels);
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
    /// `(unit, handle)` shape
    /// [`VoicePool::with_transport`](super::pool::VoicePool::with_transport)
    /// uses — because the receiver has to live inside the unit (the audio thread
    /// drains it) while the sender has to live outside it (the control thread
    /// fills it), and a constructor is the only place both ends exist at once.
    ///
    /// # What the channel is for, and what it is not
    ///
    /// One control: **placement**. `AudioUnit::set` already carries every scalar
    /// a voice exposes, and [`VoiceNode::set`] is where those belong. A
    /// placement cannot go there — `Setting` is one `f32` wide and a window is a
    /// `Beat` (f64) plus an optional duration — so it takes the tier the crate
    /// reserves for multi-field state, which is this queue.
    ///
    /// A node built with [`with_channels`](Self::with_channels) instead plays
    /// identically; it simply has no way to be told to move.
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

// Hand-rolled: wraps a non-`Debug` `PlaybackSlot`. Print the wrapped `Voice`
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
            slot: PlaybackSlot {
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
        // never flush less than a reset does — see `PlaybackSlot::flush_playhead_state`.
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

    /// Size the resident stretch filter's block scratch.
    ///
    /// **Required, not an optimization**, for the reason
    /// [`VoicePool`](super::pool::VoicePool)'s own `allocate` gives: a cloned
    /// node reaches the audio thread with unsized scratch, and `process` then
    /// has to allocate in the callback to avoid rendering silence.
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
    /// [`VoicePool`](super::pool::VoicePool)'s `isolate` gets this for free by
    /// clearing its voices, which drops their stretch filters with them.
    /// `VoiceNode` keeps its single slot, so it has to sever explicitly:
    /// inheriting the `AudioUnit` no-op default ships the render a filter still
    /// pointing at the live [`stretch::Unit`]'s shared vocoder bank.
    ///
    /// The hazard is latent only because the sole producer of these nodes
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

    /// The host's door to a live voice's scalar controls.
    ///
    /// # Why this exists, when `voice_mut` already did
    ///
    /// It did not reach: [`Voice`]'s per-tier fan-out (`apply_gain` and its
    /// siblings) is `pub(crate)`, so a host outside this crate could hold a
    /// `&mut Voice` and still have no way to set its gain. The alternatives were
    /// to make the sampler's internal fan-out public — exporting a
    /// `VoiceSource`-shaped API nobody outside asked for — or to use the door
    /// every other unit in the engine already has. This is the latter, and it is
    /// what makes a voice addressable by `Net::set` / `write_param` like a
    /// filter or a strip, rather than needing its own vocabulary.
    ///
    /// # Why a `&mut self` write is safe here, when the live-value rule says otherwise
    ///
    /// It looks like it should not be. `Net`'s frontend holds clones, and a
    /// control stored **by value** cannot normally be changed on a live node —
    /// the write lands on a copy the next commit discards. `play.gain` is a
    /// plain `Copy` field, so by that rule this should be lost.
    ///
    /// It is not, because **`set` is not written through the frontend at all.**
    /// With a backend attached, `Net::set` *enqueues* the setting and the audio
    /// thread applies it to the copy it is rendering (`net.rs`: `if let
    /// Some((sender, _)) = &mut self.front`). The rule governs `node_as_mut`,
    /// which mutates the frontend; the setting path sidesteps it.
    ///
    /// That distinction is measurable and was measured: reverting
    /// `MemorySource::gain` to an unshared clone leaves every test in
    /// `tests/voice_gain_survives_commit.rs` **passing**, because a `VoiceNode`
    /// renders through `slot.voice.play.gain` (see `PlaybackSlot::tick_frame_into`)
    /// and never consults the source's own cell on this path.
    ///
    /// # Which is why both are written
    ///
    /// `play.gain` is what this node renders. `apply_gain` is what a
    /// [`VoicePool`](super::pool::VoicePool) slot and the offline render read,
    /// and what survives a rebind. Writing one and not the other leaves two
    /// copies disagreeing —
    /// the hazard `Playback.placement` was deleted for. Sabotaging the
    /// `play.gain` half fails two tests; that is the half this node's audio
    /// depends on.
    ///
    /// **Do not read this as "any control can have an arm here."** `speed`,
    /// `direction` and the placement window are deliberately absent: they are
    /// reached through paths that *do* go via the frontend, where by-value
    /// storage is exactly the silent-loss hazard. Moving one of those here means
    /// checking its storage first, not copying this arm.
    ///
    /// Params this node does not own are ignored, which is the convention that
    /// lets a host push a setting without dispatching on node type.
    fn set(&mut self, setting: tutti_core::dsp::Setting) {
        let Some((param, value)) = tutti_core::unit_param::from_setting(&setting) else {
            return;
        };
        if param == tutti_core::UnitParam::Volume {
            let gain = tutti_core::Amplitude(value);
            // Both: the `Playback` record is the control-*intent* the offline
            // render and the pool read back, and the source is what the DSP
            // reads. Writing only the source would leave a rebind restoring the
            // old value — the same two-copies hazard `Playback.placement` was
            // deleted for.
            self.slot.voice.play.gain = gain;
            self.slot.voice.source.apply_gain(gain);
        }
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
