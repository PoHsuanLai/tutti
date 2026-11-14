//! The synth: a graph node that owns an allocator and a fixed set of voices,
//! and renders their sum.
//!
//! It decodes MIDI and drives allocation (`crate::voice`), per-note control
//! (`crate::synth_voice`), glide, unison and tuning. Per-note messages are
//! routed here because only this module holds both the note-id map and the
//! voices.
//!
//! Rendering works in control steps of at most `CONTROL_BLOCK` frames: each
//! step the voices and any glide write lane targets into the voice bank, which
//! renders the step and ramps every lane to its targets. The mix lives in the
//! bank, so no scratch is sized to a block.

use crate::SynthConfig;
use crate::UnisonVoices;
use crate::bank::{CONTROL_BLOCK, VoiceBank};
use crate::mpe::{Expression, MpeControl};
use crate::synth_voice::SynthVoice;
use crate::tuning::{A4_FREQ, key_freq};
use crate::voice::Gate;
use crate::{UnisonEngine, VoiceAllocator, VoiceAllocatorConfig};
use tutti_graph::{EventKind, SubBlocks};
use tutti_midi_protocol::{PitchBendSensitivity, UniversalReader};
use tutti_midi_protocol::{channel_voice, narrow_value};
use tutti_midi_protocol::{value_to_unit_f32, velocity_to_unit_f32};
use tutti_types::{Amplitude, Hz, Param, SampleRate, Semitones, Tail};
use tutti_ump::ControlValue;
use tutti_ump::NoteId;
use tutti_ump::{
    Attribute, CCNumber, MidiChannel, NoteNumber, RegisteredPerNoteController, Ump, Voice2,
};
use tutti_voicing::{Controller, Effect, Glide, MasterGain, NoteControl, Stop};

/// A polyphonic subtractive synthesizer: a MIDI-driven voice allocator over a
/// SIMD voice bank.
///
/// Construct one from a [`SynthConfig`] with [`PolySynth::new`]. It is a
/// graph node (`tutti_graph::Node`) with no audio input, stereo output and
/// one MIDI event input; each event is applied on its own frame. Inserting it
/// hands back a `tutti_graph::ParamSet` over its live params, by
/// `UnitParam`: `Volume` (master gain, linear), and with a unison engine
/// `Detune` (cents) and `StereoSpread` (0..1).
///
/// The event input accepts MIDI 1.0 and MIDI 2.0 (UMP) channel-voice
/// messages, including MIDI 2.0 per-note controllers. MPE zones are not its
/// business: classic MPE arrives already rewritten as per-note messages
/// (`tutti-midi-runtime`'s `MpeIngest`). Up to 512 events per block are
/// applied; any beyond that are dropped.
///
/// Rendering allocates nothing and takes no lock. Construction, forking and
/// changing the unison voice count allocate and belong on the control thread.
/// The oscillator, filter and envelope are fixed at construction: to change
/// them, build a new synth and replace the node.
///
/// # Examples
///
/// ```
/// use std::num::NonZeroUsize;
/// use tutti_types::MaxBlock;
/// use tutti_types::{Hz, Resonance, SampleRate, Samples};
/// use tutti_graph::{Prepare, Solo};
/// use tutti_polysynth::{FilterType, OscillatorType, PolySynth, SynthConfig};
///
/// let synth = PolySynth::new(SynthConfig {
///     oscillator: OscillatorType::Saw,
///     polyphony: NonZeroUsize::new(8).unwrap(),
///     filter: FilterType::Moog { cutoff: Hz(2_000.0), resonance: Resonance(0.5) },
///     ..Default::default()
/// });
/// assert_eq!(synth.active_voice_count(), 0);
///
/// // Alone in a graph; a host wires a clip or keyboard to its event input.
/// let mut solo = Solo::new(synth, Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples(64))));
/// let out = solo.render(64);
/// assert_eq!(out.len(), 2); // stereo
/// ```
#[derive(Debug)]
pub struct PolySynth {
    config: SynthConfig,
    /// The rate the voices are built for: [`PolySynth::INITIAL_RATE`] until the
    /// graph prepares the synth (`Node::prepare`).
    sample_rate: SampleRate,
    allocator: VoiceAllocator,
    voices: Vec<SynthVoice>,
    /// Every sub-voice's oscillator, filter and envelope state, in SIMD lanes.
    bank: VoiceBank,
    /// The key frequency last struck: where a glide the config asks for leaves
    /// from (the glides themselves are the voices').
    last_freq: Hz,
    unison: Option<UnisonEngine>,
    /// Where each channel's pitch stands, as the note logic last said: what
    /// its wheel times its sensitivity comes to.
    bends: [tutti_types::Semitones; 16],
    /// The per-note sensitivity a reset returns to: the construction value.
    master_volume: Param<Amplitude>,
    /// The last finite master volume the node read: a cell a host wrote a NaN
    /// or an infinity into reads as this, not as itself.
    last_volume: f32,
    /// The Universal System Exclusive the synth's Group carries to it: the
    /// edge that reassembles it and keeps what is addressed to this synth.
    universal: UniversalReader,
    /// The master gain (`Voicing::master_gain`) the last control step ended
    /// at: each step ramps from it to the gain now, so a Master Volume or
    /// Balance never steps the output.
    master_applied: MasterGain,
    /// Voices that finished this span. Sized to the polyphony and only cleared,
    /// so it never allocates on the audio thread.
    finished_indices: Vec<usize>,
}

impl PolySynth {
    /// The rate a synth renders at until its graph prepares it: 44.1 kHz. The
    /// rate is the graph's, not the config's (`Node::prepare`).
    pub const INITIAL_RATE: SampleRate = SampleRate::SR_44K1;

    /// Creates a synth from a [`SynthConfig`], usually written as `Default`
    /// plus struct update:
    /// `SynthConfig { oscillator: OscillatorType::Saw, ..Default::default() }`.
    ///
    /// Every voice, and every unison sub-voice within it, is built here, so
    /// this allocates in proportion to `polyphony * unison.voice_count` and
    /// belongs off the audio thread. There is no upper bound on the polyphony:
    /// every voice is built up front, so it is a memory and CPU budget, not a
    /// real-time risk.
    #[must_use]
    pub fn new(config: SynthConfig) -> Self {
        let polyphony = config.polyphony.get();
        let allocator_config = VoiceAllocatorConfig {
            max_voices: config.polyphony,
            mode: config.voice_mode,
            strategy: config.allocation_strategy,
            bend_range: config.pitch_bend_range,
            per_note_bend_range: PitchBendSensitivity::MPE_MEMBER_DEFAULT.semitones(),
        };
        let mut allocator = VoiceAllocator::new(allocator_config);
        allocator.store_initial_tuning(config.tuning.table());

        let unison = config.unison.as_ref().map(|u| UnisonEngine::new(u.clone()));
        // The engine's count: voices and bank must match its params.
        let unison_count = unison
            .as_ref()
            .map_or(UnisonVoices::ONE, UnisonEngine::voice_count);

        let sample_rate = Self::INITIAL_RATE;
        let voices = (0..polyphony)
            .map(|_| SynthVoice::from_config(&config, sample_rate, unison_count))
            .collect();
        let bank = Self::build_bank(&config, sample_rate, unison_count);

        let master_volume = Param::new(Amplitude::UNITY);
        let universal = UniversalReader::new(config.group, config.device);
        let bends = [Semitones(0.0); 16];

        Self {
            config,
            sample_rate,
            allocator,
            voices,
            bank,
            last_freq: A4_FREQ,
            unison,
            bends,
            master_volume,
            last_volume: Amplitude::UNITY.get(),
            universal,
            master_applied: MasterGain::UNITY,
            finished_indices: Vec::with_capacity(polyphony),
        }
    }

    fn build_bank(
        config: &SynthConfig,
        sample_rate: SampleRate,
        unison_count: UnisonVoices,
    ) -> VoiceBank {
        VoiceBank::new(
            config.oscillator,
            &config.filter,
            &config.envelope,
            sample_rate,
            config.polyphony.get(),
            unison_count,
        )
    }

    /// Applies one MIDI event now, as if it arrived at the start of the next
    /// rendered block.
    ///
    /// Takes `&mut self`, so it cannot reach a synth that is already in a
    /// graph; there, send MIDI on the event input instead. Useful for setting
    /// a synth up before insertion or driving it by hand.
    pub fn apply_midi(&mut self, ump: Ump) {
        self.process_midi_event(ump);
    }

    /// A master volume as [`set_volume`](Self::set_volume) stores it:
    /// floored at 0. Shared by the setter and a driven port's value.
    #[inline]
    pub(crate) fn clamp_volume(volume: f32) -> f32 {
        volume.max(0.0)
    }

    /// Sets the master output gain as a linear [`Amplitude`]: `1.0` (the
    /// default) is unity, `0.0` silence, and values above unity boost.
    /// Negative values clamp to 0.
    ///
    /// Applied once per block to the summed mix, not per voice. Takes `&mut
    /// self`, so it sets the gain a synth is inserted with; once in a graph the
    /// gain is the synth's `ParamSet` entry, `Volume`, which a host writes and a
    /// modulation lane takes as its knob.
    pub fn set_volume(&mut self, volume: Amplitude) {
        self.master_volume
            .store(Amplitude(Self::clamp_volume(volume.get())));
    }

    /// Returns the current master gain as a linear amplitude.
    pub fn volume(&self) -> Amplitude {
        self.master_volume.load()
    }

    /// Returns how many voices are currently sounding, counting those in their release
    /// stage. A voice stays counted until its envelope reaches silence, which is
    /// why this can exceed the number of keys held.
    pub fn active_voice_count(&self) -> usize {
        self.voices.iter().filter(|v| v.is_active()).count()
    }

    /// Returns the number of sub-voices stacked per note, or one when this
    /// synth has no unison engine, so the total oscillator count is always
    /// `polyphony * this`.
    pub fn unison_voice_count(&self) -> UnisonVoices {
        self.unison
            .as_ref()
            .map_or(UnisonVoices::ONE, |u| u.config().voice_count)
    }

    /// Reads one packet and applies it. The only place the synth reads the
    /// wire: everything below takes typed values.
    ///
    /// A MIDI 1.0 channel voice message is read as the MIDI 2.0 one it means
    /// ([`channel_voice`]), so a note sounds the same on either wire. Only the
    /// synth's own Group is read ([`SynthConfig::group`]): a message in another
    /// Group addresses another channel space (M2-104 Appendix B.1), so it is
    /// not this synth's, whichever channel it names.
    ///
    /// The Group's System Exclusive is read here too ([`UniversalReader`]): a
    /// Universal message addressed to this synth or to all of them is the note
    /// logic's (`Voicing::universal`). This is a sink, so a message that is
    /// neither (a clock, a manufacturer's sysex, one `decode` cannot frame) is
    /// unused, not an error.
    fn process_midi_event(&mut self, ump: Ump) {
        if let Some(heard) = self.universal.push(ump) {
            self.allocator.universal(heard);
            self.carry_out_gates();
        }
        if let Some((address, voice)) = channel_voice(ump)
            && address.group == self.config.group
        {
            self.voice2(address.channel, voice);
        }
    }

    /// A MIDI 2.0 channel voice message.
    fn voice2(&mut self, channel: MidiChannel, voice: Voice2) {
        match voice {
            // A MIDI 2.0 Note On at velocity 0 is a Note On (M2-104 §7.4.2),
            // sounding at velocity 0. A MIDI 1.0 one arrives as a `NoteOff`
            // (`channel_voice` widens it, App. D.3.1).
            Voice2::NoteOn {
                note,
                velocity,
                attribute,
            } => {
                self.handle_note_on(note, velocity_to_unit_f32(velocity), channel, attribute);
            }
            // Release velocity is not used: the release is the envelope's.
            Voice2::NoteOff { note, .. } => self.handle_note_off(note, channel),
            Voice2::ControlChange { controller, value } => {
                // The pedals and the channel-mode controllers mean the same to
                // every instrument; the rest are this synth's own.
                match Controller::effect(controller, narrow_value(value)) {
                    Some(effect) => self.handle_effect(effect, channel),
                    // The channel's controllers (modulation, volume, pan,
                    // expression, the portamento and legato switches) are the
                    // note logic's, which keeps them for each channel; the sound
                    // controllers are this synth's own.
                    None => {
                        self.bend_message(channel, &voice);
                        self.handle_cc(controller, value_to_unit_f32(value));
                    }
                }
            }
            // Channel Pressure is the channel's, kept by the note logic.
            Voice2::ChannelPressure(_) => self.bend_message(channel, &voice),
            // A channel's pitch bend moves the notes sounding on that channel
            // (MIDI 1.0, Pitch Bend Change). The note logic keeps the wheels and
            // says where each channel's pitch stands.
            Voice2::PitchBend(_) => self.bend_message(channel, &voice),
            // The channel's registered parameters (the bend sensitivity, the
            // channel tuning, the Modulation Depth Range) are the note logic's.
            Voice2::Registered { .. } => self.bend_message(channel, &voice),
            // Per-note controllers, Per-Note Management and the per-note bend's
            // sensitivity (RPN 0/7) are the note logic's: it keeps what each
            // channel's Note Number was told (so a value sent before its Note
            // On reaches the note), which voices have been detached, and the
            // sensitivity of each channel (M2-104 section 7.4). The synth
            // carries out the controllers it can apply (`apply_note_control`).
            Voice2::PerNotePitchBend { .. }
            | Voice2::PolyPressure { .. }
            | Voice2::AssignablePerNote { .. }
            | Voice2::RegisteredPerNote { .. }
            | Voice2::PerNoteManagement { .. } => self.bend_message(channel, &voice),
            // Controllers and messages with no target in a synth.
            Voice2::Assignable { .. }
            | Voice2::RelativeRegistered { .. }
            | Voice2::RelativeAssignable { .. }
            | Voice2::ProgramChange { .. } => {}
        }
    }

    /// Runs a bend, a sensitivity or a per-note message through the note logic
    /// and carries out what it says.
    fn bend_message(&mut self, channel: MidiChannel, message: &Voice2) {
        self.allocator.message(channel, message);
        self.carry_out_gates();
    }

    /// The frequency ratio a channel's bend applies.
    fn bend_ratio(&self, channel: MidiChannel) -> f32 {
        self.bends[channel.index()].to_pitch_ratio()
    }

    fn handle_note_on(
        &mut self,
        note: NoteNumber,
        vel_norm: f32,
        channel: MidiChannel,
        attribute: Attribute,
    ) {
        let id = NoteId::from_channel_note(channel, note);
        // A key struck again while it sounds replaces its voice: the note logic
        // releases the old one at once whatever holds it, and the new one takes
        // the holds; the MIDI 1.0 spec is silent. With `NoSteal` and a full
        // pool the new note finds no slot, so the key goes silent.
        self.allocator
            .allocate(id, note, channel, vel_norm, attribute);
        self.carry_out_gates();
    }

    fn handle_note_off(&mut self, note: NoteNumber, channel: MidiChannel) {
        let id = NoteId::from_channel_note(channel, note);
        // `release` resolves the exact voice by id and reports whether it truly
        // stopped (vs. held by a pedal). Gate that voice by index — never by a
        // (note, channel) scan, which would alias two same-pitch voices.
        self.allocator.release(id, channel);
        self.carry_out_gates();
    }

    /// Applies a per-note controller to the voice in `slot`, as far as a voice
    /// has the means to: the note's sounding pitch (an absolute pitch: its told
    /// Pitch 7.25 or Attribute Pitch 7.9 with the per-note bend, summed by
    /// `tutti-voicing`; the channel's bend goes on it), pressure,
    /// Volume and Brightness (as slide). The other Registered Per-Note
    /// Controllers (pan, modulation, the rest of Appendix A) have no per-note
    /// DSP here. Assignable index 7 and 74 mean volume and slide by this
    /// synth's convention, as CC 7 and CC 74 do.
    fn apply_note_control(&mut self, slot: usize, control: NoteControl) {
        let mpe = match control {
            NoteControl::Pitch(pitch) => {
                // An absolute pitch: no tuning of the key is on it.
                let freq = key_freq(pitch.note_number(), Semitones(0.0));
                let ratio = self.bend_ratio(self.voices[slot].channel());
                self.voices[slot].set_tuning_freq(freq);
                // The note's own pitch, with the glide it may be on and the
                // channel's bend on it.
                let glide = self.voices[slot].glide_ratio();
                self.voices[slot].set_pitch(freq * glide * ratio);
                return;
            }
            NoteControl::Pressure(value) => {
                MpeControl::Pressure(Expression::from_control(ControlValue::new(value)))
            }
            NoteControl::Registered(RegisteredPerNoteController::Volume, value) => {
                MpeControl::Gain(Expression::from_control(ControlValue::new(value)))
            }
            NoteControl::Registered(RegisteredPerNoteController::Brightness, value) => {
                MpeControl::Slide(Expression::from_control(ControlValue::new(value)))
            }
            // The index is a wire byte, not a 7-bit value: 0xCA is not CC74.
            NoteControl::Assignable(index, value) if index == CCNumber::BRIGHTNESS.get() => {
                MpeControl::Slide(Expression::from_control(ControlValue::new(value)))
            }
            NoteControl::Assignable(index, value) if index == CCNumber::VOLUME.get() => {
                MpeControl::Gain(Expression::from_control(ControlValue::new(value)))
            }
            NoteControl::Registered(..) | NoteControl::Assignable(..) => return,
        };
        self.voices[slot].apply_mpe(mpe);
    }

    fn handle_effect(&mut self, effect: Effect, channel: MidiChannel) {
        match effect {
            Effect::Sustain(on) => {
                self.allocator.sustain_pedal(channel, on);
                self.carry_out_gates();
            }
            Effect::Sostenuto(on) => {
                self.allocator.sostenuto_pedal(channel, on);
                self.carry_out_gates();
            }
            // Reset All Controllers (RP-015, M2-104 Appendix B.2): the channel's
            // controllers return to their defaults, but not the per-note
            // controllers, which belong to individual notes (but for Polyphonic
            // Pressure). All of it is the note logic's: the pedals, the wheel,
            // modulation, expression, pressure (the notes the pedals held are
            // released), by channel. The sound controllers (CC 70-79:
            // resonance, brightness) are not reset by RP-015, so CC 71 and CC 74
            // stay where they were.
            Effect::ResetControllers => {
                self.allocator.reset_controllers(channel);
                self.carry_out_gates();
            }
            Effect::Stop(Stop::Cut) => {
                self.allocator.all_sound_off(channel);
                self.carry_out_gates();
            }
            // All Notes Off (MIDI 1.0 Channel Mode Messages) lets go of the
            // keys; a pedal that is down still holds its notes, as at a
            // note-off.
            Effect::Stop(Stop::Release) => {
                self.allocator.all_notes_off(channel);
                self.carry_out_gates();
            }
        }
    }

    fn handle_cc(&mut self, cc_num: CCNumber, value: f32) {
        match cc_num {
            CCNumber::RESONANCE => {
                self.voices
                    .iter_mut()
                    .for_each(|v| v.set_filter_resonance(value));
            }
            // CC74 (Brightness). Under MPE a member-channel CC74 is rewritten to a
            // native per-note controller at the input edge (routed per-voice via
            // the `AssignablePerNoteController` arm above), so a channel-wide CC74
            // reaching here is a plain global brightness/cutoff.
            CCNumber::BRIGHTNESS => {
                self.voices.iter_mut().for_each(|v| v.set_cc_cutoff(value));
            }
            _ => {}
        }
    }

    fn apply_pitch_bend(&mut self) {
        // The bend is a frequency ratio at every site (note-on and glide bend
        // interpolated frequencies, which have no note number). Adding it to
        // the note number instead would differ on an unequal tuning table.
        let bends = &self.bends;
        self.voices
            .iter_mut()
            .filter(|v| v.is_active())
            .for_each(|voice| Self::sound(voice, bends));
    }

    /// Sets `voice`'s pitch to what its factors make of it: the note's own (its
    /// key's, or the one a per-note controller told it), the glide it is on and
    /// its channel's bend.
    fn sound(voice: &mut SynthVoice, bends: &[tutti_types::Semitones; 16]) {
        let base_freq = voice
            .tuned()
            .unwrap_or_else(|| key_freq(f32::from(voice.note().get()), voice.tune()));
        let bend = bends[voice.channel().index()].to_pitch_ratio();
        voice.set_pitch(base_freq * voice.glide_ratio() * bend);
    }

    /// Does to the voices what the note logic's commands asked of their slots.
    fn carry_out_gates(&mut self) {
        let mut retuned = false;
        let mut gates = self.allocator.take_gates();
        for gate in gates.drain(..) {
            match gate {
                Gate::Off(i) => self.voices[i].note_off(),
                Gate::Cut(i) => {
                    self.voices[i].reset();
                    self.bank.kill_voice(i);
                }
                Gate::Retune(channel, bend) => {
                    self.bends[channel.index()] = bend;
                    retuned = true;
                }
                Gate::Tune(i, tune) => {
                    self.voices[i].set_tune(tune);
                    retuned = true;
                }
                Gate::Control(i, control) => self.apply_note_control(i, control),
                Gate::ResetControls(i) => {
                    self.voices[i].reset_mpe();
                    self.voices[i].clear_tuning();
                    retuned = true;
                }
                Gate::Start {
                    slot,
                    key,
                    channel,
                    tune,
                    velocity,
                    legato,
                    glide,
                } => self.start_voice(slot, key, channel, tune, velocity, legato, glide),
            }
        }
        self.allocator.return_gates(gates);
        if retuned {
            self.apply_pitch_bend();
        }
    }

    /// Starts the voice in `slot` on `note` (or, `legato`, moves it there).
    /// `glide` is the one the stream asked for (Portamento On/Off and
    /// Portamento Control, resolved by the note logic): the voice's pitch leaves
    /// the glide's key and reaches the note's in the glide's time. With none,
    /// the config's own portamento rule may glide it from the key struck before.
    fn start_voice(
        &mut self,
        slot_index: usize,
        note: NoteNumber,
        channel: MidiChannel,
        tune: Semitones,
        vel_norm: f32,
        is_legato: bool,
        glide: Option<Glide>,
    ) {
        let base_freq = key_freq(f32::from(note.get()), tune);
        let last = std::mem::replace(&mut self.last_freq, base_freq);
        let bend_multiplier = self.bend_ratio(channel);
        let voice = &mut self.voices[slot_index];
        if !is_legato && voice.is_fresh() {
            // A fresh draw for this voice alone, held by the voice: the notes of
            // a chord each get their own stack of phases.
            if let Some(unison) = &mut self.unison {
                voice.set_start_phases(unison.randomize_phases());
            }
        }
        voice.set_velocity_mod(vel_norm);
        voice.set_tune(tune);
        if is_legato {
            // The new key's own pitch: what a per-note controller told the old
            // one is not the new one's.
            voice.clear_tuning();
            voice.update_legato(note, channel);
        } else {
            voice.note_on(note, channel, vel_norm, base_freq);
        }
        match glide {
            Some(g) => {
                let from = key_freq(f32::from(g.from.get()), g.from_tune);
                voice.begin_glide(from, base_freq, g.time);
            }
            None => voice.config_glide(last, base_freq, is_legato),
        }
        voice.set_pitch(base_freq * voice.glide_ratio() * bend_multiplier);
    }

    fn mark_voice_finished(&mut self, slot_index: usize) {
        let slots = self.allocator.slots();
        if slot_index < slots.len() {
            let voice_id = slots[slot_index].voice_id();
            self.allocator.voice_finished(voice_id);
        }
    }

    /// One control step: advance any glide by `frames`, then have every
    /// sounding voice write its lane targets.
    ///
    /// The glide advances frame by frame and the voices are aimed at where it will
    /// be at the *end* of the step; the bank ramps each lane there sample by
    /// sample. So a glide moves the pitch every frame at any block length.
    /// Rendering a whole sub-block at the glide's final pitch would instead
    /// produce a staircase, one step per MIDI sub-block.
    fn control_step(&mut self, frames: usize) {
        // Each gliding voice advances by the step and is aimed where its glide
        // will be at the end of it. Its pitch is its own factors again (the
        // note's, the glide, the bend), so a glide never overwrites what a
        // per-note controller told the note. Neither operand changes inside a
        // step (MIDI is handled between steps).
        let bends = &self.bends;
        for voice in self
            .voices
            .iter_mut()
            .filter(|v| v.is_active() && v.is_gliding())
        {
            voice.advance_glide(frames);
            Self::sound(voice, bends);
        }

        let unison = self.unison.as_ref();
        for (i, voice) in self.voices.iter_mut().enumerate() {
            if voice.is_active() {
                let channel = self.allocator.channel_state(voice.channel());
                voice.drive(&mut self.bank, i, frames, unison, channel);
            }
        }
    }

    /// Render `frames` frames with no MIDI inside them, handing each frame to
    /// `emit` as `(start + offset, left, right)` scaled by `volume`.
    ///
    /// Works in control steps of at most [`CONTROL_BLOCK`] frames, so it
    /// renders every frame of any `frames` with no scratch sized to a block.
    ///
    /// The steps sit on a fixed grid of [`CONTROL_BLOCK`] frames from the block
    /// start, cut additionally at each MIDI event, so a host block that is a
    /// multiple of the grid renders sample-identically however it is sized.
    fn render_span(
        &mut self,
        start: usize,
        frames: usize,
        volume: f32,
        emit: &mut impl FnMut(usize, f32, f32),
    ) {
        let mut done = 0;
        while done < frames {
            let to_grid = CONTROL_BLOCK - (start + done) % CONTROL_BLOCK;
            let n = (frames - done).min(to_grid);
            self.control_step(n);
            let (left, right) = self.bank.render(n);
            // The master gain ramps across the step, from where the last one
            // ended to where the Universal messages have put it: at unity
            // throughout it is the identity.
            let (from, to) = (self.master_applied, self.allocator.master_gain());
            let ramp = |from: Amplitude, to: Amplitude, i: usize| {
                from.get() + (to.get() - from.get()) * ((i + 1) as f32 / n as f32)
            };
            for (i, (&l, &r)) in left.iter().zip(right).enumerate() {
                emit(
                    start + done + i,
                    l * volume * ramp(from.left, to.left, i),
                    r * volume * ramp(from.right, to.right, i),
                );
            }
            self.master_applied = to;
            done += n;
        }
        self.retire_finished_voices();
    }

    /// Report each sounding voice's level to the allocator and free the ones
    /// whose release has run out.
    fn retire_finished_voices(&mut self) {
        self.finished_indices.clear();
        for (i, voice) in self.voices.iter_mut().enumerate() {
            if !voice.is_active() {
                continue;
            }
            let channel = *self.allocator.channel_state(voice.channel());
            let level = voice.output_level(&self.bank, i, &channel);
            voice.set_envelope_level(level);
            self.allocator.update_envelope_level(i, level);
            if voice.is_finished(&self.bank, i) {
                voice.deactivate();
                self.finished_indices.push(i);
            }
        }

        // Index, not `mem::take`: taking would free the `Vec` on the audio thread.
        for i in 0..self.finished_indices.len() {
            let slot_index = self.finished_indices[i];
            self.mark_voice_finished(slot_index);
        }
    }

    /// Renders a block split at its events (`Io::sub_blocks`): each chunk's
    /// MIDI is applied, then its frames go to
    /// [`render_span`](Self::render_span), so every event lands on its frame.
    /// Nothing here is bounded by a block size or an event count, so any
    /// block renders every frame and reads every event.
    fn render_events(
        &mut self,
        blocks: SubBlocks<'_>,
        volume: f32,
        emit: &mut impl FnMut(usize, f32, f32),
    ) {
        for (range, events) in blocks {
            for event in events {
                if let EventKind::Midi(ump) = event.kind {
                    self.process_midi_event(ump);
                }
            }
            self.render_span(range.start, range.len(), volume, emit);
        }
    }
}

impl PolySynth {
    /// Silence every voice and forget the glide: a freshly prepared synth's
    /// state. Its `Param` cells and MPE mode are untouched.
    pub(crate) fn reset_voices(&mut self) {
        for voice in &mut self.voices {
            voice.reset();
        }
        self.bank.reset();
        self.allocator.reset();
        self.universal.reset();
        self.master_applied = MasterGain::UNITY;
        self.bends = [tutti_types::Semitones(0.0); 16];
        self.last_freq = A4_FREQ;
    }

    /// Detach the control cells (master volume, unison detune and spread)
    /// at their values now: this synth reads and writes cells of its own
    /// from here on. A fork's half of `fork_instance`.
    pub(crate) fn detach_controls(&mut self) {
        self.master_volume.detach();
        if let Some(unison) = &mut self.unison {
            unison.detach();
        }
    }

    /// Re-rate the voice bank and the glide.
    pub(crate) fn set_rate(&mut self, sample_rate: SampleRate) {
        self.sample_rate = sample_rate;
        self.bank.set_sample_rate(sample_rate);
        for voice in &mut self.voices {
            voice.set_rate(sample_rate);
        }
    }
}

impl Clone for PolySynth {
    fn clone(&self) -> Self {
        // The clone's MIDI scratch is its own (a clone is a fork's template,
        // and later a fork: it plays only its own event input).
        Self {
            config: self.config.clone(),
            allocator: self.allocator.clone(),
            voices: self.voices.clone(),
            bank: self.bank.clone(),
            last_freq: self.last_freq,
            unison: self.unison.clone(),
            bends: self.bends,
            master_volume: self.master_volume.clone(),
            last_volume: self.last_volume,
            universal: self.universal.clone(),
            master_applied: self.master_applied,
            sample_rate: self.sample_rate,
            finished_indices: Vec::with_capacity(self.config.polyphony.get()),
        }
    }
}

#[cfg(test)]
mod tests;

impl PolySynth {
    /// The amplitude envelope's release stage, as the node's tail.
    ///
    /// This node takes no audio input — it is driven by MIDI — so "after the
    /// input stops" means after the last note-off, at which point every
    /// still-sounding voice rings for its release time. That time is an
    /// authored parameter rather than an estimate, which is what makes this
    /// exact.
    ///
    /// A bounce that ends at the last note-off without it chops the release
    /// off every note in the project.
    pub(crate) fn release_tail(&self) -> Tail {
        match self
            .config
            .envelope
            .release
            .to_samples_ceil(self.sample_rate)
        {
            s if s.is_zero() => Tail::None,
            s => Tail::Finite(s),
        }
    }
}

mod node;
