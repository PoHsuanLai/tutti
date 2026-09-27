//! [`VoiceNode`] — a single [`Voice`] as its own graph node.
//!
//! Zero inputs, N outputs, and typed controls ([`VoiceNodeHandle`], what its
//! `IntoNode` hands back): its gain, a scalar cell the node reads once per
//! block, and its placement, over a command queue the node drains at the top
//! of each block. For resynth, preview and a single timeline clip, where a
//! whole pool would be ceremony. It shares `PlaybackSlot` with
//! [`VoicePool`](super::pool::VoicePool), so the per-voice read is the same
//! code in both — a fix in one is a fix in both.

use std::sync::Arc;

use crate::lanes::{BlockScratch, LANE_FRAMES};
use crate::stretch;
use crate::{nonempty, MAX_SAMPLER_CHANNELS};

use super::clock::Clock;
use super::command::{PlacementRecord, VoiceCommand, VoiceNodeHandle, COMMAND_CAPACITY};
use super::disk_voice::VoiceHealth;
use super::slot::{stretch_wanted, PlaybackSlot};
use super::types::{SlotId, Voice};
use crossbeam_channel::{bounded, Receiver};
use tutti_core::{Amplitude, ChannelLayout, Param, SampleRate, Tail, UnitParam};
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
///   not a scalar, so it rides a bounded command queue the node owns and
///   drains at the top of each block, allocation-free.
///
/// # Forks
///
/// The node's fork source keeps a copy of the voice as it was inserted, and a
/// fork is that copy made to share nothing with the live node
/// (`Voice::fork_copy`: its own gain cell; a disk voice reading its file
/// itself), moved to the placement the handle last queued and set to the gain
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
    /// Commands from the control thread, drained at the top of every block.
    ///
    /// **A dead channel until a handle exists**, not an `Option`: the drain
    /// is one `try_recv` that immediately answers `Empty` rather than a
    /// branch on every block.
    pub(crate) rx: Receiver<VoiceCommand>,
    /// The gain the handle writes; read once per block.
    gain: Param<Amplitude>,
    /// The last placement the handle queued, for the fork source.
    placement: PlacementRecord,
    /// The transport as the voice reads it, kept across blocks so a jump is
    /// flushed where it happens.
    clock: Clock,
    /// The block read's lanes (`PlaybackSlot::render_into`), built here, on
    /// the control thread.
    scratch: BlockScratch,
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
        Self {
            slot,
            channels,
            rx: bounded(0).1,
            gain,
            placement: PlacementRecord::default(),
            clock: Clock::new(),
            scratch: BlockScratch::new(),
        }
    }

    /// This node with a fresh command queue, and the handle that drives it:
    /// what [`into_parts`](IntoNode::into_parts) hands the graph and the
    /// caller. For a host (or a test) that calls the node by hand. A handle
    /// taken before goes dead
    /// ([`SendError::Disconnected`](super::command::SendError::Disconnected)).
    pub fn with_handle(mut self) -> (Self, VoiceNodeHandle) {
        let (tx, rx) = bounded(COMMAND_CAPACITY);
        self.rx = rx;
        let params = self.param_set();
        let handle = VoiceNodeHandle {
            tx,
            placement: Arc::clone(&self.placement),
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

    /// Apply every queued command, and the gain. Runs at the top of each
    /// block.
    ///
    /// Allocation-free by construction: the only command a node accepts carries
    /// two `Copy` scalars, and applying it is a field write. The pool's drain
    /// has to be more careful because `AddVoice` moves a whole `Voice` — which
    /// is why its handle builds the stretch filter on the *control* thread
    /// before sending.
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
                // Placement is the source's alone: `Playback` has no placement
                // field (two copies kept in sync by hand is how a fork reaches
                // the wrong one), so there is exactly one write.
                self.slot
                    .voice
                    .source
                    .apply_placement(start_beat, duration_beats);
            }
        }
        // Both halves of the gain: `Playback` is what the memory tier's read
        // scales by, the source's own cell what the disk tier's does.
        let gain = self.gain.load();
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

impl Node for VoiceNode {
    /// No inputs, [`channels`](Self::channels) outputs; a generator, never
    /// skipped.
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, self.channels).with_tail(Tail::Unbounded)
    }

    /// The rate the voice's source and its stretch filter run at.
    fn prepare(&mut self, p: &Prepare) {
        let sample_rate = p.sample_rate();
        self.slot.voice.source.prepare_rate(sample_rate);
        self.slot.sample_rate = sample_rate;
        if let Some(unit) = &mut self.slot.stretch {
            unit.set_sample_rate(sample_rate);
        }
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        self.drain_commands();
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
        let mut from = 0;
        while from < frames {
            let to = (from + LANE_FRAMES).min(frames);
            self.slot
                .render_into(&clock, from..to, n, &mut self.scratch, &mut refs[..n]);
            from = to;
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
}

/// A voice node's fork source: see "Forks" on [`VoiceNode`].
struct VoiceNodeFork {
    /// The voice as inserted. Shares with the live node what a `Voice`
    /// clone shares (a memory voice's gain cell, a disk voice's stream
    /// record and control cell, never its ring reader); a fork copies it
    /// with `Voice::fork_copy`, which shares nothing.
    template: Voice,
    channels: ChannelLayout,
    placement: PlacementRecord,
    /// The node's params: a fork starts from the gain last **set** (the
    /// authored value), not a modulation composite caught in the live cell.
    params: ParamSet,
}

impl ForkSource for VoiceNodeFork {
    fn fork(&self, mode: ForkMode<'_>) -> Result<Forked, ForkCause> {
        let mut voice = self.template.fork_copy(mode)?;
        let placed = *self
            .placement
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((start_beat, duration_beats)) = placed {
            voice.source.apply_placement(start_beat, duration_beats);
        }
        let gain = self
            .params
            .authored(UnitParam::Volume)
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
            placement: Arc::clone(&node.placement),
            params: handle.params.clone(),
        };
        NodeParts {
            node: Box::new(node),
            controls: handle,
            fork: Some(Box::new(fork)),
        }
    }
}
