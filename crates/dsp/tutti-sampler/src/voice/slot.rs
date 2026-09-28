//! `PlaybackSlot` — a [`Voice`] plus its resident time-stretch processor, and
//! the read that turns the pair into audio: a block at a time into planar
//! lanes (`render_into`), on the block's transport.
//!
//! This is where the two source tiers and the two stretch states meet: four
//! combinations, each of which has to agree with the others about how much
//! source one output sample costs. They are easy to get subtly wrong, so the
//! reasoning is kept inline at each fork.

use std::ops::Range;

use crate::lanes::{accumulate, scale, BlockScratch, LANE_FRAMES};
use crate::nonempty;
use crate::stretch;

use super::clock::BlockClock;
use super::types::{Playback, SlotId, Voice, VoiceSource};
use tutti_core::{Cents, ChannelLayout, ReadRate, SamplePosition, SampleRate, StretchFactor};

/// A [`Voice`] plus the resident time-stretch DSP processor, and the read that
/// turns the pair into audio.
///
/// The processor is the DSP object rather than a config record, so it stays on
/// the slot alongside the engine sample rate it is tuned to; the control
/// *intent* — factor, pitch, gain, direction, loop — lives in `voice.play`, and
/// the two are kept in step by [`set_stretch`](Self::set_stretch) writing both.
///
/// Shared verbatim by [`VoicePool`](super::pool::VoicePool)'s slot vector and by
/// [`VoiceNode`], which holds exactly one — so the per-voice read has one
/// definition and a fix in it is a fix in both.
///
/// # Not a `VoiceSlot`
///
/// `tutti-polysynth` has a `VoiceSlot`, and it is a different kind of thing: an
/// *allocator* slot carrying note identity plus an Idle/Active/Releasing/Stolen
/// lifecycle, which is what its stealing policy scores over. This type carries
/// no identity and no such state — it is a playback descriptor, and everything
/// about which slot is live is decided by the owner. Sharing the name across the
/// two crates invited reading one crate's stealing rules into the other's, so
/// the name says which job this is.
pub(crate) struct PlaybackSlot {
    /// Addresses this slot for every command after the add. `SlotId(0)` on a
    /// `VoiceNode`, whose single voice has nothing to disambiguate.
    pub(crate) id: SlotId,
    /// The source (in-memory or streaming) plus the control intent recorded for
    /// it. Which tier is live is the [`VoiceSource`] arm; the read below forks
    /// on that enum at each call site rather than through a trait, so a
    /// tier-conditional difference stays visible.
    ///
    /// Boxed as `VoiceCommand::AddVoice` carries it, so the pool's drain moves
    /// the box in rather than moving the `Voice` out of it — which would free
    /// the box on the audio thread. A slot leaves the pool through the
    /// retirement channel, box and all.
    pub(crate) voice: Box<Voice>,
    /// The time-stretch processor is **always resident**: it is built once (one
    /// phase-vocoder construction + two `RtScratch` buffers *per channel*) when
    /// the slot is created, and thereafter the audio thread only flips the
    /// lock-free `stretch_factor` / `pitch_cents` atomics inside it. It owns NO
    /// copy of the audio source: it is a pure frame-in → frame-out filter. At
    /// read time, the `needs_stretch()` gate (mirrored from those atomics into
    /// `voice.play.stretch` / `voice.play.pitch`) chooses whether to read the
    /// single source and route its frames through this filter, or read the
    /// source directly.
    ///
    /// **"Built once" is not the same as "built off the audio thread".**
    /// Per-buffer *updates* are genuinely allocation-free —
    /// [`VoiceCommand::UpdateStretch`] only sets atomics. But the construction
    /// itself happens in `insert_voice`, reached from
    /// [`VoiceCommand::AddVoice`], which `drain_commands` pulls from the
    /// node's `process`. So adding a voice mid-playback DOES build the vocoders in the
    /// callback, and the cost scales with channel count: at width `n` that is
    /// `n` FFT setups plus `2n` scratch allocations.
    ///
    /// Structural invariant: `voice.play.stretch` / `voice.play.pitch` cannot
    /// drift from the processor's atomics — every mutation goes through
    /// [`PlaybackSlot::set_stretch`], which writes both in one step.
    pub(crate) stretch: Option<stretch::Unit>,
    /// Width the stretch unit must be built at, remembered so a later
    /// materialisation matches the reader rather than defaulting.
    pub(crate) channels: ChannelLayout,
    /// The engine rate the resident stretch unit is tuned to, kept in step by
    /// the owner's `prepare`. A stale value here is a pitch error
    /// proportional to the device's real rate, reported nowhere.
    pub(crate) sample_rate: SampleRate,
}

impl PlaybackSlot {
    /// Build a slot at an explicit width.
    ///
    /// # Width is explicit at every call site
    ///
    /// There is no stereo-defaulting `new`, because both callers (the reader's
    /// drain and `VoiceNode`) know their own width and a default here would
    /// silently mismatch it.
    ///
    /// # The stretch unit is not built here
    ///
    /// It is `None` until the slot
    /// actually needs it, for two reasons:
    ///
    /// - Most voices never stretch. A `stretch::Unit` is one FFT setup plus two
    ///   `RtScratch` buffers *per channel* (~120 KB × N), so building one for
    ///   every voice spent that on the common case for nothing.
    /// - This constructor is reachable from the audio thread.
    ///   `VoiceCommand::AddVoice` is drained by `drain_commands`, which runs from
    ///   the node's `process`, so eager construction meant adding a voice mid-playback
    ///   allocated in the callback.
    ///
    /// # Who builds one, when the voice needs it
    ///
    /// A slot that arrives already needing stretch (non-unity `play.stretch` /
    /// `play.pitch`) gets its unit built on the **control thread**, before the
    /// audio thread ever sees the slot. The two owners do that differently, and
    /// the difference is not cosmetic:
    ///
    /// - **`VoicePool`** takes a pre-built filter from the sender — see
    ///   `VoicePoolHandle::prepare`, which fills in `VoiceCommand::AddVoice`'s
    ///   `stretch` field. It has to: `AddVoice` is drained in the callback, so the
    ///   pool cannot build one at the point it learns it needs one.
    /// - **`VoiceNode` builds its own**, in `VoiceNode::with_channels`, because
    ///   that constructor *is* control-thread code. Nothing prepares a filter for
    ///   a node, and nothing needs to.
    ///
    /// # Turning stretch on later is a pool-only capability
    ///
    /// `VoiceCommand::UpdateStretch` reaches `set_stretch` below through the
    /// pool's drain; `VoiceNode::drain_commands` handles `UpdatePlacement` and
    /// nothing else, so the same command sent to a `VoiceNodeHandle` is discarded
    /// without a word. A node's pitch is therefore fixed at construction, and a
    /// host that wants to change it respawns the voice, which is what the
    /// hosts we know of do.
    ///
    /// If that gap is ever closed, `VoiceNode`'s `prepare` has to close with
    /// it. It refreshes an existing unit (`if let Some(unit) = &mut
    /// self.slot.stretch`) and cannot create one, so a filter adopted mid-flight
    /// would keep the rate it was built at and never be corrected — a pitch
    /// error proportional to the device's real rate, with nothing logged.
    pub(crate) fn with_channels(
        id: SlotId,
        voice: Box<Voice>,
        sample_rate: SampleRate,
        channels: impl Into<ChannelLayout>,
    ) -> Self {
        let channels = nonempty(channels.into());
        Self {
            id,
            voice,
            stretch: None,
            channels,
            sample_rate,
        }
    }

    /// Whether the slot's control intent asks for stretching. **Independent of
    /// whether a unit exists**, which is why every hot path gates on
    /// `needs_stretch() && stretch.is_some()` rather than on this alone: intent
    /// without a filter means the voice reads dry.
    ///
    /// `VoicePoolHandle::prepare` asks the same question of the values it is about
    /// to send (via `stretch_values_want_filter`) to decide whether to build one.
    pub(crate) fn needs_stretch(&self) -> bool {
        stretch_wanted(&self.voice.play)
    }

    /// Drop every sample of audio this slot is holding mid-flight.
    ///
    /// Called from two places, which is why it is one function: `Node::reset`
    /// (a graph-level reset) and a transport jump (a seek or a loop wrap,
    /// on its frame: [`render_into`](Self::render_into)). They must clear exactly the same state — a seek that
    /// flushed less than a reset would leave a subset of the stale audio behind,
    /// which sounds like an intermittent artifact rather than a bug.
    ///
    /// The stretch filter is the state that matters. A placed source re-derives
    /// its position from the playhead every frame, so it is correct the instant
    /// the playhead moves; the vocoder's FIFOs and per-bin phase accumulators are
    /// not, and nothing else clears them.
    ///
    /// RT-safe: `Vocoder::reset` is buffer fills and cursor resets, no allocation.
    pub(crate) fn flush_playhead_state(&mut self) {
        self.voice.source.flush();
        if let Some(unit) = &mut self.stretch {
            unit.reset();
        }
    }

    /// Update the stretch factors — the only entry point for mutating them.
    /// Lock-free: mirrors the values into `voice.play` (read by the
    /// [`needs_stretch`](Self::needs_stretch) gate) and flips the resident
    /// processor's atomics.
    ///
    /// `incoming` adopts a processor built by the sender, for the case where this
    /// slot has none and the new values ask for one — a voice spawned at
    /// unity/zero correctly got `stretch: None` from `AddVoice`, so turning
    /// stretching on later has to bring its own filter. Passing `None` when one is
    /// already resident is the common path (pure atomics); an `incoming` that
    /// arrives redundantly is handed back rather than swapped in, so a live filter
    /// never loses its phase mid-note.
    ///
    /// **Allocation-free and free-free**, so it is safe on the audio-thread
    /// command drain: adopting is a move, and a redundant arrival is *returned*
    /// rather than dropped — see the caller in [`VoicePool::drain_commands`],
    /// which retires it for control-thread release. Dropping a `stretch::Unit`
    /// here would free its vocoder bank in the callback.
    ///
    /// Both this and `incoming` are unboxed for the same reason: moving a `Unit`
    /// out of a `Box` frees the box, which is itself a deallocation on this
    /// thread.
    ///
    /// Until a filter arrives the slot reads dry, which is why the hot paths gate
    /// on `needs_stretch() && stretch.is_some()` rather than on the intent alone.
    pub(crate) fn set_stretch(
        &mut self,
        stretch_factor: StretchFactor,
        pitch_cents: Cents,
        incoming: Option<stretch::Unit>,
    ) -> Option<stretch::Unit> {
        self.voice.play.stretch = stretch_factor;
        self.voice.play.pitch = pitch_cents;
        let surplus = match (self.stretch.is_some(), incoming) {
            // Nothing resident and the sender sent one: adopt it. This is the
            // case that makes turning stretch on mid-flight audible at all.
            (false, Some(unit)) => {
                self.stretch = Some(unit);
                None
            }
            // Already have one — hand the spare back unused.
            (true, Some(unit)) => Some(unit),
            (_, None) => None,
        };
        if let Some(unit) = &self.stretch {
            unit.set_stretch_factor(stretch_factor);
            unit.set_pitch_cents(pitch_cents);
        }
        surplus
    }

    /// Accumulate this slot's contribution to block frames `range` (at most
    /// [`LANE_FRAMES`]) into `out` (one slice per channel, block-long),
    /// **a block at a time**: the voice renders into `scratch`'s planar lanes
    /// ([`render_lanes`](Self::render_lanes)), and each lane is added into its
    /// output channel by one vectorised [`accumulate`]. Shared by the mixer
    /// loop and the standalone [`VoiceNode`](super::node::VoiceNode), so the
    /// per-voice read has one definition.
    ///
    /// **A jump flushes on its frame.** Where the playhead jumps inside the
    /// range (a seek, a loop wrap: a run of the block that starts with
    /// [`jump`](super::clock::Run::jump)), the read stops there, the slot's
    /// buffered state is flushed ([`flush_playhead_state`](Self::flush_playhead_state)),
    /// and it goes on from the jump: a stretch filter never plays the old
    /// region's material over the new one, and never loses what it held
    /// before the jump.
    ///
    /// **Audio-thread safe**: allocation-free (the lanes are the owner's,
    /// built on the control thread), and the tier fork is a [`VoiceSource`]
    /// match rather than a virtual call.
    ///
    /// `width` is a plain `usize`, deliberately **not** a [`ChannelLayout`]: it is
    /// an already-intersected clamp that callers compute as
    /// `slot width ∧ output channels ∧ MAX_SAMPLER_CHANNELS`, not a declaration
    /// of how many channels anything *has*.
    #[inline]
    pub(crate) fn render_into(
        &mut self,
        clock: &BlockClock<'_>,
        range: Range<usize>,
        width: usize,
        scratch: &mut BlockScratch,
        out: &mut [&mut [f32]],
    ) {
        let n = width.min(out.len());
        debug_assert!(range.len() <= LANE_FRAMES, "a piece longer than a lane");
        let mut from = range.start;
        for run in clock.runs_in(range.clone()) {
            if run.jump {
                if run.start > from {
                    self.render_piece(clock, from..run.start, n, scratch, out);
                    from = run.start;
                }
                self.flush_playhead_state();
                self.prime_stretch(clock, run.start, n, scratch);
            }
        }
        self.render_piece(clock, from..range.end, n, scratch, out);
    }

    /// After a jump's flush, feed a memory voice's stretch filter the
    /// source it would have read in the [`refill`](stretch::Unit::refill_frames)
    /// frames leading up to block frame `at`, and discard what it says. A
    /// flushed filter holds no window of input, so it is otherwise silent
    /// after every seek or wrap for as long as it takes to take one in (a
    /// window times the effective stretch: about 4 100 frames at 2x);
    /// primed, its output from `at` on is what it would have been had the
    /// voice been playing there all along — the same lag, no gap.
    ///
    /// The positions are the placed read's own, extended backwards from
    /// `at`'s by its step (`read_rate × stretch_rate`); one before the
    /// wave's start feeds nothing. Only the memory tier primes: it can read
    /// any position, where the disk tier reads a ring forward and has nothing
    /// before the jump. No-op when the voice does not stretch or has no read
    /// at `at` (outside its window, a standing transport).
    ///
    /// Audio-thread safe: the scratch is the slot's owner's, and the filter
    /// runs as it does for a block. Costs the filter's work for the refill,
    /// once per jump.
    fn prime_stretch(
        &mut self,
        clock: &BlockClock<'_>,
        at: usize,
        n: usize,
        scratch: &mut BlockScratch,
    ) {
        if !self.needs_stretch() {
            return;
        }
        let direction = self.voice.play.direction;
        let gain = self.voice.play.gain.get();
        let (VoiceSource::Memory(sampler), Some(unit)) =
            (&mut self.voice.source, self.stretch.as_mut())
        else {
            return;
        };
        let refill = unit.refill_frames();
        if refill == 0 {
            return;
        }
        let stretch_rate = unit.input_rate();
        let BlockScratch {
            out,
            raw,
            positions,
            gather,
        } = scratch;
        sampler.placed_positions(clock, at..at + 1, stretch_rate, &mut positions[..1]);
        let Some(origin) = positions[0] else {
            return;
        };
        let step = sampler.read_rate().then(stretch_rate).get();
        let mut done = 0;
        while done < refill {
            let frames = (refill - done).min(LANE_FRAMES);
            for (i, slot) in positions[..frames].iter_mut().enumerate() {
                let back = (refill - done - i) as f64;
                let pos = origin.get() - back * step;
                *slot = (pos >= 0.0).then_some(SamplePosition(pos));
            }
            sampler.read_placed_lanes(&positions[..frames], direction, raw, n, gather);
            for lane in raw.lanes_mut().iter_mut().take(n) {
                scale(&mut lane[..frames], gain);
            }
            // Only the frames with a read feed the filter, as in the block.
            let mut i = 0;
            while i < frames {
                let inside = positions[i].is_some();
                let mut j = i + 1;
                while j < frames && positions[j].is_some() == inside {
                    j += 1;
                }
                if inside {
                    unit.filter_lanes(raw.lanes(), out.lanes_mut(), n, i..j);
                }
                i = j;
            }
            done += frames;
        }
    }

    /// [`render_lanes`](Self::render_lanes) for `range`, added into `out`.
    fn render_piece(
        &mut self,
        clock: &BlockClock<'_>,
        range: Range<usize>,
        n: usize,
        scratch: &mut BlockScratch,
        out: &mut [&mut [f32]],
    ) {
        if range.is_empty() {
            return;
        }
        let frames = range.len();
        self.render_lanes(clock, range.clone(), n, scratch);
        for (c, lane) in scratch.out.lanes().iter().enumerate().take(n) {
            accumulate(&mut out[c][range.clone()], &lane[..frames]);
        }
    }

    /// Render block frames `range` (at most [`LANE_FRAMES`]) of this voice,
    /// `n` channels wide, into frames `0..range.len()` of `scratch.out`:
    /// every frame of each of the `n` lanes is written, silence as zero.
    /// `scratch.raw` is the filter's input, when the voice stretches.
    ///
    /// Forks on the tier and the stretch, and reads what each tier reads:
    /// see the comments at each arm for the rule each keeps.
    pub(crate) fn render_lanes(
        &mut self,
        clock: &BlockClock<'_>,
        range: Range<usize>,
        n: usize,
        scratch: &mut BlockScratch,
    ) {
        let frames = range.len();
        let direction = self.voice.play.direction;
        let gain = self.voice.play.gain.get();
        // Route through the filter only when the intent asks for it AND a unit
        // is resident; a missing unit reads dry rather than silent.
        let stretching = self.needs_stretch() && self.stretch.is_some();
        let BlockScratch {
            out,
            raw,
            positions,
            gather,
        } = scratch;
        let positions = &mut positions[..frames];
        match (&mut self.voice.source, self.stretch.as_mut()) {
            (VoiceSource::Memory(sampler), Some(unit)) if stretching => {
                // **The stretch rate must reach the origin, not only the
                // step.** A placed voice re-derives its origin from the
                // playhead as the clock moves, and the playhead runs at wall
                // clock; seating there and stepping slower makes each seat
                // land a full piece ahead of where the previous one finished,
                // so the stretch is discarded at every boundary.
                //
                // Without this the factor degenerates to pure varispeed —
                // 2.0x turns 440 Hz into 880 Hz with the duration unchanged.
                // Folding the rate into the step *only* is worse still
                // (measured: pitch 35% off, purity 0.95 -> 0.54), because
                // origin and step then disagree within each block as well as
                // across them.
                //
                // Both come from `placed_positions`, which seats at the gate
                // with the stretch and steps by `read_rate` with the same
                // stretch — the two must be derived together or they drift
                // apart again. (The step is `read_rate`, not the gate's
                // `window_rate`: a file at another rate than the session's
                // moves `src_ratio` file frames per output frame.)
                let stretch_rate = unit.input_rate();
                sampler.placed_positions(clock, range, stretch_rate, positions);
                // Gain before the filter, as the frame read fed it.
                sampler.read_placed_lanes(positions, direction, raw, n, gather);
                for lane in raw.lanes_mut().iter_mut().take(n) {
                    scale(&mut lane[..frames], gain);
                }
                // Frames outside the window (or on a standing transport) feed
                // the filter nothing: it keeps what it holds, and they are
                // silent. The window opens and closes on its frames, so a
                // piece may be partly inside: the filter runs over each
                // stretch that is.
                let mut i = 0;
                while i < frames {
                    let inside = positions[i].is_some();
                    let mut j = i + 1;
                    while j < frames && positions[j].is_some() == inside {
                        j += 1;
                    }
                    if inside {
                        unit.filter_lanes(raw.lanes(), out.lanes_mut(), n, i..j);
                    } else {
                        for lane in out.lanes_mut().iter_mut().take(n) {
                            lane[i..j].fill(0.0);
                        }
                    }
                    i = j;
                }
            }
            (VoiceSource::Disk(reader), Some(unit)) if stretching => {
                // The disk tier has no cursor to scale: the stretch rate is
                // published into the `RtState` the reader's step reads, where
                // it composes with varispeed and the conversion. Published per
                // block rather than on the parameter-change path because the
                // rate is the *filter's*, and a voice returned to unity must
                // publish unity again — a set-once would leave the stream
                // reading at the old factor.
                reader.set_stretch_rate(unit.input_rate());
                reader.render_lanes(clock, range, raw, n);
                unit.filter_lanes(raw.lanes(), out.lanes_mut(), n, 0..frames);
            }
            (VoiceSource::Memory(sampler), _) => {
                // Seated at the gate's origin (`window_rate`, varispeed
                // alone, in the wave's own frames) and stepped by `read_rate`
                // (varispeed and `src_ratio`), per frame: see
                // `MemorySource::placed_positions`. Stepping by the gate's
                // rate read a 24 kHz file on a 48 kHz clock one file frame per
                // output frame, then jumped back at every block.
                sampler.placed_positions(clock, range, ReadRate::UNITY, positions);
                sampler.read_placed_lanes(positions, direction, out, n, gather);
                for lane in out.lanes_mut().iter_mut().take(n) {
                    scale(&mut lane[..frames], gain);
                }
            }
            (VoiceSource::Disk(reader), _) => {
                // Unity, every block: the stretch rate belongs to the filter
                // rather than to the voice, so a voice returned to 1.0x — or
                // one whose filter was removed — must publish unity or its
                // stream keeps reading at the old factor. `set_stretch` keeps
                // the resident filter and only rewrites its atomics, so this
                // is reachable through ordinary use, not just teardown. The
                // reader applies its own gain (the stream's control cell).
                reader.set_stretch_rate(ReadRate::UNITY);
                reader.render_lanes(clock, range, out, n);
            }
        }
    }
}

/// Whether a [`Playback`] record asks for stretching. Shared by the sender (to
/// decide whether to build a filter) and [`PlaybackSlot::needs_stretch`] (to decide
/// whether to route through one), so the two cannot disagree about what
/// "stretching" means.
#[inline]
pub(crate) fn stretch_wanted(play: &Playback) -> bool {
    stretch_values_want_filter(play.stretch, play.pitch)
}

/// The same question asked of a loose factor/pitch pair, for
/// [`VoiceCommand::UpdateStretch`](crate::voice::VoiceCommand::UpdateStretch) —
/// which carries the two values but no `Playback` to wrap them in.
///
/// [`stretch_wanted`] delegates here so the thresholds exist **once**. Writing
/// them out a second time at the sender is how the two sides come to disagree
/// about whether a given value stretches: the sender would decline to build a
/// filter that `needs_stretch` then routes through, and the voice reads dry with
/// no error — the precise failure this pair is factored to prevent.
#[inline]
pub(crate) fn stretch_values_want_filter(stretch: StretchFactor, pitch: Cents) -> bool {
    (stretch.get() - 1.0).abs() > 0.001 || pitch.get().abs() > 0.5
}
