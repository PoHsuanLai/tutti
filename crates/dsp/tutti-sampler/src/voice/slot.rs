//! [`VoiceSlot`] — a [`Voice`] plus its resident time-stretch processor, and the
//! per-sample read that turns the pair into audio.
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
    Samples, StretchFactor,
};

// ---------------------------------------------------------------------------
// VoiceSlot — a `Voice` plus the resident time-stretch DSP processor. The
// processor is the DSP object (not config), so it stays on the slot alongside
// the engine sample rate; the control intent lives in `voice.play`.
// ---------------------------------------------------------------------------

pub(crate) struct VoiceSlot {
    pub(crate) id: SlotId,
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
    /// **"Built once" is not the same as "built off the audio thread", and this
    /// distinction used to be blurred here.** Per-buffer *updates* are genuinely
    /// allocation-free — [`VoiceCommand::UpdateStretch`] only sets atomics. But
    /// the construction itself happens in `insert_voice`, reached from
    /// [`VoiceCommand::AddVoice`], which `drain_commands` pulls from `tick` /
    /// `process`. So adding a voice mid-playback DOES build the vocoders in the
    /// callback, and at width `n` that is `n` FFT setups plus `2n` scratch
    /// allocations rather than the stereo pair the old wording implied.
    ///
    /// Pre-existing, and not made reachable by the width work — but the cost now
    /// scales with channel count, so it is worth stating plainly instead of
    /// leaving the reader to infer safety.
    ///
    /// Structural invariant: `voice.play.stretch` / `voice.play.pitch` cannot
    /// drift from the processor's atomics — every mutation goes through
    /// [`VoiceSlot::set_stretch`], which writes both in one step.
    pub(crate) stretch: Option<stretch::Unit>,
    /// Width the stretch unit must be built at, remembered so a later
    /// materialisation matches the reader rather than defaulting.
    pub(crate) channels: ChannelLayout,
    pub(crate) sample_rate: SampleRate,
}

impl VoiceSlot {
    /// Build a slot. Width is explicit at every call site — there is no
    /// stereo-defaulting `new`, because both callers (the reader's drain and
    /// `VoiceNode`) know their own width and a default here would silently
    /// mismatch it.
    ///
    /// The stretch unit is **not** built here. It is `None` until the slot
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
    /// A slot that arrives already needing stretch (non-unity `play.stretch` /
    /// `play.pitch`) is given its unit by the SENDER, on the control thread,
    /// before the command is queued — see `VoicePoolHandle::prepare`, which
    /// fills in `VoiceCommand::AddVoice`'s `stretch` field. Turning stretch on
    /// *later* goes through the same door via `VoiceCommand::UpdateStretch`.
    ///
    /// (Both used to be described as `VoiceSlot::materialize_stretch`, a method
    /// that has never existed. That dangling name is why the update path went
    /// unbuilt: every reader took the doc's word that a filter would arrive.)
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

    /// Read ONE mixed stereo frame from this slot: the exact per-variant read the
    /// `tick` mixdown does for a single slot, routed through the resident stretch
    /// filter when `needs_stretch()`. Factored so the mixer loop and the
    /// standalone [`VoiceNode`] share one definition — no duplication, no dyn.
    /// Alloc-free: returns a stack frame. RT: the `VoiceSource` enum match is
    /// unchanged, only relocated here.
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
            if let (VoiceSource::Disk(reader), Some(unit)) =
                (&self.voice.source, self.stretch.as_ref())
            {
                reader.set_stretch_rate(unit.input_rate());
            }
            read_source_frame_into(&mut self.voice.source, direction, gain, &mut raw[..n]);
            out.fill(0.0);
            if let Some(unit) = &mut self.stretch {
                unit.tick(&raw[..n], out);
            }
        } else {
            match &mut self.voice.source {
                VoiceSource::Memory(sampler) => match sampler.window_position() {
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

    /// Accumulate this slot's block-rate contribution into `output`: the exact
    /// per-variant read the `process` mixdown does for a single slot. Factored so
    /// the mixer loop and the standalone [`VoiceNode`] share one definition. RT:
    /// the `VoiceSource` enum match is unchanged, only relocated here.
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
                    // Without this the factor behaved as pure varispeed — 2.0x
                    // turned 440 Hz into 880 Hz with the duration unchanged.
                    // Folding the rate into the step *only* was worse still
                    // (pitch +35% off, purity 0.95 -> 0.54): origin and step then
                    // disagreed within each block as well as across them.
                    //
                    // Both come from `stretched_window_position` / the same
                    // composed rate for exactly that reason — the two must be
                    // derived together or they drift apart again.
                    let rate = sampler.window_rate().then(unit.input_rate());
                    let Some(start_pos) = sampler.stretched_window_position(unit.input_rate())
                    else {
                        return;
                    };
                    for i in 0..size {
                        let pos = start_pos + rate.advance(Samples(i));
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
                    let Some(start_pos) = sampler.window_position() else {
                        return;
                    };
                    // `window_rate`, not a hand-rolled `speed * src_ratio`: this
                    // site multiplied the two by hand, which double-applied
                    // `src_ratio` against a gate origin that had already resolved
                    // it. The named method is what keeps origin and step matched.
                    let rate = sampler.window_rate();
                    for i in 0..size {
                        let pos = start_pos + rate.advance(Samples(i));
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
#[inline]
fn read_clip_sample_into(
    sampler: &MemorySource,
    direction: Direction,
    pos: SamplePosition,
    gain: Amplitude,
    out: &mut [f32],
) {
    match direction {
        Direction::Reverse => {
            let len = sampler.duration_samples() as f64;
            let reversed = (len - 1.0 - pos.get()).max(0.0);
            sampler.get_sample_raw_into(reversed, out);
        }
        Direction::Forward => sampler.get_sample_raw_into(pos.get(), out),
    }
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
/// (which owns no source) on the hot path.
#[inline]
fn read_source_frame_into(
    source: &mut VoiceSource,
    direction: Direction,
    gain: Amplitude,
    out: &mut [f32],
) {
    match source {
        VoiceSource::Memory(sampler) => match sampler.window_position() {
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
/// decide whether to build a filter) and [`VoiceSlot::needs_stretch`] (to decide
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
