//! [`VoiceNode`] — a single [`Voice`] as its own graph node.
//!
//! Zero inputs, N outputs, and typed controls ([`VoiceNodeHandle`], what its
//! `IntoNode` hands back): its gain, a scalar cell the node reads once per
//! block, and its placement, a published window it reads once per block. For resynth, preview and a single timeline clip, where a
//! whole pool would be ceremony. It shares `PlaybackSlot` with
//! [`VoicePool`](super::pool::VoicePool), so the per-voice read is the same
//! code in both — a fix in one is a fix in both.

use std::sync::{Arc, PoisonError};

use crate::lanes::{BlockScratch, UNPREPARED_FRAMES};
use crate::stretch;
use crate::{nonempty, MAX_SAMPLER_CHANNELS};

use super::clock::{BlockClock, Clock};
use super::command::{PlacedWindow, PlacementCell, VoiceNodeHandle};
use super::disk_voice::VoiceHealth;
use super::slot::{stretch_wanted, PlaybackSlot};
use super::types::{SlotId, Voice};
use super::VoiceWindow;
use tutti_core::{Amplitude, ArcLend, ChannelLayout, Param, Reader, SampleRate, Tail, UnitParam};
use tutti_graph::{
    Cx, ForkCause, ForkMode, ForkSource, Forked, IntoNode, Io, Node, NodeParts, ParamSet, Prepare,
    Shape, Status,
};

/// One [`Voice`] as a graph node in its own right: zero inputs, `channels`
/// outputs.
///
/// Where [`VoicePool`](super::pool::VoicePool) holds a *list* of voices and sums
/// them, this holds exactly ONE and plays it — a degenerate single-voice
/// mixdown. Its block read is the same per-voice read the pool does for one
/// slot, shared through the slot itself, so there is no duplication and no
/// per-sample dynamic dispatch.
///
/// # Controls
///
/// Inserted into a graph (`IntoNode`), it hands back a [`VoiceNodeHandle`]:
///
/// - **gain** — a `Param<Amplitude>` cell, also addressable as
///   [`UnitParam::Volume`] through the handle's [`ParamSet`]. The node reads
///   it once per block into the voice's `Playback` record and its source.
/// - **placement** — a [`Beat`](tutti_core::Beat) and an optional duration:
///   not a scalar, so the handle publishes it to an `ArcLend` cell the
///   node reads once per block, latest wins, allocation-free.
///
/// # Forks
///
/// The node's fork source keeps a copy of the voice as it was inserted, and a
/// fork is that copy made to share nothing with the live node
/// (`Voice::fork_copy`: its own gain cell; a disk voice reading its file
/// itself), moved to the placement the handle last published and set to the gain
/// last set, with a stretch filter of its own. It reads its render's
/// transport from its block's `Env`: nothing to rebind. A disk voice's node
/// forks only for an offline render.
pub struct VoiceNode {
    /// The single voice plus its resident stretch filter and the per-sample
    /// read, shared with the pool's slots.
    pub(crate) slot: PlaybackSlot,
    /// Output width — the node's audio outputs, fixed at construction.
    /// Declared rather than inferred from the voice, so an edge wired against
    /// it cannot be re-arityed under a live graph.
    pub(crate) channels: ChannelLayout,
    /// The window the handle publishes, read once per block; the fork source
    /// reads the same cell. Empty (`None`) until a handle sets one, so a node
    /// driven by hand keeps the window it was built with.
    placement: PlacementCell,
    /// A reader of `placement`, registered whenever the cell is set (here,
    /// and in [`with_handle`](Self::with_handle)): what the block lends
    /// through.
    placement_reader: Reader<Option<VoiceWindow>>,
    /// The window last applied from `placement`, so a block applies a
    /// window once, when it changes, rather than every block.
    applied: Option<VoiceWindow>,
    /// The gain the handle writes; read once per block.
    gain: Param<Amplitude>,
    /// The transport as the voice reads it, kept across blocks so a jump is
    /// flushed where it happens.
    clock: Clock,
    /// The block read's lanes (`PlaybackSlot::render_into`), built here and
    /// sized from `Prepare::max_block` in `prepare`, on the control thread.
    scratch: BlockScratch,
}

impl VoiceNode {
    /// Wraps a single [`Voice`] as a standalone **stereo** graph node. Builds the
    /// resident stretch processor once (like a mixer slot), off any hot path.
    pub fn new(voice: Voice) -> Self {
        Self::with_channels(voice, ChannelLayout::STEREO)
    }

    /// Wraps a single [`Voice`] as a `channels`-wide graph node.
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
        // `prepare` re-rates the filter to the graph's rate.
        let sample_rate = SampleRate::SR_44K1;
        let stretch = stretch_wanted(&voice.play).then(|| {
            let unit = stretch::Unit::with_channels(sample_rate, channels);
            unit.set_stretch_factor(voice.play.stretch);
            unit.set_pitch_cents(voice.play.pitch);
            unit
        });
        let gain = Param::new(voice.play.gain);
        let mut slot =
            PlaybackSlot::with_channels(SlotId(0), Box::new(voice), sample_rate, channels);
        slot.stretch = stretch;
        let placement = PlacementCell::default();
        Self {
            slot,
            channels,
            placement_reader: placement.reader(),
            placement,
            applied: None,
            gain,
            clock: Clock::new(),
            scratch: BlockScratch::new(UNPREPARED_FRAMES, channels.count() as usize),
        }
    }

    /// This node with a fresh placement cell, and the handle that drives
    /// it: what [`into_parts`](IntoNode::into_parts) hands the graph and the
    /// caller. For a host (or a test) that calls the node by hand. A
    /// placement set through a handle taken before no longer reaches the
    /// node (its gain still does: that cell is the node's for life).
    pub fn with_handle(mut self) -> (Self, VoiceNodeHandle) {
        self.placement = Arc::new(ArcLend::new(None));
        self.placement_reader = self.placement.reader();
        let params = self.param_set();
        let handle = VoiceNodeHandle {
            placement: Arc::clone(&self.placement),
            placed: PlacedWindow::default(),
            gain: self.gain.handle(),
            params,
        };
        (self, handle)
    }

    /// The node's params by address: its gain, as [`UnitParam::Volume`].
    /// The cells the node reads, so a set taken before the node is inserted
    /// addresses the inserted node (it is the set the handle carries).
    pub fn param_set(&self) -> ParamSet {
        ParamSet::builder()
            .param(UnitParam::Volume, self.gain.as_atomic())
            .build()
    }

    /// Apply the published placement, and the gain. Runs at the top of each
    /// block.
    ///
    /// Allocation- and free-free by construction: one `ArcLend` read (a
    /// borrow that cannot free, dropped before the block's read), a `Copy`
    /// window, and a field write when it changed. The pool's drain has to be
    /// more careful because its commands carry owned payloads (a `Voice`, a
    /// wave, a stretch filter), which is why it retires what it lets go of.
    /// A node takes no such payload: there is nothing for it to drop.
    ///
    /// `driven` is the block's gain from the node's param port when a lane
    /// drives it (its last frame, [`VOICE_PARAMS`]); `None` reads the cell.
    #[inline]
    fn drain_commands(&mut self, driven: Option<f32>) {
        let published = *self.placement_reader.lend();
        if published != self.applied {
            if let Some(window) = published {
                // Placement is the source's alone: `Playback` has no
                // placement field (two copies kept in sync by hand is how a
                // fork reaches the wrong one), so there is exactly one write.
                self.slot
                    .voice
                    .source
                    .apply_placement(window.start, window.duration);
            }
            self.applied = published;
        }
        // Both halves of the gain: `Playback` is what the memory tier's read
        // scales by, the source's own cell what the disk tier's does.
        let gain = driven.map_or_else(|| self.gain.load(), |v| Amplitude(v.max(0.0)));
        if gain != self.slot.voice.play.gain {
            self.slot.voice.play.gain = gain;
            self.slot.voice.source.apply_gain(gain);
        }
    }

    /// Output width — the node's audio outputs.
    pub fn channels(&self) -> ChannelLayout {
        self.channels
    }

    /// The wrapped voice (immutable view).
    pub fn voice(&self) -> &Voice {
        &self.slot.voice
    }

    /// The wrapped voice (mutable view), for a caller building the node
    /// before it goes into a graph.
    pub fn voice_mut(&mut self) -> &mut Voice {
        &mut self.slot.voice
    }

    /// A block longer than the scratch, in pieces as long as it: only a
    /// node driven by hand without a `prepare` (its `UNPREPARED_FRAMES`) or
    /// past the `max_block` it was prepared for reaches this.
    #[cold]
    #[inline(never)]
    fn render_in_pieces(
        &mut self,
        clock: &BlockClock<'_>,
        frames: usize,
        n: usize,
        out: &mut [&mut [f32]],
    ) {
        let piece = self.scratch.frames();
        let mut from = 0;
        while from < frames {
            let to = (from + piece).min(frames);
            self.slot
                .render_into(clock, from..to, n, &mut self.scratch, out);
            from = to;
        }
    }

    /// What the node declares to the cost model: `Moderate` for a plain
    /// read, `Heavy` with a time-stretch filter.
    ///
    /// Measured (doc 013, "The cost model and coarsening"): a plain memory voice 13 ns
    /// per frame and channel, a stretched one 288 (the phase vocoder),
    /// against `SvfFilterNode`'s 7.2 and `Heavy`'s 256. A node's filter is
    /// built with it or never (`UpdateStretch` reaches only a pool's
    /// slots), so every read of the shape (at insert and at each
    /// re-prepare) gives the same class. A disk voice reads through the
    /// same kernel as a memory one.
    fn cost(&self) -> tutti_graph::Cost {
        if self.slot.stretch.is_some() {
            tutti_graph::Cost::Heavy
        } else {
            tutti_graph::Cost::Moderate
        }
    }

    /// Its scratch's frames and width.
    #[cfg(test)]
    pub(crate) fn scratch_size(&self) -> (usize, usize) {
        (self.scratch.frames(), self.scratch.width())
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

/// The param a [`VoiceNode`] lets the graph modulate: its gain, as
/// [`UnitParam::Volume`]. Block-rate in the voice (read once per block, as
/// its cell is), so a driven port is read the same way: its value on the
/// block's last frame.
pub const VOICE_PARAMS: [UnitParam; 1] = [UnitParam::Volume];

impl Node for VoiceNode {
    /// No inputs, [`channels`](Self::channels) outputs; a generator, never
    /// skipped.
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, self.channels)
            .with_tail(Tail::Unbounded)
            .with_cost(self.cost())
            .with_params(&VOICE_PARAMS)
    }

    /// The rate the voice's source and its stretch filter run at, and the
    /// block scratch sized from `max_block` (a re-prepare at another size
    /// resizes it).
    fn prepare(&mut self, p: &Prepare) {
        self.scratch
            .resize(p.max_block().get(), self.channels.count() as usize);
        let sample_rate = p.sample_rate();
        self.slot.voice.source.prepare_rate(sample_rate);
        self.slot.sample_rate = sample_rate;
        if let Some(unit) = &mut self.slot.stretch {
            unit.set_sample_rate(sample_rate);
        }
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        self.drain_commands(io.param(0).frames().and_then(|v| v.last().copied()));
        let clock = self.clock.observe(cx.env);
        let frames = io.frames();
        let (_, mut outs) = io.split();
        // Every output this node declares, not only the `n` the read writes:
        // a node wider than `MAX_SAMPLER_CHANNELS` leaves the rest silent
        // rather than holding whatever the buffer held.
        for ch in outs.iter_mut() {
            ch.fill(0.0);
        }
        let n = (self.channels.count() as usize)
            .min(outs.len())
            .min(MAX_SAMPLER_CHANNELS);
        let mut refs: [&mut [f32]; MAX_SAMPLER_CHANNELS] =
            std::array::from_fn(|_| Default::default());
        for (slot, ch) in refs.iter_mut().zip(outs.iter_mut()) {
            *slot = ch;
        }
        // One piece: in a graph, `prepare` sized the scratch for every block.
        if frames <= self.scratch.frames() {
            self.slot
                .render_into(&clock, 0..frames, n, &mut self.scratch, &mut refs[..n]);
        } else {
            self.render_in_pieces(&clock, frames, n, &mut refs[..n]);
        }
        Status::Modified
    }

    /// The voice's buffered audio flushed (one definition shared with a
    /// jump's flush, so a jump can never flush less than a reset does), the
    /// transport forgotten.
    fn reset(&mut self) {
        self.slot.flush_playhead_state();
        self.clock.reset();
    }

    fn param_base(&self, k: usize) -> Option<f32> {
        (k == 0).then(|| self.gain.load().get())
    }
}

/// A voice node's fork source: see "Forks" on [`VoiceNode`].
struct VoiceNodeFork {
    /// The voice as inserted. Shares with the live node what a `Voice`
    /// clone shares (a memory voice's gain cell, a disk voice's stream
    /// record and control cell, never its ring reader); a fork copies it
    /// with `Voice::fork_copy`, which shares nothing.
    template: Voice,
    channels: ChannelLayout,
    /// The window the live node's handle last published, read when a fork
    /// is taken (the control side's record, never the node's cell).
    placed: PlacedWindow,
    /// The node's params: a fork starts from the gain cell, which holds the
    /// gain last **set** (a modulation lane sends its composite to the
    /// port and never writes the cell).
    params: ParamSet,
}

impl ForkSource for VoiceNodeFork {
    fn fork(&self, mode: ForkMode<'_>) -> Result<Forked, ForkCause> {
        let mut voice = self.template.fork_copy(mode)?;
        let placed = *self.placed.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(window) = placed {
            voice.source.apply_placement(window.start, window.duration);
        }
        let gain = self
            .params
            .get(UnitParam::Volume)
            .map_or(voice.play.gain, Amplitude);
        voice.play.gain = gain;
        voice.source.apply_gain(gain);
        let health = voice.fault().map(VoiceHealth);
        let node = VoiceNode::with_channels(voice, self.channels);
        let forked = Forked::new(Box::new(node));
        Ok(match health {
            Some(h) => forked.with_health(Arc::new(h)),
            None => forked,
        })
    }
}

impl IntoNode for VoiceNode {
    type Controls = VoiceNodeHandle;

    fn into_parts(self) -> NodeParts<VoiceNodeHandle> {
        let (node, handle) = self.with_handle();
        let fork = VoiceNodeFork {
            template: node.slot.voice.as_ref().clone(),
            channels: node.channels,
            placed: Arc::clone(&handle.placed),
            params: handle.params.clone(),
        };
        NodeParts {
            node: Box::new(node),
            controls: handle,
            fork: Some(Box::new(fork)),
        }
    }
}
