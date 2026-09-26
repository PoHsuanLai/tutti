//! SoundFont (.sf2) synthesis via RustySynth.
//!
//! Build a [`SoundFontUnit`] with [`SoundFontUnit::new`] from a decoded
//! `SoundFont` and a [`SynthesizerSettings`], then
//! [`program_change`](SoundFontUnit::program_change) to pick the preset and
//! channel. A host that wants asset-managed loading wires it in its own adapter
//! layer; this crate only needs the decoded `SoundFont`.
//!
//! Zero inputs, two outputs — the unit *is* the source. It is a native graph
//! node (`tutti_graph::Node`): notes arrive on its event input, each applied
//! at its own offset within a block — to an 8-frame resolution, which is
//! RustySynth's floor rather than this crate's choice; see
//! [`SoundFontUnit`]'s "How an event's offset is honoured" and
//! [`SYNTH_BLOCK_FRAMES`].
//! [`note_on`](SoundFontUnit::note_on) / [`note_off`](SoundFontUnit::note_off)
//! bypass that timing and are **not** the intended path — see their own docs
//! and the README.
//!
//! The quick start, the fixed-rate trap, the timing-resolution floor and the
//! 7-bit resolution boundary are in the crate README, included below.
#![doc = include_str!("../README.md")]

mod error;
pub use error::{Error, Result};

// `SoundFontUnit::fork_instance`: the unit in a fork of the native graph
// (an export), with its clip.
mod fork;
mod node;

pub use rustysynth::{SoundFont, SoundFontError, SynthesizerSettings};

use rustysynth::Synthesizer;
use tutti_core::Arc;
use tutti_core::SampleRate;
use tutti_midi_types::ump::MidiEvent;

/// Capacity of the block's MIDI scratch, in events: past it a block's events
/// are dropped. Fully initialised, since events are written into its slots.
const MIDI_BUFFER_CAPACITY: usize = 256;

/// The rustysynth `block_size` this unit builds its `Synthesizer` at, in frames.
///
/// **This is half of the frame-offset fix, and the half that is not obvious.**
/// Splitting the block at each event offset (see [`SoundFontUnit`]'s "How an
/// event's offset is honoured") is
/// necessary but not sufficient: `Synthesizer::render` accepts any length, but
/// it serves those frames out of an internal `block_size` chunk that
/// `render_block` fills *whole*. Voices render a full chunk at a time and mix
/// gains ramp across it, so a note applied part-way into an already-rendered
/// chunk cannot affect it. `block_size` is therefore the floor on this unit's
/// MIDI timing resolution, and it was rustysynth's default of **64** — one
/// whole 64-frame block, which is why offsets 16, 32 and 48 inside a
/// 64-frame block used to produce byte-identical output.
///
/// 8 is the finest rustysynth accepts (`SynthesizerSettings::check_block_size`
/// rejects anything outside `8..=1024`), so **timing resolution is 8 frames,
/// not 1** — 0.18 ms at 44.1 kHz, against the 1.45 ms it was. See
/// [`SoundFontUnit`]'s "How an event's offset is honoured" for what that
/// means for a caller.
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

/// A stereo graph node that renders MIDI through a decoded SoundFont.
///
/// Zero inputs, two outputs, one MIDI event input. Events are applied at
/// their own offset within a block, to the 8-frame resolution
/// [`SYNTH_BLOCK_FRAMES`] explains.
///
/// # How an event's offset is honoured
///
/// The block is **split at every distinct event offset**. For events at
/// offsets `o1 < o2 < …`, the sequence is: render `[0, o1)`, apply every event
/// at `o1`, render `[o1, o2)`, apply every event at `o2`, … , render the tail
/// to the block's end. An event at offset `o` therefore affects the sample at
/// `o` and every sample after it, and no sample before it.
///
/// **8 frames, not 1.** The split is exact, but rustysynth serves frames out
/// of a [`SYNTH_BLOCK_FRAMES`]-frame internal chunk it fills whole, so two
/// offsets inside one such chunk still collapse together. Offsets 0, 8, 16, …
/// resolve distinctly and each shifts the output by exactly its own delta;
/// offsets 16 and 20 do not. That is a floor rustysynth imposes — 8 is the
/// smallest `block_size` it accepts — and the node declares it
/// (`Resolution::Frames(8)`). Callers that need finer than 0.18 ms (at
/// 44.1 kHz) cannot get it from this unit without a change inside the
/// vendored synthesizer.
///
/// Two ordering rules, and both are load-bearing:
///
/// 1. **Apply before rendering the frames the event governs, never after.**
///    `Synthesizer::render` advances state; a note applied after the frames
///    it should sound in is one segment late. Every segment is rendered after
///    every event whose offset is `<=` its first frame is applied.
/// 2. **Events at the same offset apply in the order they came** (the event
///    input arrives sorted, stably), and the whole equal-offset run lands
///    before a single frame it governs is rendered.
///
/// # The rate: fixed per synthesizer, followed per node
///
/// RustySynth builds its voice tables against a rate at construction and
/// offers no way to re-rate them. [`new`](Self::new) builds at
/// `SynthesizerSettings::sample_rate`; the node's `prepare` swaps in
/// [`with_sample_rate`](Self::with_sample_rate) (on the control thread,
/// keeping the preset) when its graph runs at another rate, so a graph never
/// plays it at the wrong pitch. A rate RustySynth refuses (outside
/// 16–192 kHz) leaves it at its own.
pub struct SoundFontUnit {
    synthesizer: Synthesizer,
    sample_rate: SampleRate,
    /// De-interleaved scratch for one block, sized in the node's `prepare`
    /// to the graph's largest block and never resized on the audio thread.
    /// A block renders into `[..size]` in segments split at each event's
    /// offset.
    left_buffer: Vec<f32>,
    right_buffer: Vec<f32>,
    /// The block's MIDI, sorted by offset: its event input's.
    midi_buffer: Vec<MidiEvent>,
}

impl SoundFontUnit {
    /// Builds a unit over a decoded `SoundFont`.
    ///
    /// The sample rate is fixed here from `settings.sample_rate`: RustySynth
    /// cannot re-rate a synthesizer, so a graph prepared at another rate has
    /// the node build a new one ([`with_sample_rate`](Self::with_sample_rate)).
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
            left_buffer: Vec::new(),
            right_buffer: Vec::new(),
            midi_buffer: vec![MidiEvent::noop(); MIDI_BUFFER_CAPACITY],
        })
    }

    /// The rate this unit renders at: its settings' at construction, or the
    /// rate its graph prepared it at.
    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// A copy of this unit that renders at `sample_rate`: the same SoundFont
    /// (shared, not reloaded), settings, preset and channel state, and no
    /// voice sounding. Allocates a new synthesizer's voices and effect lines:
    /// control thread.
    ///
    /// The way to move a unit to another rate (see "The rate" on
    /// [`SoundFontUnit`]): the node's `prepare` uses it when its graph (a live
    /// graph, an export's fork) runs at another rate.
    ///
    /// # Errors
    ///
    /// [`Error::SoundFont`] if RustySynth refuses the rate (outside
    /// 16–192 kHz).
    pub fn with_sample_rate(&self, sample_rate: SampleRate) -> Result<Self> {
        // RustySynth takes whole hertz; every rate a device or a render asks
        // for is one.
        let hz = sample_rate.get().round() as i32;
        let synthesizer = self
            .synthesizer
            .with_sample_rate(hz)
            .map_err(|e| Error::SoundFont(e.to_string()))?;
        Ok(Self {
            synthesizer,
            sample_rate: SampleRate::from(hz.max(0) as u32),
            left_buffer: vec![0.0; self.left_buffer.len()],
            right_buffer: vec![0.0; self.right_buffer.len()],
            midi_buffer: vec![MidiEvent::noop(); MIDI_BUFFER_CAPACITY],
        })
    }

    /// Starts a note directly, bypassing MIDI.
    ///
    /// **Not the intended path.** Notes should reach this unit on its event
    /// input; this pair has no offset, so a note lands at the start of
    /// whatever block follows rather than where it was placed, and `&mut self`
    /// puts it out of reach once the unit is in a graph. The peer crate
    /// `tutti-polysynth` exposes no such pair.
    ///
    /// **Public only because tests still drive notes through it** (this
    /// crate's integration tests and `bevy_tutti::soundfont`'s); port those
    /// to the event input and the pair can go crate-private in the same
    /// change.
    ///
    /// These are RustySynth's MIDI 1.0 integers, not the engine's MIDI 2.0
    /// vocabulary: `channel` is 0..16, `key` and `velocity` are 7-bit (0..128).
    /// A `velocity` of 0 reads as a note-off to RustySynth.
    pub fn note_on(&mut self, channel: i32, key: i32, velocity: i32) {
        self.synthesizer.note_on(channel, key, velocity);
    }

    /// Releases a note directly, bypassing MIDI.
    ///
    /// **Not the intended path** — see [`note_on`](Self::note_on) for why.
    ///
    /// `channel` is 0..16 and `key` is 7-bit (0..128), per MIDI 1.0.
    pub fn note_off(&mut self, channel: i32, key: i32) {
        self.synthesizer.note_off(channel, key);
    }

    /// Selects the preset a channel plays, bypassing MIDI.
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

    /// Render `size` frames into the scratch, applying the first `count`
    /// (sorted) events of `midi_buffer` at their offsets: the loop
    /// [`SoundFontUnit`]'s "How an event's offset is honoured" documents. An
    /// offset at or past `size` (which a graph's event input never carries)
    /// is clamped to the last frame rather than dropped.
    fn render_events(&mut self, size: usize, count: usize) {
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

impl SoundFontUnit {
    /// Release every key on every channel.
    ///
    /// Note-off rather than `Synthesizer::reset`: voices are released into their
    /// envelopes rather than cut, which is what
    /// `test_reset_silences_all_notes` measures (silence *after* the decay, not
    /// immediately). There is no buffer state to discard alongside it — the
    /// scratch buffers are fully rewritten by every block, and rustysynth
    /// owns the only surviving chunk cursor.
    pub(crate) fn release_all(&mut self) {
        (0..16).for_each(|channel| {
            (0..128).for_each(|key| {
                self.synthesizer.note_off(channel, key);
            });
        });
    }
}

impl Clone for SoundFontUnit {
    fn clone(&self) -> Self {
        Self {
            synthesizer: self.synthesizer.clone(),
            sample_rate: self.sample_rate,
            // Scratch, not state: sized fresh rather than copied, since every
            // block overwrites the frames it reads back.
            left_buffer: vec![0.0; self.left_buffer.len()],
            right_buffer: vec![0.0; self.right_buffer.len()],
            midi_buffer: vec![MidiEvent::noop(); MIDI_BUFFER_CAPACITY],
        }
    }
}
