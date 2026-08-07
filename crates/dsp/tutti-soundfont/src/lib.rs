//! SoundFont (.sf2) synthesis via RustySynth.
//!
//! Build a [`SoundFontUnit`] with [`SoundFontUnit::new`] from a decoded
//! `SoundFont` and a [`SynthesizerSettings`], then `program_change` to pick the
//! preset/channel. A host that wants asset-managed loading wires it in its own
//! adapter layer; this crate only needs the decoded `SoundFont`.
//!
//! Split out of the old `tutti-synth` (renamed [`tutti-polysynth`]) because a
//! `.sf2` player and a subtractive voice engine share no code: this unit
//! reaches for none of that crate's voice allocation, tuning, portamento or
//! unison. What they share is the *shape* — both are `AudioUnit`s with a
//! [`MidiInPort`] — and that comes from `tutti-core` and `tutti-midi-runtime`,
//! not from each other.
//!
//! [`tutti-polysynth`]: https://docs.rs/tutti-polysynth

pub mod error;
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

pub struct SoundFontUnit {
    synthesizer: Synthesizer,
    sample_rate: SampleRate,
    buffer_size: usize,
    left_buffer: Vec<f32>,
    right_buffer: Vec<f32>,
    buffer_pos: usize,
    /// This unit's MIDI input endpoint (routing address + mailbox + current pull
    /// source). See [`MidiInPort`] for the fundsp clone/isolate sharing semantics.
    midi: MidiInPort,
    midi_buffer: Vec<MidiEvent>,
}

impl SoundFontUnit {
    pub fn new(
        soundfont: Arc<SoundFont>,
        settings: &SynthesizerSettings,
    ) -> Result<Self> {
        let synthesizer = Synthesizer::new(&soundfont, settings)
            .map_err(|e| Error::SoundFont(e.to_string()))?;

        let buffer_size = 64;

        Ok(Self {
            synthesizer,
            // `SynthesizerSettings::sample_rate` is rustysynth's `i32`. We are
            // the library, so the conversion into the engine's vocabulary
            // happens here rather than being pushed onto callers.
            sample_rate: SampleRate::from(settings.sample_rate.max(0) as u32),
            buffer_size,
            left_buffer: vec![0.0; buffer_size],
            right_buffer: vec![0.0; buffer_size],
            buffer_pos: buffer_size,
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

    pub fn clear_midi_source(&mut self) {
        self.midi.clear();
    }

    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    pub fn note_on(&mut self, channel: i32, key: i32, velocity: i32) {
        self.synthesizer.note_on(channel, key, velocity);
    }

    pub fn note_off(&mut self, channel: i32, key: i32) {
        self.synthesizer.note_off(channel, key);
    }

    pub fn program_change(&mut self, channel: i32, preset: i32) {
        self.synthesizer
            .process_midi_message(channel, 0xC0, preset, 0);
    }

    fn refill_buffers(&mut self) {
        self.left_buffer[..self.buffer_size].fill(0.0);
        self.right_buffer[..self.buffer_size].fill(0.0);
        self.synthesizer
            .render(&mut self.left_buffer, &mut self.right_buffer);
        self.buffer_pos = 0;
    }

    /// Poll this block's events into `midi_buffer`, sorted by `frame_offset`,
    /// and return the count. Does **not** dispatch — the caller applies each
    /// event at its offset (see [`Self::process`]) so timing stays
    /// sample-accurate rather than collapsing every event to the block start.
    fn poll_midi_events_sorted(&mut self, block_size: usize) -> usize {
        let count = self.midi.poll(block_size, &mut self.midi_buffer);
        if count > 1 {
            self.midi_buffer[..count].sort_unstable_by_key(|e| e.frame_offset);
        }
        count
    }

    /// Apply one polled event to the synthesizer. RustySynth speaks MIDI 1.0
    /// wire format. `normalize` gives us a single MIDI-2 vocabulary; `dispatch`
    /// downscales to 7-bit via the spec Min-Center-Max converters (a documented
    /// 1.0 boundary: per-note messages have no rustysynth analogue, dropped).
    fn apply_event(&mut self, event: &MidiEvent) {
        self.dispatch(&tutti_midi_types::normalize(event));
    }

    /// Pull one output sample from the rustysynth render buffer, refilling the
    /// 64-sample chunk on demand.
    #[inline]
    fn next_output_sample(&mut self) -> (f32, f32) {
        if self.buffer_pos >= self.buffer_size {
            self.refill_buffers();
        }
        let s = (
            self.left_buffer[self.buffer_pos],
            self.right_buffer[self.buffer_pos],
        );
        self.buffer_pos += 1;
        s
    }

    /// MIDI 1.0 boundary. Values downscale via spec Min-Center-Max (convert.rs).
    /// Translated: NoteOn/Off, CC, channel pitch bend, program change.
    /// (Channel pressure / key pressure arrive as CC/poly-pressure UMP but
    /// rustysynth exposes no dedicated setter, so they fall through the `_`
    /// arm — see Dropped.)
    /// Dropped (no rustysynth MIDI-1 analogue): per-note pitch bend, per-note
    /// controllers, per-note management (Detach/Reset), channel/poly pressure,
    /// RPN/NRPN, and any 16-bit velocity / 32-bit CC precision beyond 7 bits.
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
    fn reset(&mut self) {
        (0..16).for_each(|channel| {
            (0..128).for_each(|key| {
                self.synthesizer.note_off(channel, key);
            });
        });
        self.buffer_pos = self.buffer_size;
    }

    /// Sever the live MIDI input this clone shares with the original synth.
    ///
    /// Same rationale as [`crate::PolySynth::isolate`]: an offline render ticks
    /// this clone on a worker thread while the live synth plays, so a shared inbox
    /// would let the worker *steal* the live synth's events and a shared source
    /// cell would let clearing here sever the live clip. [`MidiInPort::isolate`]
    /// mints a fresh private mailbox + source cell so this clone reads nothing.
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

        let (l, r) = self.next_output_sample();
        output[0] = l;
        output[1] = r;
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        // Poll the whole block's events (sorted by offset) and interleave their
        // application with rendering: apply every event due at `pos`, emit one
        // sample, advance. Events are sample-accurate — the MIDI subsystem
        // delivers them to our inbox once per block, each carrying its
        // `frame_offset`.
        let count = self.poll_midi_events_sorted(size);
        let mut event_idx = 0;

        for pos in 0..size {
            // Apply every event whose offset is at or before this sample. Events
            // past `size` are clamped in so a stray late offset still fires.
            while event_idx < count
                && (self.midi_buffer[event_idx].frame_offset as usize).min(size.saturating_sub(1))
                    <= pos
            {
                let event = self.midi_buffer[event_idx];
                self.apply_event(&event);
                event_idx += 1;
            }

            let (l, r) = self.next_output_sample();
            output.set_f32(0, pos, l);
            output.set_f32(1, pos, r);
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
            buffer_size: self.buffer_size,
            left_buffer: self.left_buffer.clone(),
            right_buffer: self.right_buffer.clone(),
            buffer_pos: self.buffer_pos,
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
