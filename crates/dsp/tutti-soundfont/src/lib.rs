//! SoundFont (.sf2) synthesis via RustySynth.
//!
//! Build a [`SoundFontUnit`] with [`SoundFontUnit::new`] from a decoded
//! `SoundFont` and a [`SynthesizerSettings`], then
//! [`program_change`](SoundFontUnit::program_change) to pick the preset and
//! channel. A host that wants asset-managed loading wires it in its own adapter
//! layer; this crate only needs the decoded `SoundFont`.
//!
//! Zero inputs, two outputs — the unit *is* the source, so it enters a `Net`
//! with only its output piped.
//!
//! Notes arrive through the unit's [`MidiInPort`], reached via
//! [`midi_sender`](SoundFontUnit::midi_sender) or
//! [`midi_port`](SoundFontUnit::midi_port), and are applied at their own
//! `frame_offset` within a block — to an 8-frame resolution, which is
//! RustySynth's floor rather than this crate's choice; see
//! [`SoundFontUnit::process`] and [`SYNTH_BLOCK_FRAMES`].
//! [`note_on`](SoundFontUnit::note_on) / [`note_off`](SoundFontUnit::note_off)
//! bypass that inbox and are **not** the intended path — see their own docs and
//! the README.
//!
//! The quick start, the fixed-rate trap, the timing-resolution floor and the
//! 7-bit resolution boundary are in the crate README, included below.
#![doc = include_str!("../README.md")]

mod error;
pub use error::{Error, Result};

mod node_id;

pub use rustysynth::{SoundFont, SoundFontError, SynthesizerSettings};

use rustysynth::Synthesizer;
use tutti_core::Arc;
use tutti_core::{AudioUnit, BufferMut, BufferRef, SampleRate, Setting, SignalFrame};
use tutti_midi_runtime::{MidiInPort, MidiSender};
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiUnitId, MidiUnitIn};

/// Capacity of the scratch buffer used to poll MIDI events per audio callback.
///
/// `poll_into` takes `&mut [MidiEvent]` and iterates over existing slots, so the
/// buffer must be fully initialised (not just allocated with `with_capacity`).
const MIDI_BUFFER_CAPACITY: usize = 256;

/// Frames of de-interleaved render scratch held per channel.
///
/// `AudioUnit::process` is contractually capped at [`tutti_core::MAX_BUFFER_SIZE`]
/// — a `BufferMut` holds exactly one SIMD block per channel — so this is the
/// largest block that can arrive. Allocated once in [`SoundFontUnit::new`] and
/// never resized, which is what keeps `process` allocation-free; a `size` past
/// it is clamped rather than growing the buffer on the audio thread.
const RENDER_SCRATCH_FRAMES: usize = tutti_core::MAX_BUFFER_SIZE;

/// The rustysynth `block_size` this unit builds its `Synthesizer` at, in frames.
///
/// **This is half of the frame-offset fix, and the half that is not obvious.**
/// Splitting the block at each event offset (see [`SoundFontUnit::process`]) is
/// necessary but not sufficient: `Synthesizer::render` accepts any length, but
/// it serves those frames out of an internal `block_size` chunk that
/// `render_block` fills *whole*. Voices render a full chunk at a time and mix
/// gains ramp across it, so a note applied part-way into an already-rendered
/// chunk cannot affect it. `block_size` is therefore the floor on this unit's
/// MIDI timing resolution, and it was rustysynth's default of **64** — one
/// whole `MAX_BUFFER_SIZE` block, which is why offsets 16, 32 and 48 inside a
/// 64-frame block used to produce byte-identical output.
///
/// 8 is the finest rustysynth accepts (`SynthesizerSettings::check_block_size`
/// rejects anything outside `8..=1024`), so **timing resolution is 8 frames,
/// not 1** — 0.18 ms at 44.1 kHz, against the 1.45 ms it was. See
/// [`SoundFontUnit::process`] for what that means for a caller.
///
/// # What it costs, measured
///
/// `block_size` is a resolution knob rather than a correctness one: rendering
/// the same note at 8 vs 64 differs by at most 6.8e-4 against a signal RMS of
/// 1.3e-2 (~5%), from finer gain-ramp granularity and the chorus/reverb line
/// sizing — no algorithm changes, and only `voice.block` is sized from it.
///
/// CPU, release build, 8 sustained voices, per 64-frame block: 5.8 µs at
/// `block_size` 64, 10.7 µs at 8. That is 0.40% → 0.74% of the real-time
/// budget for those frames — around 5 µs bought for correct MIDI timing.
pub const SYNTH_BLOCK_FRAMES: usize = 8;

/// A stereo `AudioUnit` that renders MIDI through a decoded SoundFont.
///
/// Zero inputs, two outputs. Events arrive through the [`MidiInPort`] returned
/// by [`Self::midi_port`] and are applied at their own `frame_offset` within a
/// block, to the 8-frame resolution [`SYNTH_BLOCK_FRAMES`] explains.
///
/// # The sample rate is fixed for the unit's lifetime
///
/// The rate is set once from `SynthesizerSettings::sample_rate` in
/// [`new`](Self::new) and cannot change afterwards: RustySynth builds its voice
/// tables against a rate at construction and offers no way to re-rate them, so
/// [`AudioUnit::set_sample_rate`] is a deliberate no-op here rather than a
/// missing implementation.
///
/// This is the one trap the type carries, because the graph will not complain.
/// A unit built at 44.1 kHz and run in a 48 kHz graph keeps rendering — every
/// note simply plays at the wrong pitch and tempo, with no error at any layer.
/// A rate change means constructing a new unit and swapping it into the graph,
/// not reconfiguring this one.
pub struct SoundFontUnit {
    synthesizer: Synthesizer,
    sample_rate: SampleRate,
    /// De-interleaved scratch for one `process` block. Sized once at
    /// construction to [`RENDER_SCRATCH_FRAMES`] and never resized, so the
    /// audio thread never allocates. `process` renders into `[..size]` in
    /// segments split at each pending event's offset.
    left_buffer: Vec<f32>,
    right_buffer: Vec<f32>,
    /// This unit's MIDI input endpoint (routing address + mailbox + current pull
    /// source). See [`MidiInPort`] for the fundsp clone/isolate sharing semantics.
    midi: MidiInPort,
    midi_buffer: Vec<MidiEvent>,
}

impl SoundFontUnit {
    /// Builds a unit over a decoded `SoundFont`.
    ///
    /// The sample rate is fixed here from `settings.sample_rate`: RustySynth
    /// cannot be re-rated afterwards, so [`AudioUnit::set_sample_rate`] is a
    /// no-op on this unit. A graph running at a different rate needs a new unit,
    /// not a reconfigured one.
    ///
    /// # `settings.block_size` is overridden
    ///
    /// Whatever the caller passes, the synthesizer is built at
    /// [`SYNTH_BLOCK_FRAMES`] — see that constant for why. Every other field of
    /// `settings` (sample rate, polyphony, reverb/chorus) is honoured as given.
    ///
    /// # Errors
    ///
    /// Returns [`Error::SoundFont`] if RustySynth refuses the `SoundFont` +
    /// [`SynthesizerSettings`] pair.
    pub fn new(soundfont: Arc<SoundFont>, settings: &SynthesizerSettings) -> Result<Self> {
        // `SynthesizerSettings` is `#[non_exhaustive]`, so it is rebuilt from
        // `new` rather than struct-updated from the caller's copy.
        let mut settings_owned = SynthesizerSettings::new(settings.sample_rate);
        settings_owned.maximum_polyphony = settings.maximum_polyphony;
        settings_owned.enable_reverb_and_chorus = settings.enable_reverb_and_chorus;
        settings_owned.block_size = SYNTH_BLOCK_FRAMES;

        let synthesizer = Synthesizer::new(&soundfont, &settings_owned)
            .map_err(|e| Error::SoundFont(e.to_string()))?;

        Ok(Self {
            synthesizer,
            // `SynthesizerSettings::sample_rate` is rustysynth's `i32`. The
            // conversion into the engine's vocabulary happens here rather than
            // being pushed onto callers.
            sample_rate: SampleRate::from(settings.sample_rate.max(0) as u32),
            left_buffer: vec![0.0; RENDER_SCRATCH_FRAMES],
            right_buffer: vec![0.0; RENDER_SCRATCH_FRAMES],
            midi: MidiInPort::new(),
            midi_buffer: vec![MidiEvent::noop(); MIDI_BUFFER_CAPACITY],
        })
    }

    /// This unit's MIDI input endpoint — routing address, push mailbox, and the
    /// source-install slot, in one borrow.
    ///
    /// The whole-port accessor exists so a host can reach all three through a
    /// single downcast. Resolving a unit's MIDI identity means asking the unit,
    /// and asking three times for three halves of one endpoint invites a caller
    /// to cache one of them — which is how an id goes stale across a
    /// `crossfade` that keeps the graph node but mints a new port.
    pub fn midi_port(&self) -> &MidiInPort {
        &self.midi
    }

    /// Producer handle for this unit's MIDI inbox.
    pub fn midi_sender(&self) -> MidiSender {
        self.midi.sender()
    }

    /// Layer a MIDI source over the live inbox. Used by offline export for a
    /// [`MidiSnapshotReader`], or by clip playback for a
    /// [`tutti_midi_runtime::MidiClipSource`]. Both the source and the inbox
    /// are polled, so clip playback does not silence live input.
    ///
    /// The install is visible across fundsp's clone-on-commit (see
    /// [`MidiInPort`]), so the same source reaches the box the audio thread runs.
    ///
    /// [`MidiSnapshotReader`]: tutti_midi_runtime::MidiSnapshotReader
    pub fn set_midi_source(&mut self, source: Arc<dyn MidiUnitIn>) {
        self.midi.install(source);
    }

    /// Removes any layered MIDI source, leaving the live inbox as the only feed.
    ///
    /// Detaches the source; it does not flush events already in the inbox.
    pub fn clear_midi_source(&mut self) {
        self.midi.clear();
    }

    /// The rate this unit renders at, fixed at construction from
    /// [`SynthesizerSettings`].
    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// Starts a note directly, bypassing the MIDI inbox.
    ///
    /// **Not the intended path.** Notes should reach this unit through
    /// [`midi_sender`](Self::midi_sender); this pair has none of the inbox's
    /// properties. There is no `frame_offset`, so a note lands at the start of
    /// whatever block follows rather than where it was placed; a `MidiBus` cannot
    /// address it; and `&mut self` puts it out of reach once the unit is in a
    /// `Net`. The peer crate `tutti-polysynth` exposes no such pair.
    ///
    /// **Public only because `bevy-tutti` still tests through it.** Roughly ten
    /// call sites in `bevy_tutti::soundfont`'s test module drive notes this way
    /// rather than through the inbox, so narrowing this to `pub(crate)` would
    /// break them. Those tests are what the narrowing waits on: port them to
    /// `midi_sender` first, and the pair can go crate-private in the same change
    /// — nothing else outside this crate calls it.
    ///
    /// These are RustySynth's MIDI 1.0 integers, not the engine's MIDI 2.0
    /// vocabulary: `channel` is 0..16, `key` and `velocity` are 7-bit (0..128).
    /// A `velocity` of 0 reads as a note-off to RustySynth.
    pub fn note_on(&mut self, channel: i32, key: i32, velocity: i32) {
        self.synthesizer.note_on(channel, key, velocity);
    }

    /// Releases a note directly, bypassing the MIDI inbox.
    ///
    /// **Not the intended path** — see [`note_on`](Self::note_on) for why.
    ///
    /// `channel` is 0..16 and `key` is 7-bit (0..128), per MIDI 1.0.
    pub fn note_off(&mut self, channel: i32, key: i32) {
        self.synthesizer.note_off(channel, key);
    }

    /// Selects the preset a channel plays, bypassing the MIDI inbox.
    ///
    /// `channel` is 0..16 and `preset` is the 7-bit program number (0..128)
    /// within the SoundFont's current bank.
    pub fn program_change(&mut self, channel: i32, preset: i32) {
        self.synthesizer
            .process_midi_message(channel, 0xC0, preset, 0);
    }

    /// Render exactly `range` of this block's scratch buffers, advancing the
    /// synthesizer by `range.len()` frames.
    ///
    /// This is the whole of the frame-offset fix. `Synthesizer::render` accepts
    /// a buffer of **any** length and owns its own 64-frame chunk cursor
    /// (`block_read`), refilling only when that cursor runs out — so rendering
    /// `[0, 16)` then `[16, 64)` is sample-for-sample identical to rendering
    /// `[0, 64)` in one call *given the same synthesizer state*, and any state
    /// change made between the two segments takes effect at frame 16 exactly.
    ///
    /// The previous shape kept a second 64-frame buffer here and refilled it
    /// whole, which is what made offsets 16, 32 and 48 within a 64-frame block
    /// byte-identical: the chunk carrying those frames had already been rendered
    /// before the event was applied, so the note could only sound from the next
    /// chunk. There is no such buffer any more; the only chunking left is
    /// rustysynth's own, and it is invisible because it survives across calls.
    fn render_range(&mut self, range: core::ops::Range<usize>) {
        if range.is_empty() {
            return;
        }
        self.synthesizer.render(
            &mut self.left_buffer[range.clone()],
            &mut self.right_buffer[range],
        );
    }

    /// Poll this block's events into `midi_buffer`, sorted by `frame_offset`,
    /// and return the count. Does **not** dispatch — the caller applies each
    /// event at its offset (see [`Self::process`]) rather than collapsing every
    /// event to the block start.
    fn poll_midi_events_sorted(&mut self, block_size: usize) -> usize {
        let count = self
            .midi
            .poll(block_size, self.sample_rate, &mut self.midi_buffer);
        if count > 1 {
            self.midi_buffer[..count].sort_unstable_by_key(|e| e.frame_offset);
        }
        count
    }

    /// Normalize one polled event into MIDI 2.0 vocabulary, then hand it to
    /// [`Self::dispatch`], which is where the downscale to 7-bit happens.
    fn apply_event(&mut self, event: &MidiEvent) {
        self.dispatch(&tutti_midi_types::normalize(event));
    }

    /// Apply one MIDI 2.0 event to the synthesizer, downscaling to MIDI 1.0.
    ///
    /// This is the crate's resolution boundary: values narrow through the spec's
    /// Min-Center-Max converters in `tutti_midi_types::convert`.
    ///
    /// # Translated
    ///
    /// NoteOn / NoteOff, control change, channel pitch bend, program change.
    ///
    /// # Dropped
    ///
    /// Anything with no RustySynth MIDI 1.0 analogue falls through the `_` arm:
    /// per-note pitch bend, per-note controllers, per-note management
    /// (Detach / Reset), channel and poly pressure, RPN/NRPN, and any 16-bit
    /// velocity or 32-bit CC precision beyond 7 bits. Channel and key pressure
    /// arrive as well-formed UMP but RustySynth exposes no setter for them, so
    /// they are dropped rather than approximated.
    fn dispatch(&mut self, event: &MidiEvent) {
        use tutti_midi_types::convert::{
            midi2_cc_to_midi1, midi2_pitch_bend_to_midi1, midi2_velocity_to_midi1,
        };
        use tutti_midi_types::midi2::channel_voice2::ChannelVoice2 as Cv2;
        use tutti_midi_types::midi2::{Channeled, UmpMessage};

        let Ok(UmpMessage::ChannelVoice2(cv2)) = UmpMessage::try_from(event.data_words()) else {
            return;
        };
        let ch = i32::from(u8::from(cv2.channel()));
        match cv2 {
            Cv2::NoteOn(m) => {
                // A zero after downscale would read as NoteOff to rustysynth;
                // `normalize` already folded true velocity-0 to NoteOff, so any
                // NoteOn here is audible — clamp the 7-bit floor to 1.
                let vel_u7 = midi2_velocity_to_midi1(m.velocity()).max(1);
                self.synthesizer.note_on(
                    ch,
                    i32::from(u8::from(m.note_number())),
                    i32::from(vel_u7),
                );
            }
            Cv2::NoteOff(m) => {
                self.synthesizer
                    .note_off(ch, i32::from(u8::from(m.note_number())));
            }
            Cv2::ProgramChange(m) => {
                self.synthesizer.process_midi_message(
                    ch,
                    0xC0,
                    i32::from(u8::from(m.program())),
                    0,
                );
            }
            Cv2::ChannelPitchBend(m) => {
                let bend14 = midi2_pitch_bend_to_midi1(m.pitch_bend_data());
                let lsb = i32::from(bend14 & 0x7F);
                let msb = i32::from((bend14 >> 7) & 0x7F);
                self.synthesizer.process_midi_message(ch, 0xE0, lsb, msb);
            }
            Cv2::ControlChange(m) => {
                self.synthesizer.process_midi_message(
                    ch,
                    0xB0,
                    i32::from(u8::from(m.control())),
                    i32::from(midi2_cc_to_midi1(m.control_change_data())),
                );
            }
            _ => {}
        }
    }
}

impl AudioUnit for SoundFontUnit {
    /// Release every key on every channel.
    ///
    /// Note-off rather than `Synthesizer::reset`: voices are released into their
    /// envelopes rather than cut, which is what
    /// `test_reset_silences_all_notes` measures (silence *after* the decay, not
    /// immediately). There is no buffer state to discard alongside it — the
    /// scratch buffers are fully rewritten by every `process`, and rustysynth
    /// owns the only surviving chunk cursor.
    fn reset(&mut self) {
        (0..16).for_each(|channel| {
            (0..128).for_each(|key| {
                self.synthesizer.note_off(channel, key);
            });
        });
    }

    /// Sever the live MIDI input this clone shares with the original synth.
    ///
    /// Same rationale as `tutti_polysynth::PolySynth::isolate`: an offline render
    /// ticks this clone on a worker thread while the live synth plays, so a shared
    /// inbox would let the worker *steal* the live synth's events and a shared
    /// source cell would let clearing here sever the live clip.
    /// [`MidiInPort::isolate`] mints a fresh private mailbox + source cell so this
    /// clone reads nothing.
    fn isolate(&mut self) {
        self.midi.isolate();
    }

    fn set_sample_rate(&mut self, _sample_rate: tutti_core::SampleRate) {
        // RustySynth sample rate is fixed at construction
    }

    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        assert_eq!(output.len(), 2, "SoundFontUnit is stereo (2 outputs)");
        // Single-sample block: every event lands at this one sample.
        let count = self.poll_midi_events_sorted(1);
        for i in 0..count {
            let event = self.midi_buffer[i];
            self.apply_event(&event);
        }

        // One frame, rendered directly. rustysynth's own 64-frame chunk cursor
        // survives across calls, so a one-frame render is not a one-frame chunk
        // — it consumes one frame of the current chunk and refills only when
        // that runs out.
        self.render_range(0..1);
        output[0] = self.left_buffer[0];
        output[1] = self.right_buffer[0];
    }

    /// Render `size` frames, applying each polled MIDI event at its own
    /// `frame_offset`.
    ///
    /// # How the offset is honoured
    ///
    /// The block is **split at every distinct pending event offset**. For events
    /// at offsets `o1 < o2 < …`, the sequence is: render `[0, o1)`, apply every
    /// event at `o1`, render `[o1, o2)`, apply every event at `o2`, … , render
    /// the tail to `size`. An event at offset `o` therefore affects the sample
    /// at `o` and every sample after it, and no sample before it.
    ///
    /// # The resolution this actually achieves
    ///
    /// **8 frames, not 1.** The split above is exact, but rustysynth serves
    /// frames out of a [`SYNTH_BLOCK_FRAMES`]-frame internal chunk it fills
    /// whole, so two offsets inside one such chunk still collapse together.
    /// Offsets 0, 8, 16, … resolve distinctly and each shifts the output by
    /// exactly its own delta; offsets 16 and 20 do not. That is a floor
    /// rustysynth imposes — 8 is the smallest `block_size` it accepts — and the
    /// improvement over the 64-frame chunk this unit used to have is 8×.
    ///
    /// Callers that need finer than 0.18 ms (at 44.1 kHz) cannot get it from
    /// this unit without a change inside the vendored synthesizer.
    ///
    /// # Ordering
    ///
    /// Two rules, and both are load-bearing:
    ///
    /// 1. **Apply before rendering the frames the event governs, never after.**
    ///    `Synthesizer::render` advances state; a note applied after the frames
    ///    it should sound in is one segment late, which is exactly the defect
    ///    this shape replaces. Every `render_range` call below is preceded by
    ///    the application of every event whose offset is `<=` that segment's
    ///    first frame.
    /// 2. **Events at the same offset apply in inbox order.**
    ///    `poll_midi_events_sorted` sorts with `sort_unstable_by_key`,
    ///    which is not stable, so equal offsets could be reordered against each
    ///    other. That is tolerable here and nowhere else: the loop applies the
    ///    whole equal-offset run before rendering a single frame, so no ordering
    ///    within the run is observable in the output unless the two events
    ///    contradict each other at the same sample (a note-on and note-off of
    ///    one key at one offset), which is already an ill-formed stream.
    ///
    /// An offset at or past `size` is clamped to the last frame rather than
    /// dropped, so a stray late offset still fires within this block.
    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        // The scratch is sized for `MAX_BUFFER_SIZE`, which is the contractual
        // ceiling on `size`. Clamp rather than grow: growing here would allocate
        // on the audio thread, and a caller past the ceiling is already outside
        // the `BufferMut` contract.
        let size = size.min(RENDER_SCRATCH_FRAMES);
        if size == 0 {
            return;
        }

        let count = self.poll_midi_events_sorted(size);
        let mut event_idx = 0;
        let mut pos = 0usize;
        while pos < size {
            // Apply every event due at or before `pos` — the equal-offset run
            // lands in full before any of the frames it governs is rendered.
            while event_idx < count
                && (self.midi_buffer[event_idx].frame_offset as usize).min(size - 1) <= pos
            {
                let event = self.midi_buffer[event_idx];
                self.apply_event(&event);
                event_idx += 1;
            }

            // Render up to the next event's offset, so the segment [pos, next)
            // carries exactly the state the events at `pos` established.
            let next = if event_idx < count {
                (self.midi_buffer[event_idx].frame_offset as usize)
                    .min(size - 1)
                    .max(pos + 1)
            } else {
                size
            };
            self.render_range(pos..next);
            pos = next;
        }

        for i in 0..size {
            output.set_f32(0, i, self.left_buffer[i]);
            output.set_f32(1, i, self.right_buffer[i]);
        }
    }

    fn inputs(&self) -> usize {
        0
    }

    fn outputs(&self) -> usize {
        2
    }

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        SignalFrame::new(self.outputs())
    }

    fn set(&mut self, _setting: Setting) {}

    fn get_id(&self) -> u64 {
        node_id::SOUNDFONT_ID
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
            + self.left_buffer.capacity() * core::mem::size_of::<f32>()
            + self.right_buffer.capacity() * core::mem::size_of::<f32>()
    }

    fn allocate(&mut self) {}
}

impl Clone for SoundFontUnit {
    fn clone(&self) -> Self {
        Self {
            synthesizer: self.synthesizer.clone(),
            sample_rate: self.sample_rate,
            // Scratch, not state: sized fresh rather than copied, since every
            // `process` overwrites the frames it reads back.
            left_buffer: vec![0.0; RENDER_SCRATCH_FRAMES],
            right_buffer: vec![0.0; RENDER_SCRATCH_FRAMES],
            // Shares the mailbox + source cell (fundsp clone-on-commit); see
            // [`MidiInPort`]. `isolate()` severs it for an offline render.
            midi: self.midi.clone(),
            midi_buffer: vec![MidiEvent::noop(); MIDI_BUFFER_CAPACITY],
        }
    }
}

impl SoundFontUnit {
    /// This unit's MIDI routing address.
    pub fn midi_unit_id(&self) -> MidiUnitId {
        self.midi.unit_id()
    }
}
