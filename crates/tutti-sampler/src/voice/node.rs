//! [`VoiceNode`] — a single [`Voice`] as its own graph node.
//!
//! Zero inputs, N outputs, and typed controls ([`VoiceNodeHandle`], what its
//! `IntoNode` hands back): its gain, a scalar cell the node reads once per
//! block, and its window, loop, speed and play state, published values it
//! reads once per block. For resynth, preview and a single timeline clip,
//! where a whole pool would be ceremony. It shares `PlaybackSlot` with
//! [`VoicePool`](super::pool::VoicePool), so the per-voice read is the same
//! code in both — a fix in one is a fix in both.

use std::sync::Arc;

use crate::lanes::{BlockScratch, UNPREPARED_FRAMES};
use crate::streamer::Stream;
use crate::stretch;
use crate::{MAX_SAMPLER_CHANNELS, nonempty};

use super::clock::{BlockClock, Clock};
use super::command::{Steering, SteeringCell, VoiceNodeHandle};
use super::disk_voice::DiskVoice;
use super::disk_voice::VoiceHealth;
use super::slot::{PlaybackSlot, stretch_wanted};
use super::types::{Playback, Reach, SlotId, Voice, VoiceSource};
use tutti_graph::{
    Cx, ForkCause, ForkMode, ForkSource, Forked, IntoNode, Io, Node, NodeParts, ParamSet, Prepare,
    Shape, Status,
};
use tutti_rt::Reader;
use tutti_transport::TempoReader;
use tutti_types::{Amplitude, ChannelLayout, Param, SampleRate, Tail, UnitParam};

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
/// - gain — a `Param<Amplitude>` cell, also addressable as
///   [`UnitParam::Volume`] through the handle's [`ParamSet`]. The node reads
///   it once per block into the voice's `Playback` record and its source.
/// - window, loop, speed and play state: not scalars, so the handle publishes
///   them to an `ArcLend` cell the node reads once per block, latest wins,
///   allocation-free. A disk voice's loop is its [`Stream`]'s, which the
///   handle owns ([`VoiceNode::streaming`]).
///
/// # Forks
///
/// The node's fork source keeps a copy of the voice as it was inserted, and a
/// fork is that copy made to share nothing with the live node
/// (`Voice::fork_copy`: its own gain cell; a disk voice reading its file
/// itself), moved to the window, loop and speed the handle last published and
/// set to the gain last set, with a stretch filter of its own. It reads its render's
/// transport from its block's `Env`. A disk voice's node
/// forks only for an offline render.
pub struct VoiceNode {
    /// The single voice plus its resident stretch filter and the per-sample
    /// read, shared with the pool's slots.
    pub(crate) slot: PlaybackSlot,
    /// Output width — the node's audio outputs, fixed at construction.
    /// Declared rather than inferred from the voice, so an edge wired against
    /// it cannot be re-arityed under a live graph.
    pub(crate) channels: ChannelLayout,
    /// A reader of the steering the handle publishes, lent once per block; the
    /// reader owns the cell (`Reader::cell`), which the handle and the fork
    /// source share. Empty until a handle sets a field, so a node driven
    /// by hand keeps what it was built with.
    steering: Reader<Steering>,
    /// The steering last applied, so a block applies a field once, when it
    /// changes, rather than every block.
    applied: Steering,
    /// The disk stream of a streaming node, until [`with_handle`](Self::with_handle)
    /// hands it to the handle: a stream is control-thread only (its drop
    /// sends on a bounded queue), so the node never keeps one it plays with.
    stream: Option<Stream>,
    /// The gain the handle writes; read once per block.
    gain: Param<Amplitude>,
    /// The transport as the voice reads it, kept across blocks so a jump is
    /// flushed where it happens.
    clock: Clock,
    /// The project's tempo map, when the node reads one: lent once per block
    /// to its clock ([`with_tempo`](Self::with_tempo)).
    tempo: Option<TempoReader>,
    /// The block read's lanes (`PlaybackSlot::render_into`), built here and
    /// sized from `Prepare::max_block` in `prepare`, on the control thread.
    scratch: BlockScratch,
    /// Whether a handle still reaches the node; a fork's is frozen. It
    /// decides how exactly the node states its ring-out
    /// ([`ring_out`](Self::ring_out)).
    reach: Reach,
}

impl VoiceNode {
    /// Wraps a single [`Voice`] as a standalone stereo graph node. Builds the
    /// resident stretch processor once (like a mixer slot), off any hot path.
    pub fn new(voice: Voice) -> Self {
        Self::with_channels(voice, ChannelLayout::STEREO)
    }

    /// Wraps a single [`Voice`] as a `channels`-wide graph node.
    ///
    /// Builds the stretch filter here when `voice.play` asks for one. This
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
            steering: SteeringCell::default().reader(),
            applied: Steering::default(),
            stream: None,
            gain,
            clock: Clock::new(),
            tempo: None,
            scratch: BlockScratch::new(UNPREPARED_FRAMES, channels.count() as usize),
            reach: Reach::Live,
        }
    }

    /// A node over a streaming voice, and the [`Stream`] that feeds it: the
    /// handle ([`with_handle`](Self::with_handle), or the graph's insert)
    /// takes the stream, so the voice sounds for as long as the handle lives,
    /// and its loop is set through the handle. `play`'s loop and direction are
    /// set on the stream first, as [`VoicePoolHandle::add_streaming`](super::VoicePoolHandle::add_streaming)
    /// sets them; one the stream could not take (its streamer has exited) is
    /// not claimed by the voice's `Playback`. `voice` is the one built from the
    /// stream's own reader.
    ///
    /// A node never handed to a handle keeps the stream until it is dropped:
    /// built by hand, drop it on the control thread.
    #[must_use]
    pub fn streaming(mut voice: DiskVoice, mut stream: Stream, mut play: Playback) -> Self {
        if play.loop_ != stream.loop_() && stream.set_loop(play.loop_).is_err() {
            play.loop_ = stream.loop_();
        }
        if play.direction != stream.direction() && stream.set_direction(play.direction).is_err() {
            play.direction = stream.direction();
        }
        voice.set_gain(play.gain);
        voice.set_speed(play.speed);
        let width = ChannelLayout::from_count(voice.width() as u16);
        let mut node = Self::with_channels(
            Voice {
                source: VoiceSource::Disk(voice),
                play,
            },
            width,
        );
        node.stream = Some(stream);
        node
    }

    /// This node reading the project's tempo map through `tempo`
    /// ([`TransportSettings::tempo_reader`](tutti_transport::TransportSettings::tempo_reader)).
    /// Over a map whose tempo changes, how long a placed voice has played is
    /// the map's seconds between its start and the playhead, not the beats
    /// over the tempo of the moment; see
    /// [`VoicePool::with_tempo`](super::VoicePool::with_tempo). It matters
    /// only when the voice is placed on the transport. A fork reads the same map.
    #[must_use]
    pub fn with_tempo(mut self, tempo: TempoReader) -> Self {
        self.tempo = Some(tempo);
        self
    }

    /// This node with a fresh steering cell, and the handle that drives
    /// it: what [`into_parts`](IntoNode::into_parts) hands the graph and the
    /// caller. For a host (or a test) that calls the node by hand. What was
    /// set through a handle taken before no longer reaches the node (its
    /// gain still does: that cell is the node's for life). A streaming node's
    /// stream moves into the handle.
    pub fn with_handle(mut self) -> (Self, VoiceNodeHandle) {
        self.steering = SteeringCell::default().reader();
        let params = self.param_set();
        let handle = VoiceNodeHandle::new(
            Arc::clone(self.steering.cell()),
            self.gain.handle(),
            params,
            self.stream.take(),
        );
        (self, handle)
    }

    /// The node's params by address: its gain, as [`UnitParam::Volume`].
    /// The cells the node reads, so a set taken before the node is inserted
    /// addresses the inserted node (it is the set the handle carries).
    pub fn param_set(&self) -> ParamSet {
        self.declared_set()
    }

    /// Apply the published steering, and the gain. Runs at the top of each
    /// block.
    ///
    /// Allocation- and free-free by construction: one `ArcLend` read (a
    /// borrow that cannot free, dropped before the block's read), a `Copy`
    /// value, and a field write when one changed. The pool's drain has to be
    /// more careful because its commands carry owned payloads (a `Voice`, a
    /// wave, a stretch filter), which is why it retires what it lets go of.
    /// A node takes no such payload: there is nothing for it to drop.
    ///
    /// `driven` is the block's gain from the node's param port when a lane
    /// drives it (its last frame); `None` reads the cell.
    #[inline]
    fn drain_commands(&mut self, driven: Option<f32>) {
        let published = *self.steering.lend();
        if published != self.applied {
            apply_steering(&mut self.slot.voice, &self.applied, &published);
            self.applied = published;
        }
        // Both halves of the gain: `Playback` is what the memory tier's read
        // scales by, the source's own cell what the disk tier's does.
        // A cell holding a NaN or an infinity (a host can write anything into
        // it) is the gain the voice already has.
        let gain = driven.map_or_else(|| self.gain.load(), |v| Amplitude(v.max(0.0)));
        let gain = if gain.get().is_finite() {
            gain
        } else {
            self.slot.voice.play.gain
        };
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

    /// The ring-out the node declares ([`Shape::tail`]): its voice's
    /// ([`Voice::ring_out`]), promised as a ceiling while a handle can still
    /// steer the node and exactly in a fork, which none reaches.
    fn ring_out(&self) -> Tail {
        self.slot.voice.ring_out(self.reach)
    }

    /// What the node declares to the cost model: `Moderate` for a plain
    /// read, `Heavy` with a time-stretch filter.
    ///
    /// Measured: a plain memory voice 13 ns
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

// The param a `VoiceNode` lets the graph modulate: its gain, as
// `UnitParam::Volume`. Block-rate in the voice (read once per block, as its
// cell is), so a driven port is read the same way: its value on the block's
// last frame.
tutti_graph::declare_params!([] VoiceNode, n {
    Volume => Amplitude = &n.gain,
});

impl Node for VoiceNode {
    /// No inputs, [`channels`](Self::channels) outputs; a source, never
    /// skipped, with the ring-out of `ring_out`.
    fn shape(&self) -> Shape {
        Shape::source_unassigned(self.channels)
            .with_tail(self.ring_out())
            .with_cost(self.cost())
            .with_param_addrs(self.declared_ports().as_slice())
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
        self.drain_commands(
            self.declared_param::<{ UnitParam::Volume as u16 }>(&io)
                .last_frame(),
        );
        // Lent for the block and given back after it: the render borrows the
        // whole node.
        let mut reader = self.tempo.take();
        let map = reader.as_mut().map(TempoReader::lend);
        let clock = self.clock.observe_with(cx.env, map.as_deref());
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
        if self.applied.stopped {
            // Silent, and nothing read. A disk voice's ring read by position
            // is dropped, so the block it resumes in reads from the playhead
            // as a jump does; a free-running memory voice keeps its cursor.
            if let VoiceSource::Disk(disk) = &mut self.slot.voice.source {
                disk.flush();
            }
            drop(map);
            self.tempo = reader;
            return Status::Modified;
        }
        // One piece: in a graph, `prepare` sized the scratch for every block.
        if frames <= self.scratch.frames() {
            self.slot
                .render_into(&clock, 0..frames, n, &mut self.scratch, &mut refs[..n]);
        } else {
            self.render_in_pieces(&clock, frames, n, &mut refs[..n]);
        }
        drop(map);
        self.tempo = reader;
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
        self.declared_base(k)
    }
}

/// Applies what `now` publishes over what `was` already had applied, a field
/// at a time and only the ones that changed (a loop set again would restart
/// the free-running cursor's lap). A disk voice's loop is its stream's, so
/// only a memory voice takes one here.
fn apply_steering(voice: &mut Voice, was: &Steering, now: &Steering) {
    if now.window != was.window
        && let Some(window) = now.window
    {
        // The window is the source's alone: `Playback` has no window field
        // (two copies kept in sync by hand is how a fork reaches the wrong
        // one), so there is exactly one write.
        voice.source.apply_window(window);
    }
    if now.loop_ != was.loop_
        && let Some(setting) = now.loop_
        && let VoiceSource::Memory(source) = &mut voice.source
    {
        source.set_loop(setting);
        voice.play.loop_ = setting;
    }
    if now.speed != was.speed
        && let Some(speed) = now.speed
    {
        voice.source.apply_speed(speed);
        voice.play.speed = speed;
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
    /// The live node's steering cell, read with `current` when a fork is
    /// taken (an owning read on the control side, never a lend).
    steering: SteeringCell,
    /// The node's params: a fork starts from the gain cell, which holds the
    /// gain last set (a modulation lane sends its composite to the
    /// port and never writes the cell).
    params: ParamSet,
    /// The tempo map the live node reads, which a fork reads too.
    tempo: Option<TempoReader>,
}

impl ForkSource for VoiceNodeFork {
    fn fork(&self, mode: ForkMode) -> Result<Forked, ForkCause> {
        let mut voice = self.template.fork_copy(mode)?;
        apply_steering(&mut voice, &Steering::default(), &self.steering.current());
        let gain = self
            .params
            .get(UnitParam::Volume)
            .map_or(voice.play.gain, Amplitude);
        voice.play.gain = gain;
        voice.source.apply_gain(gain);
        let health = voice.fault().map(VoiceHealth);
        let mut node = VoiceNode::with_channels(voice, self.channels);
        node.tempo = self.tempo.clone();
        node.reach = Reach::Frozen;
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
            steering: Arc::clone(&handle.steering),
            params: handle.params.clone(),
            tempo: node.tempo.clone(),
        };
        NodeParts {
            node: Box::new(node),
            controls: handle,
            fork: Some(Box::new(fork)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemorySource, Playback, VoiceSource, VoiceWindow};
    use tutti_types::Beat;

    /// A steering the node held across the next `set_window` is freed by
    /// the next `reclaim`, with no further publish.
    #[test]
    fn reclaim_frees_a_window_a_block_held_across_the_next() {
        let wave = Arc::new(tutti_codec::Wave::from_samples(
            SampleRate::SR_48K,
            &[0.0; 64],
        ));
        let voice = Voice {
            source: VoiceSource::Memory(MemorySource::new(wave).expect("a placeable wave")),
            play: Playback::default(),
        };
        let (mut node, handle) = VoiceNode::new(voice).with_handle();
        {
            // A block in progress across the move.
            let _held = node.steering.lend();
            handle.set_window(VoiceWindow::from(Beat::new(1.0)));
        }
        assert_eq!(handle.steering.retired_len(), 1, "pinned until a scan");
        assert_eq!(tutti_rt::Reclaim::reclaim(&handle), 0);
        assert_eq!(handle.steering.retired_len(), 0, "freed by the reclaim");
    }

    /// **A node's ring-out follows what can still steer it.** A placed voice
    /// is the programme and has none, whoever steers it; a free-running voice
    /// that a handle can loop, slow or stretch promises no ceiling; and its
    /// fork, which nothing steers, declares its wave's length at the rate it
    /// reads, at the stretch it plays.
    #[test]
    fn a_nodes_ring_out_is_the_ceiling_its_steering_allows() {
        let wave = Arc::new(tutti_codec::Wave::from_samples(
            SampleRate::SR_48K,
            &[0.0; 4_800],
        ));
        let prepare = Prepare::new(
            SampleRate::SR_48K,
            tutti_types::MaxBlock::of(tutti_types::Samples(64)),
        );
        let free = |play: Playback| {
            let voice = Voice {
                source: VoiceSource::Memory(
                    MemorySource::new(Arc::clone(&wave)).expect("a placeable wave"),
                ),
                play,
            };
            let NodeParts {
                node: mut live,
                fork,
                ..
            } = VoiceNode::new(voice).into_parts();
            live.prepare(&prepare);
            let mut fork = fork
                .expect("forks")
                .fork(ForkMode::Offline)
                .expect("forks")
                .node;
            fork.prepare(&prepare);
            (live.shape().tail, fork.shape().tail)
        };
        let (live, fork) = free(Playback::default());
        assert_eq!(live, Tail::Unbounded);
        assert_eq!(fork, Tail::Finite(tutti_types::Samples(4_800)));

        let stretched = Playback {
            stretch: tutti_types::StretchFactor::new(2.0),
            ..Playback::default()
        };
        assert_eq!(
            free(stretched).1,
            Tail::Finite(tutti_types::Samples(9_600)),
            "a stretch of two plays twice as long"
        );

        let placed = Voice {
            source: VoiceSource::Memory(
                MemorySource::placed(Arc::clone(&wave), VoiceWindow::from(Beat::new(0.0)))
                    .expect("a placeable wave"),
            ),
            play: Playback::default(),
        };
        assert_eq!(VoiceNode::new(placed).shape().tail, Tail::None);
    }

    /// **A streaming node's handle owns its stream.** With every other holder
    /// gone (the caller moved the stream into the node; nothing keeps it
    /// apart), the stream stays served for as long as the handle lives, and
    /// ends with the last clone of it.
    #[test]
    fn a_streaming_nodes_handle_owns_its_stream() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("tone.wav");
        let samples = vec![0.25f32; 4_800];
        tutti_fixtures::signal::write_wav_f32(&path, SampleRate::SR_48K, 1, &samples);
        let mut streamer = crate::DiskStreamer::manual(SampleRate::SR_48K, Default::default())
            .expect("streamer builds");
        let opening = streamer.open(&path, crate::Placement::HEAD);
        let (stream, reader) = crate::testing::settle(&mut streamer, opening).expect("opens");
        let served = Arc::clone(&reader.status);
        let voice = crate::DiskVoice::from_reader(reader, crate::VoiceWindow::default());

        let (node, handle) = VoiceNode::streaming(voice, stream, Playback::default()).with_handle();
        let second = handle.clone();
        let _ = streamer.step_until_settled(1_000);
        assert!(!served.closed(), "the handle holds the stream");

        drop(handle);
        let _ = streamer.step_until_settled(1_000);
        assert!(!served.closed(), "a clone of the handle still holds it");
        assert!(
            second.status().is_some(),
            "a streaming handle reports its stream"
        );

        drop(second);
        let _ = streamer.step_until_settled(1_000);
        assert!(served.closed(), "the last clone's drop ended the stream");
        drop(node);
    }
}
