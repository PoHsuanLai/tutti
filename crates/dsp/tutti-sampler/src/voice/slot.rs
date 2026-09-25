//! `PlaybackSlot` — a [`Voice`] plus its resident time-stretch processor, and
//! the per-sample read that turns the pair into audio.
//!
//! This is where the two source tiers and the two stretch states meet: four
//! combinations, each of which has to agree with the others about how much
//! source one output sample costs. Three separate pitch/stretch bugs have lived
//! in these branches, so the reasoning is kept inline at each fork.

use crate::stretch;
use crate::{nonempty, MAX_SAMPLER_CHANNELS};

use super::memory_source::MemorySource;
use super::types::{Direction, Playback, SlotId, Voice, VoiceSource};
use tutti_core::{
    Amplitude, AudioUnit, BufferMut, Cents, ChannelLayout, ReadRate, SamplePosition, SampleRate,
    StretchFactor,
};

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
    pub(crate) voice: Voice,
    /// The time-stretch processor is **always resident**: it is built once (one
    /// phase-vocoder construction + two `RtScratch` buffers *per channel*) when
    /// the slot is created, and thereafter the audio thread only flips the
    /// lock-free `stretch_factor` / `pitch_cents` atomics inside it. It owns NO
    /// copy of the audio source: it is a pure frame-in → frame-out filter. At
    /// tick time, the `needs_stretch()` gate (mirrored from those atomics into
    /// `voice.play.stretch` / `voice.play.pitch`) chooses whether to tick the
    /// single source and route its frame through this filter, or read the source
    /// directly.
    ///
    /// **"Built once" is not the same as "built off the audio thread".**
    /// Per-buffer *updates* are genuinely allocation-free —
    /// [`VoiceCommand::UpdateStretch`] only sets atomics. But the construction
    /// itself happens in `insert_voice`, reached from
    /// [`VoiceCommand::AddVoice`], which `drain_commands` pulls from `tick` /
    /// `process`. So adding a voice mid-playback DOES build the vocoders in the
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
    /// the owner's `set_sample_rate`. A stale value here is a pitch error
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
    ///   `tick`/`process`, so eager construction meant adding a voice mid-playback
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
    /// If that gap is ever closed, `VoiceNode::set_sample_rate` has to close with
    /// it. It refreshes an existing unit (`if let Some(unit) = &mut
    /// self.slot.stretch`) and cannot create one, so a filter adopted mid-flight
    /// would keep the `SR_44K1` its constructor assumed and never be corrected —
    /// a pitch error proportional to the device's real rate, with nothing logged.
    pub(crate) fn with_channels(
        id: SlotId,
        voice: Voice,
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
    /// Called from two places, which is why it is one function: `AudioUnit::reset`
    /// (a graph-level reset) and the per-block seek check (a transport
    /// discontinuity). They must clear exactly the same state — a seek that
    /// flushed less than a reset would leave a subset of the stale audio behind,
    /// which sounds like an intermittent artifact rather than a bug.
    ///
    /// The stretch filter is the state that matters. A placed source re-derives
    /// its position from the playhead every block, so it is correct the instant
    /// the playhead moves; the vocoder's FIFOs and per-bin phase accumulators are
    /// not, and nothing else clears them.
    ///
    /// RT-safe: `Vocoder::reset` is buffer fills and cursor resets, no allocation.
    pub(crate) fn flush_playhead_state(&mut self) {
        self.voice.source.as_audio_unit_mut().reset();
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

    /// Read ONE frame from this slot into `out`, whose length is the frame's
    /// channel count: the exact per-variant read the `tick` mixdown does for a
    /// single slot, routed through the resident stretch filter when
    /// `needs_stretch()`. Factored so the mixer loop and the standalone
    /// [`VoiceNode`] share one definition — no duplication, no per-sample dyn.
    ///
    /// **Audio-thread safe**: allocation-free, working out of a stack frame
    /// capped at `MAX_SAMPLER_CHANNELS`, and the tier fork is a
    /// [`VoiceSource`] match rather than a virtual call.
    #[inline]
    pub(crate) fn tick_frame_into(&mut self, out: &mut [f32]) {
        let direction = self.voice.play.direction;
        let gain = self.voice.play.gain;
        // Borrow the source and the filter as DISJOINT fields: `active_stretch`
        // would hold `&mut self` across the source read otherwise.
        let stretching = self.needs_stretch() && self.stretch.is_some();
        if stretching {
            // Tick the SINGLE source once to get its raw frame (the same
            // per-variant read the else-branch uses — the `VoiceSource` enum
            // still owns the read), then feed that frame into the stretch filter.
            // This is the one place a scratch frame is genuinely unavoidable:
            // the filter's input and output cannot be the same slice. Stack
            // array at the fixed ceiling, used as a prefix — no heap.
            let n = out.len().min(MAX_SAMPLER_CHANNELS);
            let mut raw = [0.0f32; MAX_SAMPLER_CHANNELS];
            // Keep the disk ring's rate in step with the filter, as `process`
            // does. A frame-at-a-time caller cannot supply the stretch itself
            // (there is no block to spread), but the ring must still drain at
            // the stretched rate or the two disagree about how much source a
            // second of output costs.
            let stretch_rate = self
                .stretch
                .as_ref()
                .map_or(ReadRate::UNITY, |unit| unit.input_rate());
            if let VoiceSource::Disk(reader) = &self.voice.source {
                reader.set_stretch_rate(stretch_rate);
            }
            read_source_frame_into(
                &mut self.voice.source,
                direction,
                gain,
                stretch_rate,
                &mut raw[..n],
            );
            out.fill(0.0);
            if let Some(unit) = &mut self.stretch {
                unit.tick(&raw[..n], out);
            }
        } else {
            match &mut self.voice.source {
                // Seated and stepped, as `process` reads it: a clock that moves
                // once per block steps through the block here too.
                VoiceSource::Memory(sampler) => match sampler.seated_position(ReadRate::UNITY) {
                    Some(pos) => read_clip_sample_into(sampler, direction, pos, gain, out),
                    None => out.fill(0.0),
                },
                VoiceSource::Disk(reader) => {
                    // Unity when nothing stretches — see the `process` twin.
                    reader.set_stretch_rate(ReadRate::UNITY);
                    // The `DiskVoice` owns its placement gate: it
                    // emits silence outside the voice window and pulls the
                    // butler ring inside it. Alloc-free (preallocated
                    // `fetch_scratch`).
                    out.fill(0.0);
                    reader.tick(&[], out);
                }
            }
        }
    }

    /// Accumulate this slot's contribution to `size` FRAMES into `output`: the
    /// exact per-variant read the `process` mixdown does for a single slot.
    /// Factored so the mixer loop and the standalone [`VoiceNode`] share one
    /// definition.
    ///
    /// **Audio-thread safe**: allocation-free, and the tier fork is a
    /// [`VoiceSource`] match rather than a virtual call.
    ///
    /// `width` is a plain `usize`, deliberately **not** a [`ChannelLayout`]: it is
    /// an already-intersected clamp that callers compute as
    /// `slot width ∧ output.channels() ∧ MAX_SAMPLER_CHANNELS`, not a declaration
    /// of how many channels anything *has*. Wrapping it back into a layout would
    /// claim a width the caller has already narrowed away.
    #[inline]
    pub(crate) fn process_into(&mut self, size: usize, width: usize, output: &mut BufferMut) {
        let direction = self.voice.play.direction;
        let gain = self.voice.play.gain;
        // `BufferMut` is planar with no frame accessor, so a per-sample frame is
        // unavoidable here. Stack array at the fixed ceiling, used as a prefix.
        let n = width.min(output.channels()).min(MAX_SAMPLER_CHANNELS);
        let mut frame = [0.0f32; MAX_SAMPLER_CHANNELS];

        /// Accumulate a frame prefix into the planar output at sample `i`.
        macro_rules! mix_in {
            ($frame:expr, $i:expr) => {
                for (c, &s) in $frame.iter().enumerate().take(n) {
                    output.set_f32(c, $i, output.at_f32(c, $i) + s);
                }
            };
        }

        // Route through the filter only when the intent asks for it AND a unit
        // is resident; see `active_stretch` for why a missing unit reads dry
        // rather than silent. Destructured so the source and the filter are
        // disjoint borrows.
        let stretching = self.needs_stretch() && self.stretch.is_some();
        let stretch = &mut self.stretch;
        if stretching {
            // Per-sample: read the SINGLE source frame (same per-variant read
            // as the else-branch — the enum still owns the read), then feed
            // it through the stretch filter. The filter's in and out cannot
            // alias, hence the second stack frame.
            let mut raw = [0.0f32; MAX_SAMPLER_CHANNELS];
            let Some(unit) = stretch.as_mut() else {
                return;
            };
            match &mut self.voice.source {
                VoiceSource::Memory(sampler) => {
                    // **The stretch rate must reach the origin, not only the
                    // step.** A placed voice re-derives its origin from the
                    // playhead every block, and the playhead runs at wall clock;
                    // seating there and stepping slower makes each block re-seat
                    // a full block ahead of where the previous one finished, so
                    // the stretch is discarded at every boundary.
                    //
                    // Without this the factor degenerates to pure varispeed —
                    // 2.0x turns 440 Hz into 880 Hz with the duration unchanged.
                    // Folding the rate into the step *only* is worse still
                    // (measured: pitch 35% off, purity 0.95 -> 0.54), because
                    // origin and step then disagree within each block as well as
                    // across them.
                    //
                    // Both come from `seated_position`, which seats at
                    // `stretched_window_position` and steps by `read_rate`
                    // with the same stretch — the two must be derived together
                    // or they drift apart again. (The step is `read_rate`, not
                    // the gate's `window_rate`: a file at another rate than the
                    // session's moves `src_ratio` file frames per output frame.)
                    let stretch_rate = unit.input_rate();
                    for i in 0..size {
                        let Some(pos) = sampler.seated_position(stretch_rate) else {
                            // Outside the window: nothing to feed, and the filter
                            // keeps what it holds, as it did when a block
                            // outside the window returned here whole.
                            continue;
                        };
                        read_clip_sample_into(sampler, direction, pos, gain, &mut raw[..n]);
                        frame[..n].fill(0.0);
                        unit.tick(&raw[..n], &mut frame[..n]);
                        mix_in!(frame, i);
                    }
                }
                VoiceSource::Disk(reader) => {
                    // The disk tier has no cursor to scale — it pops from the
                    // butler ring and advances its own `fractional_pos`. So the
                    // stretch rate is published into the `RtState` both sides
                    // share, where it composes with varispeed and `src_ratio` at
                    // the one point all three of the reader's consumers read
                    // (per-sample advance in `tick` and `process`, plus the
                    // `samples_needed` fetch estimate that must agree with them).
                    //
                    // Published per block rather than on the parameter-change
                    // path because the rate is the *filter's*, and a voice
                    // returned to unity must publish unity again — a set-once
                    // would leave the ring draining at the old factor.
                    reader.set_stretch_rate(unit.input_rate());
                    for i in 0..size {
                        raw[..n].fill(0.0);
                        reader.tick(&[], &mut raw[..n]);
                        frame[..n].fill(0.0);
                        unit.tick(&raw[..n], &mut frame[..n]);
                        mix_in!(frame, i);
                    }
                }
            }
        } else {
            match &mut self.voice.source {
                VoiceSource::Memory(sampler) => {
                    // Seated at the gate's origin (`window_rate`, varispeed
                    // alone, in the wave's own frames) and stepped by
                    // `read_rate` (varispeed and `src_ratio`), per frame: see
                    // `MemorySource::seated_position`. Stepping by the gate's
                    // rate read a 24 kHz file on a 48 kHz clock one file frame
                    // per output frame, then jumped back at every block.
                    for i in 0..size {
                        let Some(pos) = sampler.seated_position(ReadRate::UNITY) else {
                            continue;
                        };
                        read_clip_sample_into(sampler, direction, pos, gain, &mut frame[..n]);
                        mix_in!(frame, i);
                    }
                }
                VoiceSource::Disk(reader) => {
                    // Unity, every block: the stretch rate belongs to the filter
                    // rather than to the voice, so a voice returned to 1.0x — or
                    // one whose filter was removed — must publish unity or its
                    // ring keeps draining at the old factor. `set_stretch` keeps
                    // the resident filter and only rewrites its atomics, so this
                    // is reachable through ordinary use, not just teardown.
                    reader.set_stretch_rate(ReadRate::UNITY);
                    // Sum the reader per-sample (its placement gate + ring
                    // pull run inside each `tick`). Per-sample accumulation
                    // mirrors the stretch branch above and keeps this
                    // alloc-free — no per-slot scratch `BufferMut`.
                    for i in 0..size {
                        frame[..n].fill(0.0);
                        reader.tick(&[], &mut frame[..n]);
                        mix_in!(frame, i);
                    }
                }
            }
        }
    }
}

/// Read the reversed-or-forward source sample from an in-memory `MemorySource`, scaled
/// by the voice's `gain`. Free function so both the mixer/`VoiceNode` read and
/// the stretch feed share it.
///
/// The `MemorySource` is a PURE producer here: it reads the raw interpolated
/// frame via `get_sample_raw` (no gain), and gain is applied ONCE at this
/// Voice/Playback level from `gain` (mirrored from `voice.play.gain`). This
/// matches the streaming tier, where gain lives in the source's shared state,
/// and keeps a single, well-defined gain application point per tier.
///
/// The read itself — reverse, and a loop — is the source's own
/// ([`MemorySource::read_placed_into`]), so a bare placed source and one in a
/// slot read a position the same way.
#[inline]
fn read_clip_sample_into(
    sampler: &MemorySource,
    direction: Direction,
    pos: SamplePosition,
    gain: Amplitude,
    out: &mut [f32],
) {
    sampler.read_placed_into(pos, direction, out);
    let g = gain.get();
    for s in out.iter_mut() {
        *s *= g;
    }
}

/// Read ONE frame from a single voice source into `out`, using the exact
/// per-variant read the direct (non-stretch) path uses — the `VoiceSource` enum
/// still owns the read. `gain` scales the in-memory read at the Voice level (see
/// [`read_clip_sample_into`]); the streaming reader applies its own gain
/// internally. Writes every element of `out`. Used to feed the stretch filter
/// (which owns no source) on the hot path; `stretch_rate` is the filter's, which
/// a placed memory read seats and steps by, as `process` does.
#[inline]
fn read_source_frame_into(
    source: &mut VoiceSource,
    direction: Direction,
    gain: Amplitude,
    stretch_rate: ReadRate,
    out: &mut [f32],
) {
    match source {
        VoiceSource::Memory(sampler) => match sampler.seated_position(stretch_rate) {
            Some(pos) => read_clip_sample_into(sampler, direction, pos, gain, out),
            None => out.fill(0.0),
        },
        VoiceSource::Disk(reader) => {
            // `AudioUnit::tick` writes only as many channels as the unit has;
            // clear first so a narrower reader leaves silence, not stale data.
            out.fill(0.0);
            reader.tick(&[], out);
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
