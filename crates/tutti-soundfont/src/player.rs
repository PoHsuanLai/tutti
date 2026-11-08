//! The SoundFont player: the channels' controller state, the voices that
//! sound, the core that decides when they start and end, and the mix.
//!
//! A message goes to [`Voicing::step`], which says which voices to start,
//! release or cut; [`Voices`] does it. What a note becomes is the preset's
//! ([`PresetPatch`]); how a voice sounds is the voice's own.

use std::collections::HashMap;
use std::sync::Arc;

use tutti_midi_protocol::{narrow_bend, narrow_value};
use tutti_sf2::SoundFont;
use tutti_types::{SampleRate, Semitones};
use tutti_ump::{Attribute, CCNumber, MidiChannel, U7, Voice2};
use tutti_voicing::{Mode, Order, Retrigger, Rules, Steal, Voicing};

use crate::array_math::ArrayMath;
use crate::channel::Channel;
use crate::chorus::Chorus;
use crate::error::{Error, Result};
use crate::patch::PresetPatch;
use crate::render::Render;
use crate::reverb::Reverb;
use crate::settings::Settings;
use crate::soundfont_math::SoundFontMath;
use crate::voices::Voices;

/// The release velocity of a Note Off that a Note On at velocity 0 stands for
/// (M2-104 Appendix D.3.1), which a SoundFont does not read.
const NEUTRAL_RELEASE: u16 = 0x8000;

/// The gain applied to the mix of every voice.
const MASTER_VOLUME: f32 = 0.5;

/// The rates a player renders at, in whole hertz.
const RATES: std::ops::RangeInclusive<i32> = 16_000..=192_000;

/// The note rules of a SoundFont, as a value of [`Rules`]. Where the SoundFont
/// 2.04 specification says, the rule is the spec's; where it is silent, it is
/// tutti's, and says so.
///
/// - `Poly`: the specification describes polyphonic playback and nothing else.
/// - `Replace` (tutti): the spec does not say what a key struck again does.
///   The earlier strike is released, whatever pedal holds it, so a repeated
///   key never stacks voices.
/// - `By(Oldest)` (tutti): the spec does not say who gives way when the
///   voices are all taken. A released voice goes first, then the one that
///   started longest ago.
/// - `exclusive_groups` (spec, section 8.1.2, generator 57): a note with an
///   exclusive class ends the other sounding notes of that class in its
///   preset, rapidly; the scope is the preset, which the patch names.
/// - `sostenuto` (tutti, with MIDI 1.0): CC 66. The spec lists CC 66 as "soft"
///   and CC 67 as "sostenuto" in section 9.2, the reverse of MIDI 1.0, whose
///   numbers a keyboard sends; tutti follows MIDI.
fn rules(polyphony: usize) -> Rules {
    Rules {
        mode: Mode::Poly,
        retrigger: Retrigger::Replace,
        polyphony,
        steal: Steal::By(Order::Oldest),
        exclusive_groups: true,
        sostenuto: true,
        // The channel's own bend range (RPN 0 as a MIDI 1.0 Control Change
        // run) is what the voices read; the core's figure is not used.
        bend_range: Semitones(2.0),
    }
}

/// The player. Its voices are built against a rate and cannot be re-rated in
/// place: [`with_sample_rate`](Self::with_sample_rate) builds another.
#[derive(Debug, Clone)]
pub(crate) struct Player {
    font: Arc<SoundFont>,
    render: Render,
    settings: Settings,

    presets: HashMap<i32, usize>,
    default_preset: usize,

    channels: Vec<Channel>,
    voicing: Voicing,
    voices: Voices,

    block_left: Vec<f32>,
    block_right: Vec<f32>,
    block_read: usize,

    effects: Option<Effects>,
}

impl Player {
    /// The channel General MIDI plays percussion on.
    pub(crate) const PERCUSSION_CHANNEL: MidiChannel = MidiChannel::of(9);

    /// Builds a player over `font`. Allocates the voices and the effect lines.
    ///
    /// # Errors
    ///
    /// [`Error::SampleRateOutOfRange`] outside 16 to 192 kHz.
    pub(crate) fn new(font: &Arc<SoundFont>, settings: &Settings) -> Result<Self> {
        let render = Render::new(settings.sample_rate());
        if !RATES.contains(&render.hz()) {
            return Err(Error::SampleRateOutOfRange(settings.sample_rate()));
        }
        let polyphony = settings.polyphony().get();

        let mut presets: HashMap<i32, usize> = HashMap::new();
        let mut min_preset_id = i32::MAX;
        let mut default_preset: usize = 0;
        for (i, preset) in font.presets().iter().enumerate() {
            // A preset whose bank or program is not MIDI's cannot be played
            // (SF2 2.04 section 7.2): it is kept in the font and not looked up.
            let (Some(bank), Some(patch)) = (preset.bank(), preset.patch()) else {
                continue;
            };
            // The preset ID is Int32, where the upper 16 bits represent the bank number
            // and the lower 16 bits represent the patch number.
            // This ID is used to search for presets by the combination of bank number
            // and patch number.
            let preset_id = (i32::from(bank.get()) << 16) | i32::from(patch.get());
            presets.insert(preset_id, i);

            // The preset with the minimum ID number will be default.
            // If the SoundFont is GM compatible, the piano will be chosen.
            if preset_id < min_preset_id {
                default_preset = i;
                min_preset_id = preset_id;
            }
        }

        let channels = (0..MidiChannel::COUNT)
            .map(|i| Channel::new(i == Self::PERCUSSION_CHANNEL.get()))
            .collect();

        Ok(Self {
            font: Arc::clone(font),
            render,
            settings: *settings,
            presets,
            default_preset,
            channels,
            voicing: Voicing::new(rules(polyphony)),
            voices: Voices::new(&render, polyphony),
            block_left: vec![0_f32; render.block_len()],
            block_right: vec![0_f32; render.block_len()],
            block_read: render.block_len(),
            effects: settings.reverb_and_chorus().then(|| Effects::new(&render)),
        })
    }

    /// The preset `channel` plays: its bank and program, else General MIDI's
    /// program in bank 0 (the standard drum set, 128:0, for a drum bank), else
    /// the font's first.
    fn preset_of(&self, channel: MidiChannel) -> usize {
        let info = &self.channels[usize::from(channel.get())];
        let id = (info.bank_number() << 16) | info.patch_number();
        if let Some(&preset) = self.presets.get(&id) {
            return preset;
        }
        let gm_id = if info.bank_number() < 128 {
            info.patch_number()
        } else {
            128 << 16
        };
        self.presets
            .get(&gm_id)
            .copied()
            .unwrap_or(self.default_preset)
    }

    /// Reads one channel voice message.
    ///
    /// Followed: Note On and Off, program change, pitch bend, and the
    /// controllers named at [`control`](Self::control). Everything else is
    /// dropped.
    pub(crate) fn apply(&mut self, channel: MidiChannel, message: Voice2) {
        match message {
            // A MIDI 2.0 Note On at velocity 0 is a Note Off here, as it
            // always has been (M2-104 7.4.2 keeps it a Note On; that is for
            // the translation stage).
            Voice2::NoteOn {
                note, velocity: 0, ..
            } => self.dispatch(
                channel,
                Voice2::NoteOff {
                    note,
                    velocity: NEUTRAL_RELEASE,
                    attribute: Attribute::None,
                },
            ),
            Voice2::NoteOn { .. } | Voice2::NoteOff { .. } => self.dispatch(channel, message),
            Voice2::ControlChange { controller, value } => {
                self.control(channel, controller, narrow_value(value));
                self.dispatch(channel, message);
            }
            Voice2::PitchBend(bend) => {
                let bend = narrow_bend(bend);
                self.channels[usize::from(channel.get())]
                    .set_pitch_bend(i32::from(bend.lsb().get()), i32::from(bend.msb().get()));
            }
            // The program only: a bank is Bank Select's (CC 0), which the
            // channel latches.
            Voice2::ProgramChange { program, .. } => self.program_change(channel, program),
            // No setter for pressure; no per-note control (a SoundFont voice
            // is a channel's); and a native RPN or NRPN is not assembled into
            // the Control Change run the channel follows.
            _ => {}
        }
    }

    /// Selects the program `channel` plays within its bank.
    pub(crate) fn program_change(&mut self, channel: MidiChannel, program: U7) {
        self.channels[usize::from(channel.get())].set_patch(i32::from(program.get()));
    }

    /// Hands a note or pedal message to the core, and carries out what it
    /// says.
    fn dispatch(&mut self, channel: MidiChannel, message: Voice2) {
        self.voices.refresh();
        let patch = PresetPatch {
            font: &self.font,
            preset: self.preset_of(channel),
        };
        let commands = self
            .voicing
            .step(channel, &message, &patch, self.voices.views());
        self.voices.execute(commands, &self.font);
    }

    /// What a Control Change sets in the channel.
    ///
    /// Followed: bank select, modulation, data entry, volume, pan,
    /// expression, reverb and chorus send, the RPN and NRPN selectors, and
    /// reset all controllers. The pedals (64, 66), all sound off (120), all
    /// notes off (123) and the mode messages are the core's
    /// (`Voicing::step`), not the channel's. Any other controller is ignored.
    fn control(&mut self, channel: MidiChannel, controller: CCNumber, value: U7) {
        let data = i32::from(value.get());
        let info = &mut self.channels[usize::from(channel.get())];
        match controller.get() {
            0x00 => info.set_bank(data),
            0x01 => info.set_modulation_coarse(data),
            0x21 => info.set_modulation_fine(data),
            0x06 => info.data_entry_coarse(data),
            0x26 => info.data_entry_fine(data),
            0x07 => info.set_volume_coarse(data),
            0x27 => info.set_volume_fine(data),
            0x0A => info.set_pan_coarse(data),
            0x2A => info.set_pan_fine(data),
            0x0B => info.set_expression_coarse(data),
            0x2B => info.set_expression_fine(data),
            0x5B => info.set_reverb_send(data),
            0x5D => info.set_chorus_send(data),
            0x63 => info.set_nrpn_coarse(data),
            0x62 => info.set_nrpn_fine(data),
            0x65 => info.set_rpn_coarse(data),
            0x64 => info.set_rpn_fine(data),
            0x79 => info.reset_all_controllers(),
            _ => (),
        }
    }

    /// Lets every voice go into its release, whatever holds it, and forgets
    /// the pedals and keys: a fork's start.
    pub(crate) fn release_all(&mut self) {
        self.voices.release_all();
        self.voicing.reset();
    }

    /// Cuts every voice, forgets the pedals and keys, returns every
    /// channel's performance controllers to their defaults (modulation,
    /// expression, bend and the RPN selection: Reset All Controllers) and
    /// mutes the effects. What the host selected stays: the bank, the program,
    /// the volume, the pan and the sends.
    pub(crate) fn reset(&mut self) {
        self.voices.clear();
        self.voicing.reset();
        for channel in &mut self.channels {
            channel.reset_all_controllers();
        }
        if let Some(effects) = self.effects.as_mut() {
            effects.reverb.mute();
            effects.chorus.mute();
        }
        self.block_read = self.render.block_len();
    }

    /// Renders stereo audio into the provided buffers, which are the same
    /// length. A shorter of the two is all that is written.
    pub(crate) fn render(&mut self, left: &mut [f32], right: &mut [f32]) {
        debug_assert_eq!(left.len(), right.len());
        let block = self.render.block_len();
        let length = left.len().min(right.len());

        let mut wrote = 0;
        while wrote < length {
            if self.block_read == block {
                self.render_block();
                self.block_read = 0;
            }

            let src_rem = block - self.block_read;
            let dst_rem = length - wrote;
            let rem = src_rem.min(dst_rem);

            left[wrote..wrote + rem]
                .copy_from_slice(&self.block_left[self.block_read..self.block_read + rem]);
            right[wrote..wrote + rem]
                .copy_from_slice(&self.block_right[self.block_read..self.block_read + rem]);

            self.block_read += rem;
            wrote += rem;
        }
    }

    fn render_block(&mut self) {
        let inverse_block_size = 1_f32 / self.render.block_len() as f32;
        self.voices.process(self.font.wave_data(), &self.channels);

        self.block_left.fill(0_f32);
        self.block_right.fill(0_f32);
        for voice in self.voices.iter() {
            let previous_gain_left = MASTER_VOLUME * voice.previous_mix_gain_left;
            let current_gain_left = MASTER_VOLUME * voice.current_mix_gain_left;
            write_block(
                previous_gain_left,
                current_gain_left,
                voice.block(),
                &mut self.block_left[..],
                inverse_block_size,
            );
            let previous_gain_right = MASTER_VOLUME * voice.previous_mix_gain_right;
            let current_gain_right = MASTER_VOLUME * voice.current_mix_gain_right;
            write_block(
                previous_gain_right,
                current_gain_right,
                voice.block(),
                &mut self.block_right[..],
                inverse_block_size,
            );
        }

        if let Some(effects) = self.effects.as_mut() {
            let chorus = &mut effects.chorus;
            let chorus_input_left = &mut effects.chorus_input_left[..];
            let chorus_input_right = &mut effects.chorus_input_right[..];
            let chorus_output_left = &mut effects.chorus_output_left[..];
            let chorus_output_right = &mut effects.chorus_output_right[..];
            chorus_input_left.fill(0_f32);
            chorus_input_right.fill(0_f32);
            for voice in self.voices.iter() {
                let previous_gain_left = voice.previous_chorus_send * voice.previous_mix_gain_left;
                let current_gain_left = voice.current_chorus_send * voice.current_mix_gain_left;
                write_block(
                    previous_gain_left,
                    current_gain_left,
                    voice.block(),
                    chorus_input_left,
                    inverse_block_size,
                );
                let previous_gain_right =
                    voice.previous_chorus_send * voice.previous_mix_gain_right;
                let current_gain_right = voice.current_chorus_send * voice.current_mix_gain_right;
                write_block(
                    previous_gain_right,
                    current_gain_right,
                    voice.block(),
                    chorus_input_right,
                    inverse_block_size,
                );
            }
            chorus.process(
                chorus_input_left,
                chorus_input_right,
                chorus_output_left,
                chorus_output_right,
            );
            ArrayMath::multiply_add(MASTER_VOLUME, chorus_output_left, &mut self.block_left[..]);
            ArrayMath::multiply_add(
                MASTER_VOLUME,
                chorus_output_right,
                &mut self.block_right[..],
            );

            let reverb = &mut effects.reverb;
            let reverb_input = &mut effects.reverb_input[..];
            let reverb_output_left = &mut effects.reverb_output_left[..];
            let reverb_output_right = &mut effects.reverb_output_right[..];
            reverb_input.fill(0_f32);
            for voice in self.voices.iter() {
                let previous_gain = reverb.input_gain()
                    * voice.previous_reverb_send
                    * (voice.previous_mix_gain_left + voice.previous_mix_gain_right);
                let current_gain = reverb.input_gain()
                    * voice.current_reverb_send
                    * (voice.current_mix_gain_left + voice.current_mix_gain_right);
                write_block(
                    previous_gain,
                    current_gain,
                    voice.block(),
                    &mut reverb_input[..],
                    inverse_block_size,
                );
            }

            reverb.process(reverb_input, reverb_output_left, reverb_output_right);
            ArrayMath::multiply_add(MASTER_VOLUME, reverb_output_left, &mut self.block_left[..]);
            ArrayMath::multiply_add(
                MASTER_VOLUME,
                reverb_output_right,
                &mut self.block_right[..],
            );
        }
    }

    /// The rate the player renders at, in whole hertz.
    pub(crate) fn sample_rate(&self) -> SampleRate {
        SampleRate(f64::from(self.render.hz()))
    }

    /// A copy of this player that renders at `sample_rate`: the same
    /// SoundFont (shared, not reloaded), polyphony, effects switch and
    /// per-channel state (bank, patch, controllers, pitch bend), with no voice
    /// sounding and fresh effect lines. Allocates.
    ///
    /// # Errors
    ///
    /// [`Error::SampleRateOutOfRange`] if `sample_rate` is outside 16 to
    /// 192 kHz.
    pub(crate) fn with_sample_rate(&self, sample_rate: SampleRate) -> Result<Self> {
        let mut player = Self::new(&self.font, &self.settings.with_sample_rate(sample_rate))?;
        player.channels = self.channels.clone();
        Ok(player)
    }

    /// The longest the player can keep sounding once its last key is up:
    /// the longest release of any voice a preset can start, then the effects
    /// ringing out. `None` if a release cannot be bounded (it is not finite).
    pub(crate) fn ring_out(&self) -> Option<f64> {
        let mut longest = 0_f64;
        for preset in self.font.presets() {
            for preset_zone in preset.regions() {
                let instrument = &self.font.instruments()[preset_zone.instrument_id()];
                for instrument_zone in instrument.regions() {
                    let pair = crate::region_pair::RegionPair::new(preset_zone, instrument_zone);
                    longest = longest.max(f64::from(pair.release_time()));
                }
            }
        }
        if !longest.is_finite() {
            return None;
        }
        let effects = self
            .effects
            .as_ref()
            .map_or(0.0, |e| f64::from(e.reverb.ring_out(self.render.hz())));
        Some(longest + effects)
    }
}

fn write_block(
    previous_gain: f32,
    current_gain: f32,
    source: &[f32],
    destination: &mut [f32],
    inverse_block_size: f32,
) {
    if SoundFontMath::max(previous_gain, current_gain) < SoundFontMath::NON_AUDIBLE {
        return;
    }

    if (current_gain - previous_gain).abs() < 1.0E-3_f32 {
        ArrayMath::multiply_add(current_gain, source, destination);
    } else {
        let step = inverse_block_size * (current_gain - previous_gain);
        ArrayMath::multiply_add_slope(previous_gain, step, source, destination);
    }
}

#[derive(Debug, Clone)]
struct Effects {
    reverb: Reverb,
    reverb_input: Vec<f32>,
    reverb_output_left: Vec<f32>,
    reverb_output_right: Vec<f32>,

    chorus: Chorus,
    chorus_input_left: Vec<f32>,
    chorus_input_right: Vec<f32>,
    chorus_output_left: Vec<f32>,
    chorus_output_right: Vec<f32>,
}

impl Effects {
    fn new(render: &Render) -> Effects {
        Self {
            reverb: Reverb::new(render.hz()),
            reverb_input: vec![0_f32; render.block_len()],
            reverb_output_left: vec![0_f32; render.block_len()],
            reverb_output_right: vec![0_f32; render.block_len()],
            chorus: Chorus::new(render.hz(), 0.002, 0.0019, 0.4),
            chorus_input_left: vec![0_f32; render.block_len()],
            chorus_input_right: vec![0_f32; render.block_len()],
            chorus_output_left: vec![0_f32; render.block_len()],
            chorus_output_right: vec![0_f32; render.block_len()],
        }
    }
}
